/* Task work inside the conversation. Each task the user takes part in appears
   once, at the time of its latest step: incoming work to accept, the Agent's
   private draft to review, supplement or deliver, and a delivery to accept or
   return. The right-hand pane stays an overview. Nothing here changes who can
   see what: drafts are fetched from the owner's own receipts and shown only to
   the owner; delivery and acceptance remain explicit human actions. */
const draftReceipts = new Map();   // owner's draft request_id -> receipt
const draftReads = new Set();      // request_ids being fetched
const deliveryEditors = new Map(); // task id -> { text, source, revision } kept across polls
const pendingActions = new Map();  // task id -> { action, revision } read from the user's words
const deferredRevisions = new Map(); // instructions retained in memory until a base draft is available
const dismissedFocus = new Set();  // "id:revision" the user detached from the composer
const ACTIONS_KEY = 'agenct.task-actions.v1';
let actionsOwner = null;           // whose saved steps are loaded in this tab

// A waiting step survives a reload of this tab (never credentials or content).
function saveActions() {
  try { sessionStorage.setItem(ACTIONS_KEY, JSON.stringify({ user: actionsOwner, items: [...pendingActions] })); } catch {}
}
function loadActions(context) {
  if (actionsOwner === context.user) return;
  pendingActions.clear(); actionsOwner = context.user;
  try {
    const saved = JSON.parse(sessionStorage.getItem(ACTIONS_KEY));
    if (saved?.user === context.user && Array.isArray(saved.items)) {
      for (const [id, step] of saved.items) {
        if (typeof id === 'string' && ['accept', 'deliver', 'approve', 'return'].includes(step?.action)
            && Number.isSafeInteger(step.revision) && (step.action !== 'deliver' || step.draftId === null || typeof step.draftId === 'string')) {
          pendingActions.set(id, { action: step.action, revision: step.revision, draftId: step.draftId ?? null });
        }
      }
    }
  } catch {}
}
function rememberAction(id, step) { pendingActions.set(id, step); saveActions(); }
function forgetAction(id) { if (pendingActions.delete(id)) saveActions(); }

function taskChatVersion() {
  // Re-render when a fetched draft or an open editor changes, not only on API data.
  return JSON.stringify([[...draftReceipts.keys()], [...deliveryEditors.keys()], [...pendingActions], [...deferredRevisions]]);
}

function resetTaskChat({ preserveRecovery = false } = {}) {
  draftReceipts.clear(); draftReads.clear(); deliveryEditors.clear(); pendingActions.clear(); deferredRevisions.clear(); dismissedFocus.clear();
  actionsOwner = null;
  if (!preserveRecovery) { try { sessionStorage.removeItem(ACTIONS_KEY); } catch {} }
}

function finishDeferred(taskId, saved) {
  if (deferredRevisions.get(taskId) !== saved) return;
  const [next, ...rest] = saved.queued || [];
  if (next) deferredRevisions.set(taskId, { ...next, queued: rest });
  else deferredRevisions.delete(taskId);
}

function latestStep(activity, id) {
  const item = Array.isArray(activity) ? activity.find(entry => entry.id === id && entry.kind === 'task') : null;
  return item?.latest || null;
}

function readDraft(context, task, requestId) {
  if (draftReceipts.has(requestId) || draftReads.has(requestId)) return;
  draftReads.add(requestId);
  api(context, 'tasks/' + encodeURIComponent(task.id) + '/drafts/turns/' + encodeURIComponent(requestId))
    .then(receipt => {
      if (!current(context)) return;
      if (receipt?.status === 'complete' && typeof receipt.response?.draft?.content === 'string') {
        draftReceipts.set(requestId, receipt);
        return refresh(context);
      }
    })
    .catch(() => {})
    .finally(() => draftReads.delete(requestId));
}

function taskLine(text) {
  const line = el('div', 'task-line');
  line.append(el('span', 'task-line-text', text));
  return line;
}

function taskCard(task, kicker) {
  const card = el('article', 'card task-chat task-' + task.status);
  card.taskId = task.id;
  card.id = 'task-chat-' + task.id;
  const head = el('header', 'card-head');
  head.append(el('span', 'card-kicker', kicker), badge(TASK_STATUS, task.status));
  card.append(head, el('h3', 'task-title', task.summary));
  if (task.details) {
    const material = el('details', 'material');
    material.append(el('summary', null, '共享材料'), el('pre', null, task.details));
    card.append(material);
  }
  return card;
}

async function startDraft(context, task, revision, requestId) {
  const key = 'draft:' + task.id;
  if (!current(context) || busy.has(key) || !requestId) return;
  const existing = deferredRevisions.get(task.id);
  // A first draft has no base. Keep an unsent revision behind that request so
  // generating a base never discards or silently submits the user's instruction.
  const queued = existing && !revision ? [existing, ...(existing.queued || [])] : existing?.queued || [];
  const saved = { instruction: revision?.instruction, requestId, revision: task.revision,
    baseDraftId: revision?.base_draft_id, sent: true, queued };
  deferredRevisions.set(task.id, saved);
  busy.add(key);
  status(revision ? '已发送补充要求，Agent 正在生成新版本…' : 'Agent 正在起草…');
  try {
    const receipt = await api(context, 'tasks/' + encodeURIComponent(task.id) + '/drafts', 'POST', {
      request_id: requestId, expected_revision: task.revision, ...(revision ? { revision } : {})
    });
    if (!current(context)) return;
    if (receipt?.status === 'complete') {
      draftReceipts.set(receipt.request_id, receipt);
      finishDeferred(task.id, saved);
      status(revision ? '新版本草稿已生成，仅你可见。' : '基础草稿已生成，仅你可见。请核对后应用保留的修改要求。');
    }
    else status('起草结果尚未确认；不会自动重发。');
  } catch (error) {
    if (current(context)) {
      const notReserved = error.httpStatus === 409 && ['draft_task_stale_before_reservation',
        'draft_task_state_before_reservation', 'draft_role_unavailable_before_reservation',
        'draft_busy_before_reservation'].includes(error.code);
      if (!revision && notReserved) finishDeferred(task.id, saved);
      status(notReserved ? '请求未登记、未调用模型。请核对任务后再手动起草。' : '起草未完成：' + error.message);
    }
  } finally {
    if (current(context)) { busy.delete(key); await refresh(context).catch(error => status(error.message)); }
  }
}

function deliveryEditor(context, task, card, draft) {
  const editor = deliveryEditors.get(task.id);
  if (!editor) return;
  if (editor.revision !== task.revision) { deliveryEditors.delete(task.id); return; }
  const panel = el('section', 'action-editor');
  const label = el('label', null, `交付给 ${task.requester} 的内容（确认后对方可见）`);
  const input = el('textarea'); input.rows = 8; input.value = editor.text;
  input.setAttribute('aria-label', '交付内容');
  input.oninput = () => { editor.text = input.value; };
  label.append(input);
  const note = el('p', 'note', editor.source ? '基于 Agent 草稿，可直接修改。' : '手写交付。');
  const actions = el('div', 'actions');
  const submit = el('button', 'primary', '确认交付'), cancel = el('button', null, '先不交付');
  submit.type = cancel.type = 'button';
  const key = 'tasks:' + task.id;
  submit.disabled = busy.has(key);
  cancel.onclick = () => { if (!busy.has(key)) { deliveryEditors.delete(task.id); return refresh(context).catch(() => {}); } };
  submit.onclick = async () => {
    const text = input.value.trim();
    if (!current(context) || busy.has(key)) return;
    if (!text || new TextEncoder().encode(text).length > 8000) { note.textContent = '交付内容需为 1–8000 字节。'; return; }
    busy.add(key); submit.disabled = true;
    try {
      await api(context, 'tasks/' + encodeURIComponent(task.id) + '/actions', 'POST', {
        action: 'deliver', expected_revision: task.revision, result: input.value,
        ...(editor.source ? { source_draft_id: editor.source } : {})
      });
      if (!current(context)) return;
      deliveryEditors.delete(task.id); forgetAction(task.id);
      status(`已交付给 ${task.requester}。`);
    } catch (error) {
      if (current(context)) note.textContent = error.message + '。内容已保留，刷新核对后再试。';
    } finally {
      if (current(context)) { busy.delete(key); await refresh(context).catch(error => status(error.message)); }
    }
  };
  actions.append(submit, cancel);
  panel.append(label, note, actions);
  card.append(panel);
}

function latestDraft(task) {
  const mine = task.my_draft;
  return mine?.status === 'complete' ? draftReceipts.get(mine.request_id)?.response?.draft || null : null;
}

const ACTION_TEXT = {
  accept: task => ['确认接手', '接手后 Agent 会自动起草，只有你可见。'],
  deliver: task => ['确认交付', `将把上面这版草稿交付给 ${task.requester}，对方随即可见。`],
  approve: task => ['确认验收', `确认 ${task.owner} 的交付已完成。`],
  return: task => ['确认退回', `退回给 ${task.owner} 修改，可以在聊天里说明原因。`]
};

/** Carry out the confirmed step. Deliver without a draft opens the editor instead. */
async function runPendingAction(context, task, pending) {
  const key = 'tasks:' + task.id;
  if (!pending || pendingActions.get(task.id) !== pending || pending.revision !== task.revision
      || visibleTasks.get(task.id)?.revision !== task.revision || !current(context) || busy.has(key)) return;
  const draft = latestDraft(task);
  if (pending.action === 'deliver' && (draft?.id || null) !== pending.draftId) {
    status('草稿版本已变更，请重新核对后请求交付。'); return;
  }
  if (pending.action === 'deliver' && !draft) {
    deliveryEditors.set(task.id, { text: '', source: null, revision: task.revision });
    forgetAction(task.id);
    return refresh(context).catch(() => {});
  }
  busy.add(key);
  try {
    await api(context, 'tasks/' + encodeURIComponent(task.id) + '/actions', 'POST', {
      action: pending.action, expected_revision: task.revision,
      ...(pending.action === 'deliver' ? { result: draft.content, source_draft_id: draft.id } : {})
    });
    if (!current(context)) return;
    forgetAction(task.id);
    // This explicit action consumed the selected snapshot. Only advance an empty
    // composer; text typed while the request was in flight keeps its old fence.
    if (selectedTask?.id === task.id && selectedTask.revision === task.revision && !$('text').value.trim()) {
      selectedTask = null;
    }
    status({ accept: '已接手，Agent 正在起草。', deliver: `已交付给 ${task.requester}。`, approve: '已验收。', return: `已退回给 ${task.owner}。` }[pending.action]);
  } catch (error) {
    if (current(context)) status(error.message + '。请刷新核对任务状态。');
  } finally {
    if (current(context)) { busy.delete(key); await refresh(context).catch(error => status(error.message)); }
  }
}

// Waiting cards survive unrelated chat and reloads, so ordinary agreement can
// never authorize them. The exact named action still needs the selected task,
// task revision and (for delivery) reviewed draft checks below.
const CONFIRM_ACTION = /^(?:确认|确定)(接手|交付|验收|退回)[。.!！~～]*$/;
const NO = /^(算了|取消|不了|先不|不要|不用了?|cancel|no|n)[。.!！~～]*$/i;

/** Only an explicit named confirmation or a cancellation answers a waiting card
    without another model turn. Generic agreement remains ordinary chat. */
async function replyToPendingAction(context, text) {
  if (!selectedTask || selectedTask.invalidated || pendingTurn || busy.has('turn-check')) return false;
  const task = visibleTasks.get(selectedTask.id), pending = pendingActions.get(selectedTask.id);
  const named = CONFIRM_ACTION.exec(text)?.[1];
  if (!task || !pending || task.revision !== selectedTask.revision || pending.revision !== task.revision
      || !(named || NO.test(text))) return false;
  if (named && named !== { accept: '接手', deliver: '交付', approve: '验收', return: '退回' }[pending.action]) {
    status('确认的动作与当前待确认操作不同，请核对任务卡片。'); return true;
  }
  const id = task.id;
  $('text').value = ''; $('text').oninput?.();
  if (NO.test(text)) { forgetAction(id); status('好的，没有执行。'); await refresh(context).catch(() => {}); return true; }
  await runPendingAction(context, task, pending);
  return true;
}

/** The one confirmation a plain-language request leads to. */
function confirmation(context, task, card) {
  const pending = pendingActions.get(task.id);
  if (!pending) return;
  if (pending.revision !== task.revision) { forgetAction(task.id); return; }
  const [label, note] = ACTION_TEXT[pending.action](task);
  // The prompt names the exact action required for a typed confirmation.
  const draft = latestDraft(task);
  const box = el('div', 'task-confirm');
  box.append(el('p', 'note', note + ` 可以直接回复「${label}」或「算了」。`));
  const actions = el('div', 'actions');
  const key = 'tasks:' + task.id;
  if (pending.action === 'deliver' && !draft) {
    // Nothing drafted yet: the owner writes the delivery themselves.
    const write = el('button', 'primary', '填写交付内容'); write.type = 'button';
    write.onclick = () => {
      if (!current(context)) return;
      deliveryEditors.set(task.id, { text: '', source: null, revision: task.revision });
      forgetAction(task.id);
      return refresh(context).catch(() => {});
    };
    actions.append(write);
  }
  const confirm = el('button', 'primary', label); confirm.type = 'button';
  confirm.disabled = busy.has(key);
  confirm.onclick = () => runPendingAction(context, task, pending);
  if (pending.action !== 'deliver' || draft) actions.append(confirm);
  if (pending.action === 'deliver' && draft) {
    const edit = el('button', 'text-btn', '先改一下再交付'); edit.type = 'button';
    edit.onclick = () => {
      if (!current(context)) return;
      deliveryEditors.set(task.id, { text: draft?.content || '', source: draft?.id || null, revision: task.revision });
      forgetAction(task.id);
      return refresh(context).catch(() => {});
    };
    actions.append(edit);
  }
  const cancel = el('button', 'text-btn', '算了'); cancel.type = 'button';
  cancel.onclick = () => { forgetAction(task.id); return refresh(context).catch(() => {}); };
  actions.append(cancel);
  box.append(actions);
  card.append(box);
}

function ownerWorking(context, task, card) {
  const mine = task.my_draft;
  const drafting = busy.has('draft:' + task.id) || mine?.status === 'pending';
  if (mine?.status === 'complete' && !draftReceipts.has(mine.request_id)) readDraft(context, task, mine.request_id);
  const draft = latestDraft(task);
  if (drafting) card.append(el('p', 'draft-status working', 'Agent 正在起草，仅你可见…'));
  else if (mine?.status === 'complete' && !draft) card.append(el('p', 'draft-status', '正在读取 Agent 草稿…'));
  else if (mine?.status === 'uncertain') card.append(el('p', 'note', '上次起草结果未知，不会自动重发。可以在聊天里让 Agent 重新起草。'));
  if (draft && !drafting) {
    const box = el('section', 'agent-draft');
    box.append(el('div', 'agent-draft-meta', `Agent 草稿 · 第 ${draft.seq ?? 1} 版 · 仅你可见，模型生成，未经验证`));
    if (draft.instruction) box.append(el('div', 'agent-draft-ask', '按你的补充：' + draft.instruction));
    box.append(renderMarkdown(draft.content));
    card.append(box);
  }
  if (!drafting && !pendingActions.has(task.id) && !deliveryEditors.has(task.id)) {
    card.append(el('p', 'task-hint', draft
      ? `直接在下面回复补充信息或修改意见；说「交付」就发给 ${task.requester}。`
      : '在下面告诉 Agent 怎么写，它会起草一版只有你可见的草稿。'));
  }
  const deferred = deferredRevisions.get(task.id);
  const restartInitial = deferred?.sent && !deferred.baseDraftId && deferred.retryAllowed;
  if (!drafting && !draft && mine?.status !== 'complete' && (!deferred?.sent || restartInitial)
      && (deferred || mine?.status === 'uncertain')
      && (!deferred || deferred.revision === task.revision)) {
    const initial = el('button', null, mine?.status === 'uncertain' || restartInitial ? '明确重新起草' : '先生成基础草稿');
    initial.type = 'button';
    let started = false;
    initial.onclick = () => {
      const live = visibleTasks.get(task.id);
      if (started || !current(context) || busy.has('draft:' + task.id) || live?.revision !== task.revision
          || live.status !== 'working' || live.owner !== context.user
          || deferredRevisions.get(task.id) !== deferred || live.my_draft?.status === 'pending'
          || live.my_draft?.status === 'complete' || (restartInitial && !deferred.retryAllowed)) return;
      if ((live.my_draft?.status === 'uncertain' || restartInitial)
          && !confirm('原请求可能已调用模型并产生费用。重新起草可能重复工作。确认开始一次新的手动起草？')) return;
      started = true; initial.disabled = true;
      // Abandon only this explicitly checked initial attempt, not its queued
      // unsent instructions. The new attempt always receives a new receipt key.
      if (restartInitial) finishDeferred(task.id, deferred);
      return startDraft(context, task, null, crypto.randomUUID());
    };
    card.append(initial, el('p', 'note', '基础草稿先使用本任务的共享材料；已保留的修改要求会在基础草稿完成后供你手动应用。'));
  }
  if (deferred) {
    const box = el('section', 'deferred-revision');
    box.append(el('p', 'note', deferred.sent ? '起草请求已保留，结果尚未确认。仅核对原请求，不会重发。'
      : '修改要求已保留，尚未发送。请等待基础草稿可用后重试。'));
    if (deferred.instruction) box.append(el('pre', null, deferred.instruction));
    const retry = el('button', null, deferred.sent ? (deferred.baseDraftId ? '核对这次修订' : '核对这次起草') : '用保留的要求修改草稿'); retry.type = 'button';
    retry.disabled = drafting || (!deferred.sent && (!draft || deferred.revision !== task.revision));
    retry.onclick = async () => {
      if (!current(context) || deferredRevisions.get(task.id) !== deferred
          || busy.has('draft:' + task.id) || retry.disabled) return;
      if (deferred.sent) {
        retry.disabled = true;
        try {
          const receipt = await api(context, 'tasks/' + encodeURIComponent(task.id) + '/drafts/turns/' + encodeURIComponent(deferred.requestId));
          if (!current(context) || deferredRevisions.get(task.id) !== deferred) return;
          if (receipt?.status === 'complete') {
            draftReceipts.set(deferred.requestId, receipt); finishDeferred(task.id, deferred);
          } else {
            deferred.retryAllowed = receipt?.status === 'uncertain';
            status('起草结果仍未确认，保留原请求；不会重发。');
          }
        } catch (error) {
          if (current(context) && deferredRevisions.get(task.id) === deferred) {
            deferred.retryAllowed = error.httpStatus === 404;
            status('未能核对起草：' + error.message);
          }
        }
        finally { if (current(context)) { retry.disabled = false; await refresh(context).catch(() => {}); } }
        return;
      }
      if (!draft || visibleTasks.get(task.id)?.revision !== deferred.revision) return;
      return startDraft(context, task, { base_draft_id: draft.id, instruction: deferred.instruction }, deferred.requestId);
    };
    box.append(retry);
    for (const queued of deferred.queued || []) {
      box.append(el('p', 'note', '后续修改要求已保留，等待前一次核对完成后手动发送。'), el('pre', null, queued.instruction));
    }
    card.append(box);
  }
  confirmation(context, task, card);
  deliveryEditor(context, task, card, draft);
}

function taskMoment(context, task) {
  const owner = task.owner === context.user;
  const other = owner ? task.requester : task.owner;
  switch (task.status) {
    case 'awaiting_owner': {
      if (!owner) return taskLine(`已交给 ${other}：「${task.summary}」，等待对方接手`);
      const card = taskCard(task, `${other} 委派给你`);
      if (!pendingActions.has(task.id)) card.append(el('p', 'task-hint', '回复「接手」即可开始，Agent 会自动起草；也可以先问清楚再决定。'));
      confirmation(context, task, card);
      return card;
    }
    case 'working': {
      if (!owner) return taskLine(`${other} 已接手「${task.summary}」，处理中`);
      const card = taskCard(task, `${other} 委派给你`);
      ownerWorking(context, task, card);
      return card;
    }
    case 'ready_for_acceptance': {
      if (owner) return taskLine(`已把「${task.summary}」交付给 ${other}，等待验收`);
      const card = taskCard(task, `${other} 交付了结果`);
      const result = el('section', 'agent-draft');
      result.append(el('div', 'agent-draft-meta', '交付内容'), renderMarkdown(task.result || ''));
      card.append(result);
      if (!pendingActions.has(task.id)) card.append(el('p', 'task-hint', '回复「验收」，或直接说哪里需要修改。'));
      confirmation(context, task, card);
      return card;
    }
    case 'completed':
      return taskLine(owner ? `${other} 已验收「${task.summary}」` : `「${task.summary}」已验收完成`);
    case 'canceled':
      return taskLine(`「${task.summary}」已取消`);
    default:
      return null;
  }
}

/** One conversation entry per task, with the time of its latest visible step. */
function taskMoments(context, tasks, activity) {
  loadActions(context);
  const moments = [];
  for (const task of tasks) {
    if (task.owner !== context.user && task.requester !== context.user) continue;
    const element = taskMoment(context, task);
    if (!element) continue;
    // A card waiting for an answer sits at the end, right under the reply that asked.
    const asking = pendingActions.has(task.id) || deliveryEditors.has(task.id);
    moments.push({ at: asking ? null : parseUtc(latestStep(activity, task.id)?.at), element, task });
  }
  return moments;
}

/** Act on the task step the Agent read from the user's message. */
function handleTaskAction(context, action, requestId) {
  if (!action || typeof action.task_id !== 'string') return;
  const task = visibleTasks.get(action.task_id);
  if (!task || task.revision !== action.expected_revision) return;
  if (action.action === 'revise' && typeof action.instruction === 'string') {
    // Private and reversible: the new version is just another owner-only draft.
    const draft = latestDraft(task);
    if (typeof requestId !== 'string' || !/^[a-f0-9-]{36}$/.test(requestId)) return;
    const existing = deferredRevisions.get(task.id);
    if (existing) {
      if (existing.requestId !== requestId && !(existing.queued || []).some(item => item.requestId === requestId)) {
        (existing.queued ||= []).push({ instruction: action.instruction, requestId, revision: task.revision });
      }
      return; // Never overwrite or redispatch an unresolved instruction.
    }
    if (draft?.id) startDraft(context, task, { base_draft_id: draft.id, instruction: action.instruction }, requestId);
    else deferredRevisions.set(task.id, { instruction: action.instruction, requestId, revision: task.revision });
  } else if (ACTION_TEXT[action.action]) {
    rememberAction(task.id, { action: action.action, revision: task.revision, draftId: latestDraft(task)?.id || null });
  }
}

const needsMe = (context, task) => (task.owner === context.user && ['awaiting_owner', 'working'].includes(task.status))
  || (task.requester === context.user && task.status === 'ready_for_acceptance');

/** Keep the composer on the task that is waiting for this user, newest step first,
    so plain replies reach it. An explicit choice or a dismissal (×) wins. */
function focusTask(context, tasks, activity) {
  // An unresolved send keeps its own locked context until it is checked.
  if (pendingTurn || busy.has('turn-check')) return;
  // Polling never reinterprets text already being composed, and an explicit
  // choice stays fixed (including its invalidation). Only an automatic binding
  // with an empty composer follows the task to its current version.
  if ($('text').value.trim()) return;
  if (selectedTask && !selectedTask.auto) return;
  const time = task => parseUtc(latestStep(activity, task.id)?.at)?.getTime() ?? 0;
  const next = tasks.filter(task => needsMe(context, task) && !dismissedFocus.has(task.id + ':' + task.revision))
    .sort((a, b) => time(b) - time(a))[0];
  const visible = next && visibleTasks.get(next.id);
  if (!visible) { if (selectedTask?.auto) { selectedTask = null; renderTaskContext(); } return; }
  if (selectedTask?.auto && selectedTask.id === visible.id && !selectedTask.invalidated
      && taskSignature(selectedTask) === taskSignature(visible)) return;
  selectedTask = { ...visible, invalidated: false, auto: true };
  renderTaskContext();
}

function dismissFocus() {
  if (selectedTask?.auto) dismissedFocus.add(selectedTask.id + ':' + selectedTask.revision);
}

/** Hand a reviewed draft (from the version-history panel) to the conversation's delivery box. */
function useDraftInChat(context, taskId, draftId, text) {
  const task = visibleTasks.get(taskId);
  if (!current(context) || !task || task.status !== 'working') return;
  deliveryEditors.set(taskId, { text, source: draftId, revision: task.revision });
  refresh(context).then(() => showTaskInChat(taskId)).catch(error => status(error.message));
}

/** Bring a task's conversation card into view (closing the drawer on phones). */
function showTaskInChat(id) {
  const card = document.getElementById?.('task-chat-' + id);
  returnToChatOnNarrowScreen();
  if (!card) return;
  card.scrollIntoView?.({ block: 'center', behavior: 'smooth' });
  card.classList.add('flash');
  setTimeout(() => card.classList.remove('flash'), 1600);
}

