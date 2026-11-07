/* Task work inside the conversation. Each task the user takes part in appears
   once, at the time of its latest step: incoming work to accept, the Agent's
   private draft to review, supplement or deliver, and a delivery to accept or
   return. The right-hand pane stays an overview. Nothing here changes who can
   see what: drafts are fetched from the owner's own receipts and shown only to
   the owner; delivery and acceptance remain explicit human actions. */
const draftReceipts = new Map();   // owner's draft request_id -> receipt
const draftReads = new Set();      // request_ids being fetched
const deliveryEditors = new Map(); // task id -> { text, source, revision } kept across polls
let draftReply = null;             // composer bound to one task's latest draft

function taskChatVersion() {
  // Re-render when a fetched draft or an open editor changes, not only on API data.
  return JSON.stringify([[...draftReceipts.keys()], [...deliveryEditors.keys()]]);
}

function resetTaskChat() {
  draftReceipts.clear(); draftReads.clear(); deliveryEditors.clear(); draftReply = null;
  renderDraftReply();
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

async function startDraft(context, task, revision) {
  const key = 'draft:' + task.id;
  if (!current(context) || busy.has(key)) return;
  busy.add(key);
  status(revision ? '已发送补充要求，Agent 正在生成新版本…' : 'Agent 正在起草…');
  try {
    const receipt = await api(context, 'tasks/' + encodeURIComponent(task.id) + '/drafts', 'POST', {
      request_id: crypto.randomUUID(), expected_revision: task.revision, ...(revision ? { revision } : {})
    });
    if (!current(context)) return;
    if (receipt?.status === 'complete') { draftReceipts.set(receipt.request_id, receipt); status('新版本草稿已生成，仅你可见。'); }
    else status('起草结果尚未确认；不会自动重发。');
  } catch (error) {
    if (current(context)) status('起草未完成：' + error.message);
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
      deliveryEditors.delete(task.id);
      if (draftReply?.taskId === task.id) { draftReply = null; renderDraftReply(); }
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

function ownerWorking(context, task, card) {
  const mine = task.my_draft;
  const drafting = busy.has('draft:' + task.id) || mine?.status === 'pending';
  const receipt = mine?.status === 'complete' ? draftReceipts.get(mine.request_id) : null;
  if (mine?.status === 'complete' && !receipt) readDraft(context, task, mine.request_id);
  const draft = receipt?.response?.draft;
  if (drafting) card.append(el('p', 'draft-status working', 'Agent 正在起草，仅你可见…'));
  else if (mine?.status === 'complete' && !draft) card.append(el('p', 'draft-status', '正在读取 Agent 草稿…'));
  else if (mine?.status === 'uncertain') card.append(el('p', 'note', '上次起草结果未知，不会自动重发。'));
  if (draft && !drafting) {
    const box = el('section', 'agent-draft');
    const meta = `Agent 草稿 · 第 ${draft.seq ?? 1} 版 · 仅你可见，模型生成，未经验证`;
    box.append(el('div', 'agent-draft-meta', meta));
    if (draft.instruction) box.append(el('div', 'agent-draft-ask', '按你的补充：' + draft.instruction));
    box.append(renderMarkdown(draft.content));
    card.append(box);
  }
  const actions = el('div', 'actions');
  if (draft && !drafting) {
    const deliver = el('button', 'primary', `交付给 ${task.requester}`); deliver.type = 'button';
    deliver.onclick = () => {
      if (!current(context)) return;
      deliveryEditors.set(task.id, { text: draft.content, source: draft.id, revision: task.revision });
      return refresh(context).catch(() => {});
    };
    const reply = el('button', null, '补充信息 / 让 Agent 修改'); reply.type = 'button';
    reply.onclick = () => {
      if (!current(context)) return;
      draftReply = { taskId: task.id, revision: task.revision, baseDraftId: draft.id, summary: task.summary };
      renderDraftReply();
      returnToChatOnNarrowScreen();
      $('text').focus?.();
    };
    if (!deliveryEditors.has(task.id)) actions.append(deliver);
    actions.append(reply);
  } else if (!drafting && mine?.status !== 'complete') {
    const start = el('button', 'primary', '让 Agent 起草'); start.type = 'button';
    start.onclick = () => startDraft(context, task);
    actions.append(start);
  }
  if (!drafting && !deliveryEditors.has(task.id)) {
    const manual = el('button', 'text-btn', '手写交付'); manual.type = 'button';
    manual.onclick = () => {
      if (!current(context)) return;
      deliveryEditors.set(task.id, { text: '', source: null, revision: task.revision });
      return refresh(context).catch(() => {});
    };
    actions.append(manual);
  }
  if (actions.children.length) card.append(actions);
  deliveryEditor(context, task, card, draft);
}

function taskMoment(context, task) {
  const owner = task.owner === context.user;
  const other = owner ? task.requester : task.owner;
  switch (task.status) {
    case 'awaiting_owner': {
      if (!owner) return taskLine(`已交给 ${other}：「${task.summary}」，等待对方接手`);
      const card = taskCard(task, `${other} 委派给你`);
      card.append(el('p', 'note', '接手后，你的 Agent 会自动起草，草稿只有你可见；交付前由你审阅。'));
      const actions = el('div', 'actions');
      addActions(actions, context, 'tasks', task, [['accept', '接手']]);
      card.append(actions);
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
      const actions = el('div', 'actions');
      addActions(actions, context, 'tasks', task, [['approve', '验收通过'], ['return', '退回修改']]);
      card.append(actions);
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
  const moments = [];
  for (const task of tasks) {
    if (task.owner !== context.user && task.requester !== context.user) continue;
    const element = taskMoment(context, task);
    if (!element) continue;
    moments.push({ at: parseUtc(latestStep(activity, task.id)?.at), element, task });
  }
  return moments;
}

function renderDraftReply() {
  const bar = $('draft-reply');
  if (!bar) return;
  bar.classList[draftReply ? 'remove' : 'add']('hide');
  $('draft-reply-title').textContent = draftReply ? draftReply.summary : '';
  $('text').placeholder = draftReply ? '写给 Agent 的补充信息或修改要求，发送后生成新版本草稿…' : '说说你想解决的问题…';
}

/** Returns true when the composer text was used as a draft revision request. */
function sendDraftReply(context, text) {
  if (!draftReply) return false;
  const target = draftReply, task = visibleTasks.get(target.taskId);
  if (!task || task.status !== 'working' || task.revision !== target.revision) {
    draftReply = null; renderDraftReply();
    status('任务已变更，补充没有发送。请在对话中的任务卡片上重新选择。');
    return true;
  }
  if (new TextEncoder().encode(text).length > 4000) { status('补充内容最多 4000 字节。'); return true; }
  draftReply = null; renderDraftReply();
  $('text').value = '';
  startDraft(context, task, { base_draft_id: target.baseDraftId, instruction: text });
  return true;
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

$('clear-draft-reply').onclick = () => { draftReply = null; renderDraftReply(); };
