let token = '', user = '', epoch = 0, refreshVersion = 0;
let browserMode = null, csrfToken = '', logoutPending = false;
let controller = new AbortController(), pendingSend = null, pendingTurn = null;
let turnStorageAvailable = true;
let selectedAgent = null, roleGeneration = 0, roleRead = null, roleButtons = [];
let selectedTask = null, visibleTasks = new Map(), discussionButtons = [], taskView = 0;
const TURN_STORAGE_KEY = 'agenct.pending-chat-turn.v1';
const actionEditors = new Map();
let notificationTasks = null, notificationsEnabled = false;
const detailDrafts = new Map(); // Unsent shared-material text survives poll re-renders.
let lastActivity = [];
let pollTimer = null, pollDelay = 3000, refreshFlight = null, snapshot = '', renderedBusy = '';
const POLL_INTERVAL = 3000, MAX_POLL_DELAY = 60000, READ_TIMEOUT = 15000, CHAT_SEND_TIMEOUT = 90000;
const busy = new Set();
const $ = id => document.getElementById(id);
// Critical feedback and reply fallbacks stay readable until dismissed.
// Only explicitly marked routine feedback expires; no text is persisted.
const status = (text, { transient = false } = {}) => {
  const toast = $('status');
  toast.textContent = text;
  toast.classList.remove('show');
  void toast.offsetWidth;
  if (!text || !transient) return;
  toast.style?.setProperty('--life', Math.min(9, 3 + text.length / 25).toFixed(1) + 's');
  toast.classList.add('show');
};
const session = () => ({ epoch, token, user, browserMode, csrfToken, signal: controller.signal });
const current = context => context.epoch === epoch && context.browserMode === browserMode
  && (browserMode === true
    ? !!csrfToken && context.csrfToken === csrfToken && context.user === user
    : browserMode === false && !!token && context.token === token);

async function api(context, path, method = 'GET', body) {
  if (!current(context)) throw Error('身份已变更，请重新操作');
  const headers = { 'Content-Type': 'application/json' };
  if (context.browserMode) {
    // Bind reads too: another tab may have replaced the shared session cookie.
    headers['X-CSRF-Token'] = context.csrfToken;
  } else {
    headers.Authorization = 'Bearer ' + context.token;
  }
  const response = await fetch('/api/' + path, {
    method,
    signal: context.signal,
    credentials: 'same-origin',
    headers,
    body: body ? JSON.stringify(body) : undefined
  });
  const data = await response.json();
  if (!current(context)) throw Error('身份已变更，请重新操作');
  if (!response.ok) {
    const error = Error(data.error || response.status);
    error.httpStatus = response.status;
    error.code = data.code;
    throw error;
  }
  return data;
}

async function postChat(context, body) {
  const request = new AbortController();
  let timedOut = false;
  const abort = () => request.abort();
  context.signal.addEventListener('abort', abort, { once: true });
  // Longer than the Hub adapter timeout; aborting only ends this browser wait.
  const timeout = setTimeout(() => { timedOut = true; abort(); }, CHAT_SEND_TIMEOUT);
  try {
    return await api({ ...context, signal: request.signal }, 'messages', 'POST', body);
  } catch (error) {
    if (timedOut) throw Error('发送等待超时');
    throw error;
  } finally {
    clearTimeout(timeout);
    context.signal.removeEventListener('abort', abort);
    request.abort();
  }
}

function article(text) {
  const element = document.createElement('article');
  element.textContent = text;
  return element;
}

function el(tag, className, text) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}
const TASK_STATUS = {
  awaiting_owner: ['待接手', 'wait'], working: ['进行中', 'work'], ready_for_acceptance: ['待验收', 'review'],
  completed: ['已完成', 'done'], canceled: ['已取消', 'muted']
};
const PROPOSAL_STATUS = { pending: ['待你确认', 'review'], confirmed: ['已确认', 'done'], rejected: ['已拒绝', 'muted'] };
const MESSAGE_STATUS = { pending: '发送中…', failed: '未发送成功', interrupted: '已中断', uncertain: '未确认' };
const ACTION_STYLE = { confirm: 'primary', accept: 'primary', deliver: 'primary', approve: 'primary', cancel: 'danger' };
function badge(table, value) {
  const [label, tone] = table[value] || [value, 'muted'];
  return el('span', 'badge badge-' + tone, label);
}
// Panels open inside the task card they were launched from (see render).
let panelOwners = new WeakMap(), panelSlots = new Map();
const taskPane = taskId => ({ append(panel) {
  panelOwners.set(panel, taskId);
  (panelSlots.get(taskId) || $('tasks')).append(panel);
} });

const timelineUI = createTaskTimelineUI({ api, current, article, conversation: taskPane });

const draftUI = createDraftUI({ api, current, article, conversation: taskPane });

const profileUI = createProfileUI({ api, current, article, conversation: () => $('conversation'), status,
  onSaved: revision => {
    if (selectedAgent && selectedAgent.revision !== revision) selectedAgent.invalidated = true;
    closeRolePicker();
    renderRoleSelection();
  }
});

const validAgentBinding = agent => agent && typeof agent.id === 'string'
  && /^[A-Za-z0-9_-]{1,32}$/.test(agent.id)
  && Number.isSafeInteger(agent.revision) && agent.revision >= 0;
const roleLocked = () => !!pendingTurn || busy.has('send') || busy.has('turn-check');
function closeRolePicker() {
  roleGeneration++;
  if (roleRead) roleRead.abort();
  roleRead = null;
  roleButtons = [];
  $('role-options').replaceChildren();
  $('role-picker').classList.add('hide');
}
function renderRoleSelection() {
  const agent = pendingTurn?.selectedAgent || selectedAgent;
  $('choose-role').disabled = roleLocked();
  $('clear-role').disabled = roleLocked() || !selectedAgent;
  roleButtons.forEach(button => { button.disabled = roleLocked(); });
  $('role-details').textContent = agent
    ? `本次角色：${agent.name || agent.id} · ${agent.role || '以原始回执为准'} · 角色版本 ${agent.revision}`
      + (pendingTurn ? '\n发送角色已锁定；核对结果前不能切换。'
        : agent.invalidated ? '\n所选配置已变更，选择已失效。请重新选择或明确恢复默认管家。'
          : '\n仅用于下一条普通聊天。')
    : pendingTurn ? '发送角色待原始回执核对，不能重新选择或自动重发。'
      : '下一条普通聊天默认使用你的管家。';
}
$('choose-role').onclick = async () => {
  const context = session();
  if (!current(context) || roleLocked()) return;
  closeRolePicker();
  const generation = roleGeneration;
  const reads = new AbortController();
  roleRead = reads;
  const abort = () => reads.abort();
  context.signal.addEventListener('abort', abort, { once: true });
  const timeout = setTimeout(abort, READ_TIMEOUT);
  const valid = () => current(context) && generation === roleGeneration && !roleLocked();
  $('role-picker').classList.remove('hide');
  $('role-options').replaceChildren(article('正在读取你的启用角色…'));
  try {
    const profile = await api({ ...context, signal: reads.signal }, 'profile');
    if (!valid()) return;
    if (reads.signal.aborted) throw Error('读取角色超时');
    if (!Number.isSafeInteger(profile.revision) || profile.revision < 0
        || !Array.isArray(profile.agents) || !profile.agents.length || profile.agents.length > 8
        || new Set(profile.agents.map(agent => agent.id)).size !== profile.agents.length
        || profile.agents.some(agent => !validAgentBinding({ ...agent, revision: profile.revision })
          || typeof agent.name !== 'string' || !agent.name || agent.name.length > 96
          || typeof agent.enabled !== 'boolean'
          || !['coordinator', 'researcher', 'writer', 'reviewer'].includes(agent.role)
          || agent.model_ref !== 'server-default')
        || profile.agents.filter(agent => agent.enabled && agent.role === 'coordinator').length !== 1) {
      throw Error('个人角色设置响应无效');
    }
    if (selectedAgent && selectedAgent.revision !== profile.revision) selectedAgent.invalidated = true;
    $('role-options').replaceChildren();
    for (const agent of profile.agents.filter(agent => agent.enabled)) {
      const button = document.createElement('button');
      button.type = 'button';
      button.textContent = `${agent.name} · ${agent.role} · ${agent.id}`;
      button.onclick = () => {
        if (!valid()) return;
        selectedAgent = { id: agent.id, name: agent.name, role: agent.role,
          model_ref: agent.model_ref, revision: profile.revision, invalidated: false };
        closeRolePicker();
        renderRoleSelection();
        status('已选择下一条普通聊天的角色。尚未发送消息；不增加工具或任务操作权限。');
      };
      roleButtons.push(button);
      $('role-options').append(button);
    }
    renderRoleSelection();
  } catch (error) {
    if (valid()) $('role-options').replaceChildren(article('无法读取角色：' + error.message + '。未改变当前选择，可关闭后重试。'));
  } finally {
    clearTimeout(timeout);
    context.signal.removeEventListener('abort', abort);
    if (roleRead === reads) roleRead = null;
    reads.abort();
  }
};
$('close-role-picker').onclick = () => { closeRolePicker(); renderRoleSelection(); };
$('clear-role').onclick = () => {
  if (!current(session()) || roleLocked()) return;
  selectedAgent = null;
  closeRolePicker();
  renderRoleSelection();
  status('已明确恢复默认管家；下一条普通聊天不指定其他角色。', { transient: true });
};

const workerUI = createWorkerUI({ api, current, article, conversation: taskPane });

const taskSignature = task => JSON.stringify([
  task.id, task.revision, task.requester, task.owner, task.status, task.summary, task.result, task.details ?? null
]);
const MAX_DETAILS_BYTES = 256 * 1024;

function renderTaskContext() {
  const task = pendingTurn?.selectedTask || selectedTask;
  const locked = !!pendingTurn || busy.has('send') || busy.has('turn-check');
  $('task-context').classList[task ? 'remove' : 'add']('hide');
  const contextNotice = !task ? '' : task.invalidated
    ? '讨论任务已失效，请重新选择或清除后发送。'
    : pendingTurn ? '本次发送的讨论任务已锁定，核对结果前不能更换。'
      : '已选择讨论任务：' + task.summary;
  // Unchanged polling must not repeat live announcements.
  if ($('task-context-status').textContent !== contextNotice) $('task-context-status').textContent = contextNotice;
  $('task-context-title').textContent = task ? (task.invalidated ? '（已失效）' : '') + task.summary : '';
  $('task-context-label').textContent = task?.auto ? '回复给任务' : '讨论';
  $('task-context-details').textContent = task
    ? (pendingTurn ? '本次发送的任务上下文已锁定，核对结果前不能更换。'
      : task.invalidated ? '所选任务已变更或不再可见，此选择已失效。请重新选择任务，或清除后再发送。'
        : '仅用于下一条普通聊天；选择本身不会发送消息。')
      + `\n任务编号：${task.id}\n版本：${task.revision}\n${task.requester} → ${task.owner} · ${task.status}`
      + '\n上下文范围：仅此任务的参与人、状态、共享摘要、共享材料和交付结果；服务端核实后提供，长文本可能截断。不包含其他任务或完整私聊，不授权任何任务操作或执行。'
      + `\n共享摘要：\n${task.summary}`
      + (task.details ? `\n共享材料：\n${task.details}` : '')
      + `\n交付结果：\n${task.result ?? '尚无交付结果'}`
    : '';
  $('clear-task-context').disabled = !selectedTask || locked;
  discussionButtons.forEach(button => { button.disabled = locked; });
}

function updateTaskContext(context, tasks) {
  visibleTasks = new Map(tasks.filter(task => typeof task.id === 'string'
    && Number.isSafeInteger(task.revision) && task.revision >= 0
    && (task.requester === context.user || task.owner === context.user)).map(task => [task.id, task]));
  if (selectedTask && (!visibleTasks.has(selectedTask.id)
      || taskSignature(visibleTasks.get(selectedTask.id)) !== taskSignature(selectedTask))) {
    // Keep a visible invalidation notice instead of silently changing scope or
    // sending an already-written question without its intended task context.
    selectedTask.invalidated = true;
  }
  renderTaskContext();
}

function addDiscussion(element, context, task) {
  if (!visibleTasks.has(task.id)) return;
  const view = taskView;
  const button = document.createElement('button');
  button.type = 'button';
  button.textContent = '讨论此任务';
  button.onclick = () => {
    if (!current(context) || view !== taskView || pendingTurn || busy.has('send') || busy.has('turn-check')) return;
    const visible = visibleTasks.get(task.id);
    if (!visible || taskSignature(visible) !== taskSignature(task)) return;
    selectedTask = { ...visible, invalidated: false };
    renderTaskContext();
    returnToChatOnNarrowScreen();
    $('text').focus?.();
    status('已选择任务，下一条消息会附带它。', { transient: true });
  };
  discussionButtons.push(button);
  element.append(button);
}

$('clear-task-context').onclick = () => {
  if (!current(session()) || pendingTurn || busy.has('send') || busy.has('turn-check')) return;
  dismissFocus();
  selectedTask = null;
  renderTaskContext();
  status('已清除讨论的任务。', { transient: true });
};

// Only a recovery receipt is kept for this tab. Never persist credentials or chat text.
function forgetTurn() {
  pendingTurn = null;
  try { sessionStorage.removeItem(TURN_STORAGE_KEY); } catch { turnStorageAvailable = false; }
  renderTurn();
}

function rememberTurn(context) {
  try {
    sessionStorage.setItem(TURN_STORAGE_KEY, JSON.stringify({
      id: pendingTurn.id, user: context.user, browserMode: context.browserMode,
      ...(pendingTurn.selectedAgent ? { agent: { id: pendingTurn.selectedAgent.id, revision: pendingTurn.selectedAgent.revision } } : {})
    }));
    turnStorageAvailable = true;
  } catch { turnStorageAvailable = false; }
}

function restoreTurn(context) {
  let saved;
  try { saved = JSON.parse(sessionStorage.getItem(TURN_STORAGE_KEY)); }
  catch { turnStorageAvailable = false; }
  if (saved && saved.user === context.user && saved.browserMode === context.browserMode
      && typeof saved.id === 'string' && /^[a-f0-9-]{36}$/.test(saved.id)) {
    pendingTurn = { id: saved.id, status: 'checking', text: null,
      selectedAgent: validAgentBinding(saved.agent) ? { id: saved.agent.id, revision: saved.agent.revision } : null };
    if (saved.agent && !validAgentBinding(saved.agent)) pendingTurn.invalidAgentBinding = true;
    renderTurn();
  } else {
    forgetTurn();
  }
}

function renderTurn() {
  const waiting = !!pendingTurn;
  $('turn-recovery').classList[waiting ? 'remove' : 'add']('hide');
  // A normal in-flight send is not a problem to resolve: show a typing bubble and
  // keep the recovery sheet visually quiet (its text stays available to tests and
  // assistive tech). It becomes prominent only once the outcome is unresolved.
  const inFlight = waiting && (busy.has('send') || pendingTurn.status === 'checking');
  $('turn-recovery').classList[inFlight ? 'add' : 'remove']('quiet');
  $('typing').classList[inFlight ? 'remove' : 'add']('hide');
  if (inFlight) { const scroller = $('scroller'); scroller.scrollTop = scroller.scrollHeight; }
  $('turn-status').textContent = waiting
    ? (pendingTurn.status === 'checking' ? '正在自动核对上次发送状态…'
      : pendingTurn.status === 'sending' ? '正在发送，尚未收到服务端回执。'
      : pendingTurn.status === 'pending' ? '服务端已记录此请求，尚未确认处理完成。'
        : '上次发送的结果尚未确认，可能已经处理。')
      + '\n请求编号：' + pendingTurn.id
      + (busy.has('send') ? '\n正在等待响应，请勿重复发送。'
        : '\n不会自动重发。请先核对发送状态。')
      + (turnStorageAvailable ? '' : '\n当前浏览器无法保存恢复凭据，请先记下请求编号，刷新后将无法自动恢复。')
    : '';
  $('check-turn').disabled = !waiting || busy.has('send') || busy.has('turn-check');
  $('new-turn').disabled = !waiting || busy.has('send') || busy.has('turn-check');
  $('send').disabled = waiting || busy.has('send');
  renderTaskContext();
  renderRoleSelection();
  renderRepositorySelection();
}

async function completeTurn(context, response) {
  const { id: requestId, text, selectedTask: expectedTask, selectedAgent: expectedAgent } = pendingTurn;
  if (!repositoryReceiptMatches(pendingTurn.selectedRepository, response.repository_context)) throw Error('仓库回执与本次选择不符，保留恢复凭据');
  const profile = response.runtime_profile;
  if (pendingTurn.invalidAgentBinding || (expectedAgent && (!profile
      || profile.agent_id !== expectedAgent.id || profile.revision !== expectedAgent.revision
      || (expectedAgent.role && profile.role !== expectedAgent.role)
      || (expectedAgent.model_ref && profile.model_ref !== expectedAgent.model_ref)))) {
    throw Error('角色回执与本次选择不符，保留恢复凭据');
  }
  const receiptRole = profile ? `\n本次角色：${profile.agent_id} · ${profile.role} · 角色版本 ${profile.revision}` : '';
  const task = response.task_context;
  if (text !== null && (expectedTask
    ? !task || task.id !== expectedTask.id || task.revision !== expectedTask.revision
    : task != null)) throw Error('任务上下文回执与本次选择不符');
  const receiptContext = task
    ? `\n本次任务上下文：${task.id} · 版本 ${task.revision}。`
      + (task.summary_truncated ? '共享摘要已截断。' : '')
      + (task.result_truncated ? '交付结果已截断。' : '')
      + (task.details_truncated ? '共享材料已截断。' : '')
    : '';
  forgetTurn();
  if (current(context)) handleTaskAction(context, response.task_action, requestId);
  if (text !== null && $('text').value.trim() === text) $('text').value = '';
  status('已核实消息处理完成。' + receiptContext + receiptRole);
  try { await refresh(context); }
  catch (error) {
    if (current(context)) status('消息处理已完成，但会话刷新失败：' + error.message + receiptContext + receiptRole
      + (typeof response.reply === 'string' ? '\n已保存的回复：' + response.reply : ''));
  }
}

function resetIdentity({ preserveRecovery = false } = {}) {
  clearTimeout(pollTimer);
  pollTimer = null;
  refreshFlight = null;
  snapshot = '';
  renderedBusy = '';
  pollDelay = POLL_INTERVAL;
  $('last-refresh').textContent = '';
  $('sync-notice').textContent = '';
  profileUI.reset();
  workerUI.reset();
  draftUI.reset({ preserveRecovery });
  timelineUI.reset();
  controller.abort();
  controller = new AbortController();
  epoch++;
  refreshVersion++;
  token = '';
  csrfToken = '';
  user = '';
  pendingSend = null;
  pendingTurn = null;
  selectedTask = null;
  selectedAgent = null;
  closeRolePicker();
  resetRepositoryUI();
  visibleTasks = new Map();
  discussionButtons = [];
  taskView++;
  busy.clear();
  if (!preserveRecovery) forgetTurn();
  else renderTurn();
  $('conversation').replaceChildren();
  $('tasks').replaceChildren();
  $('activity').replaceChildren();
  panelOwners = new WeakMap();
  panelSlots = new Map();
  lastActivity = [];
  $('task-count').textContent = '';
  $('rail-badge').textContent = '';
  detailDrafts.clear();
  actionEditors.clear(); messagePanels.clear();
  resetTaskChat({ preserveRecovery });
  notificationTasks = null; notificationsEnabled = false;
  taskFilter = 'active'; $('task-filter').value = 'active';
  document.title = 'Agenct';
  $('enable-notifications').textContent = '开启系统通知';
  $('identity').textContent = '';
  $('text').value = '';
  $('open-agents').disabled = false;
  $('send').disabled = false;
  $('onboarding').classList.add('hide');
  $('room').classList.add('hide');
  $('login').classList.remove('hide');
}

// Only read requests are repeated. Mutations still require an explicit user action.
function schedulePoll(context = session()) {
  clearTimeout(pollTimer);
  pollTimer = null;
  if (!current(context)) return;
  pollTimer = setTimeout(() => poll(context), pollDelay);
}

async function poll(context) {
  if (!current(context)) return;
  try {
    await refresh(context, false);
    if (current(context)) pollDelay = POLL_INTERVAL;
  } catch (error) {
    if (!current(context)) return;
    if (error.httpStatus === 401) {
      resetIdentity({ preserveRecovery: true });
      status(browserMode ? '登录已失效，请重新登录。' : '登录已失效，请重新输入访问令牌。');
      return;
    }
    pollDelay = Math.min(pollDelay * 2, MAX_POLL_DELAY);
    $('last-refresh').textContent = '暂时无法同步，保留上次快照；将自动重试，也可手动刷新。';
  } finally {
    if (current(context)) schedulePoll(context);
  }
}

document.addEventListener('visibilitychange', () => {
  clearTimeout(pollTimer);
  pollTimer = null;
  if (current(session()) && user) void poll(session());
});

function refresh(context = session(), force = true) {
  if (!current(context)) return Promise.resolve();
  if (refreshFlight && refreshFlight.epoch === context.epoch) {
    // A mutation/manual refresh must obtain a snapshot started after that action.
    if (force) refreshFlight.again = true;
    return refreshFlight.promise;
  }
  const flight = { epoch: context.epoch, again: false };
  refreshFlight = flight;
  flight.promise = (async () => {
    do {
      flight.again = false;
      await readSnapshot(context);
    } while (flight.again && current(context));
  })().finally(() => {
    if (refreshFlight === flight) refreshFlight = null;
  });
  return flight.promise;
}

async function readSnapshot(context) {
  const version = ++refreshVersion;
  const reads = new AbortController();
  const abort = () => reads.abort();
  context.signal.addEventListener('abort', abort, { once: true });
  const timeout = setTimeout(abort, READ_TIMEOUT);
  let messages, proposals, tasks, activity;
  try {
    [messages, proposals, tasks, activity] = await Promise.all([
      api({ ...context, signal: reads.signal }, 'messages'),
      api({ ...context, signal: reads.signal }, 'proposals'),
      api({ ...context, signal: reads.signal }, 'tasks'),
      // Index is derived and optional: a failure must not block the conversation.
      api({ ...context, signal: reads.signal }, 'activity').catch(() => null)
    ]);
  } finally {
    clearTimeout(timeout);
    context.signal.removeEventListener('abort', abort);
    reads.abort();
  }
  if (!current(context) || version !== refreshVersion) return;
  workerUI.update(context, tasks);
  draftUI.update(context, tasks);
  timelineUI.update(context, tasks);
  updateTaskContext(context, tasks);
  focusTask(context, tasks, activity);
  updateNotifications(context, tasks);
  if (!Array.isArray(activity)) activity = lastActivity;
  lastActivity = activity;
  const nextSnapshot = JSON.stringify([messages, proposals, tasks, activity, proposals.filter(isOldProposal).map(p => p.id), taskChatVersion()]);
  const changed = snapshot !== '' && snapshot !== nextSnapshot;
  if (changed) $('sync-notice').textContent = '会话或任务有更新 · ' + new Date().toLocaleTimeString();
  $('last-refresh').textContent = (changed ? '有新的更新 · ' : '')
    + '已同步 ' + new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  // Keep focused controls, selection and drafts intact when data did not change.
  const nextBusy = JSON.stringify([...busy].filter(key => key !== 'send').sort());
  if (snapshot === nextSnapshot && renderedBusy === nextBusy) return;
  snapshot = nextSnapshot;
  renderedBusy = nextBusy;
  taskView++;
  discussionButtons = [];
  const focused = document.activeElement;
  const selection = focused && typeof focused.selectionStart === 'number' ? [focused.selectionStart, focused.selectionEnd] : null;
  const stream = $('conversation');
  const scroller = $('scroller');
  const stick = !(scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight > 160);
  const entries = renderConversation(context, messages, proposals, taskMoments(context, tasks, activity));
  const { taskCards, slots } = renderTaskCards(context, tasks);
  profileUI.append(entries);
  // Open panels (draft / timeline / queue) live inside the card they were opened
  // from; if that card is gone they fall back to the end of the list.
  const panels = [];
  workerUI.append(panels);
  draftUI.append(panels);
  timelineUI.append(panels);
  for (const panel of panels) (slots.get(panelOwners.get(panel)) || { append: node => taskCards.push(node) }).append(panel);
  panelSlots = slots;
  stream.replaceChildren(...entries);
  $('tasks').replaceChildren(...taskCards);
  applyTaskFilter();
  renderActivity(context, activity);
  $('task-count').textContent = tasks.length ? String(tasks.length) : '';
  // The rail badge counts tasks waiting on *this* user, not all tasks.
  const waiting = tasks.filter(task => (task.owner === context.user && ['awaiting_owner', 'working'].includes(task.status))
    || (task.requester === context.user && task.status === 'ready_for_acceptance')).length;
  $('rail-badge').textContent = waiting ? String(waiting) : '';
  if (stick) scroller.scrollTop = scroller.scrollHeight;
  renderTaskContext();
  if (focused?.isConnected) { focused.focus?.({ preventScroll: true }); if (selection) focused.setSelectionRange?.(...selection); }

}

async function enterRoom(context) {
  const me = await api(context, 'me');
  if (!current(context)) return;
  if (context.browserMode && me.user !== context.user) throw Error('登录身份已变更，请重新加载页面');
  user = me.user;
  context.user = user;
  restoreTurn(context);
  const mode = me.runtime === 'dsh' ? '真实模型' : me.runtime === 'mock' ? '离线演示' : '服务模式 ' + me.runtime;
  $('identity').replaceChildren(el('b', null, `${user} 的管家`), el('span', 'muted', ` · ${mode}`), el('span', 'sr-only', ' · 已验证身份'));
  $('login').classList.add('hide');
  $('room').classList.remove('hide');
  await refresh(context);
  if (current(context) && pendingTurn) await $('check-turn').onclick();
  if (current(context)) { schedulePoll(context); showOnboarding(context); }
}

async function checkSession() {
  resetIdentity({ preserveRecovery: true });
  const checkEpoch = epoch;
  browserMode = null;
  $('dev-login').classList.add('hide');
  $('oidc-login').classList.add('hide');
  $('auth-retry').classList.add('hide');
  status('正在检查登录状态…');
  const timeout = setTimeout(() => { if (epoch === checkEpoch) controller.abort(); }, READ_TIMEOUT);
  try {
    const response = await fetch('/auth/session', {
      method: 'GET', credentials: 'same-origin', cache: 'no-store', signal: controller.signal
    });
    if (!response.ok) throw Error('无法检查登录状态，请重试');
    const data = await response.json();
    if (epoch !== checkEpoch) return;
    if (typeof data.enabled !== 'boolean') throw Error('登录配置响应无效');
    browserMode = data.enabled;
    clearTimeout(timeout);
    status('');
    if (!browserMode) {
      $('dev-login').classList.remove('hide');
      return;
    }
    $('oidc-login').classList.remove('hide');
    if (data.authenticated) {
      if (typeof data.user !== 'string' || !data.user || typeof data.csrf_token !== 'string' || !data.csrf_token) {
        throw Error('登录会话响应无效');
      }
      user = data.user;
      csrfToken = data.csrf_token;
      await enterRoom(session());
    }
    // An expired/unknown login does not establish a different receipt owner.
  } catch (error) {
    if (epoch !== checkEpoch) return;
    resetIdentity({ preserveRecovery: true });
    status(error.message + '。请重试登录状态检查。');
    $('auth-retry').classList.remove('hide');
  } finally {
    clearTimeout(timeout);
  }
}

$('auth-retry').onclick = () => { if (!logoutPending) return checkSession(); };
$('oidc-login').onclick = () => {
  if (browserMode === true && !logoutPending) window.location.assign('/auth/login');
};
$('enter').onclick = async () => {
  if (browserMode !== false) return;
  const enteredToken = $('token').value;
  $('token').value = '';
  resetIdentity({ preserveRecovery: true });
  token = enteredToken;
  const context = session();
  status('');
  try {
    await enterRoom(context);
  } catch (error) {
    if (context.epoch !== epoch) return;
    resetIdentity({ preserveRecovery: true });
    status(error.message);
  }
};
$('logout').onclick = async () => {
  if (logoutPending) return;
  const context = session();
  resetIdentity();
  if (!context.browserMode) {
    status('已清除本页身份和显示内容。已发出的服务端操作不会因退出而撤销。');
    return;
  }
  const logoutEpoch = epoch;
  logoutPending = true;
  $('oidc-login').disabled = true;
  status('正在退出服务器会话…');
  const logoutController = new AbortController();
  const timeout = setTimeout(() => logoutController.abort(), READ_TIMEOUT);
  try {
    const response = await fetch('/auth/logout', {
      method: 'POST', credentials: 'same-origin', signal: logoutController.signal,
      headers: { 'X-CSRF-Token': context.csrfToken }
    });
    if (!response.ok) throw Error('服务器未确认退出');
    if (epoch === logoutEpoch) status('已退出服务器会话并清除本页内容。已发出的操作不会因退出而撤销。');
  } catch (error) {
    if (epoch === logoutEpoch) {
      status('本页内容已清除，但无法确认服务器会话已退出。请检查登录状态后重试退出。');
      $('auth-retry').classList.remove('hide');
    }
  } finally {
    clearTimeout(timeout);
    logoutPending = false;
    $('oidc-login').disabled = false;
  }
};
$('refresh').onclick = async () => {
  const context = session();
  try { await refresh(context); }
  catch (error) { if (current(context)) status(error.message); }
};
$('check-turn').onclick = async () => {
  const context = session(), turn = pendingTurn;
  if (!current(context) || !turn || busy.has('send') || busy.has('turn-check')) return;
  busy.add('turn-check');
  renderTurn();
  status('正在核对发送状态…');
  const reads = new AbortController();
  const abort = () => reads.abort();
  context.signal.addEventListener('abort', abort, { once: true });
  const timeout = setTimeout(abort, READ_TIMEOUT);
  try {
    const receipt = await api({ ...context, signal: reads.signal }, 'turns/' + encodeURIComponent(turn.id));
    if (!current(context) || pendingTurn !== turn) return;
    if (receipt.request_id !== turn.id || !['complete', 'pending', 'uncertain'].includes(receipt.status)
        || (receipt.status === 'complete' && (!receipt.response || typeof receipt.response !== 'object'))) {
      throw Error('发送状态响应无效');
    }
    if (receipt.status === 'complete') {
      await completeTurn(context, receipt.response);
    } else {
      turn.status = receipt.status;
      status(receipt.status === 'pending' ? '服务端已记录此请求，尚未确认处理完成。稍后可再次核对，未重发消息。' : '服务端无法确认上次处理结果。未重发消息。');
    }
  } catch (error) {
    if (!current(context) || pendingTurn !== turn) return;
    turn.status = 'uncertain';
    status(error.httpStatus === 404
      ? '当前账号未找到这条发送记录，仍无法确认上次结果。未重发消息。'
      : error.message + '。暂时无法核对上次结果，未重发消息。');
  } finally {
    clearTimeout(timeout);
    context.signal.removeEventListener('abort', abort);
    reads.abort();
    if (current(context)) {
      busy.delete('turn-check');
      renderTurn();
    }
  }
};

$('new-turn').onclick = () => {
  const context = session(), turn = pendingTurn;
  if (!current(context) || !turn || busy.has('send') || busy.has('turn-check')) return;
  if (!confirm('上一次发送的结果仍未知，消息可能已经处理。开始新尝试后再次发送可能重复处理。是否放弃此恢复凭据并开始新尝试？')) return;
  if (!current(context) || pendingTurn !== turn) return;
  forgetTurn();
  status('已开始新尝试。请核对输入内容，如需角色或任务上下文请重新选择，再点击发送；将使用新的请求编号。上次操作不会被撤销。');
};

$('form').onsubmit = async event => {
  event.preventDefault();
  const context = session();
  const text = $('text').value.trim();
  if (!text || !current(context) || busy.has('send')) return;
  // A short yes/no to a step waiting on the card is that confirmation itself.
  if (await replyToPendingAction(context, text)) return;
  if (pendingTurn) {
    status('上次发送结果尚未确认。请先核对发送状态，或明确选择开始新尝试。');
    return;
  }
  closeRolePicker();
  busy.add('send');
  renderTurn();
  status('');
  let chatTurn = null;
  try {
    if (await profileUI.handle(context, text)) {
      if (current(context)) {
        if ($('text').value.trim() === text) $('text').value = '';
        pendingSend = null;
      }
      return;
    }
    // A profile read may finish after logout or a different login.
    if (!current(context)) return;
    const delegation = text.match(/^\/delegate\s+([a-zA-Z0-9_-]+)\s+([\s\S]+)$/);
    if (delegation) {
      // Proposal creation retains its existing, explicit idempotent retry behavior.
      if (!pendingSend || pendingSend.text !== text) pendingSend = { text, id: crypto.randomUUID() };
      await api(context, 'proposals', 'POST', {
        owner: delegation[1], summary: delegation[2], request_id: pendingSend.id
      });
      if (!current(context)) return;
      pendingSend = null;
      if ($('text').value.trim() === text) $('text').value = '';
      await refresh(context);
    } else {
      if (selectedAgent?.invalidated) {
        status('所选角色已失效，未发送消息。请重新选择角色或明确恢复默认管家。');
        return;
      }
      if (selectedTask && (selectedTask.invalidated || !visibleTasks.has(selectedTask.id)
          || taskSignature(visibleTasks.get(selectedTask.id)) !== taskSignature(selectedTask))) {
        selectedTask.invalidated = true;
        status('任务上下文已失效，未发送消息。请重新选择任务，或清除任务上下文后再发送。');
        return;
      }
      const message = repositoryChatMessage(text, selectedRepository);
      closeRepositoryPicker();
      chatTurn = { text, message, id: crypto.randomUUID(), status: 'sending', selectedTask, selectedAgent, selectedRepository };
      selectedRepository = null;
      selectedAgent = null;
      selectedTask = null;
      pendingTurn = chatTurn;
      rememberTurn(context);
      renderTurn();
      // The message now lives in the conversation; the composer is free again.
      if ($('text').value.trim() === text) { $('text').value = ''; $('text').oninput?.(); }
      const response = await postChat(context, { message: chatTurn.message, request_id: chatTurn.id,
        ...(chatTurn.selectedAgent ? { agent_id: chatTurn.selectedAgent.id,
          expected_profile_revision: chatTurn.selectedAgent.revision } : {}),
        ...(chatTurn.selectedTask ? { selected_task: {
          task_id: chatTurn.selectedTask.id, expected_revision: chatTurn.selectedTask.revision
        } } : {})
      });
      if (!current(context) || pendingTurn !== chatTurn) return;
      const receipt = response.turn;
      if (!receipt || receipt.request_id !== chatTurn.id || !['complete', 'pending', 'uncertain'].includes(receipt.status)) {
        throw Error('发送状态响应无效');
      }
      if (receipt.status === 'complete') {
        await completeTurn(context, response);
      } else {
        chatTurn.status = receipt.status;
        status('尚未确认消息处理完成。请核对发送状态；不会自动重发。');
      }
    }
  } catch (error) {
    if (!current(context)) return;
    if (chatTurn && pendingTurn === chatTurn) {
      const rejectedSelection = chatTurn.selectedTask
        && ((error.httpStatus === 409 && error.code === 'selected_task_stale_before_dispatch')
          || (error.httpStatus === 404 && error.code === 'selected_task_unavailable_before_dispatch'));
      const rejectedAgent = chatTurn.selectedAgent
        && ((error.httpStatus === 409 && error.code === 'selected_agent_stale_before_dispatch')
          || (error.httpStatus === 400 && error.code === 'selected_agent_unavailable_before_dispatch'));
      if (rejectedAgent || rejectedSelection) {
        selectedRepository = chatTurn.selectedRepository;
        selectedAgent = chatTurn.selectedAgent ? { ...chatTurn.selectedAgent,
          invalidated: rejectedAgent || chatTurn.selectedAgent.invalidated } : null;
        // These two server codes guarantee rejection before receipt reservation
        // and provider dispatch. Generic conflicts/not-found stay uncertain.
        selectedTask = chatTurn.selectedTask ? { ...chatTurn.selectedTask,
          invalidated: rejectedSelection || chatTurn.selectedTask.invalidated } : null;
        forgetTurn();
        // Not sent: give the words back.
        if (!$('text').value.trim()) $('text').value = chatTurn.text;
        status(error.message + (rejectedAgent
          ? '。服务端尚未处理本次消息，所选角色已失效。请重新选择角色或明确恢复默认管家。未自动重发。'
          : '。服务端尚未处理本次消息，任务上下文已失效。请重新选择任务，或清除后再发送。未自动重发。'));
        await refresh(context).catch(() => {});
      } else {
        chatTurn.status = 'uncertain';
        status(error.message + '。发送结果未知，请核对发送状态；不会自动重发。');
      }
    } else {
      status(error.message + '。发送结果可能需要核对，请刷新查看。');
      await refresh(context).catch(() => {});
    }
  } finally {
    if (current(context)) {
      busy.delete('send');
      renderTurn();
    }
  }
};

function isOldProposal(proposal) {
  const date = parseUtc(proposal.created_at);
  return proposal.status === 'pending' && date && Date.now() - date.getTime() >= 86400000;
}

function updateNotifications(context, tasks) {
  const waiting = tasks.filter(task => (task.owner === context.user && ['awaiting_owner', 'working'].includes(task.status))
    || (task.requester === context.user && task.status === 'ready_for_acceptance'));
  document.title = waiting.length ? `(${waiting.length}) Agenct` : 'Agenct';
  const incoming = new Set(waiting.map(task => task.id + ':' + task.status));
  if (notificationTasks && notificationsEnabled && typeof Notification !== 'undefined' && Notification.permission === 'granted') {
    const count = [...incoming].filter(key => !notificationTasks.has(key)).length;
    if (count) {
      try { new Notification('Agenct 有新的待办', { body: `${count} 项工作需要你处理，请打开 Agenct 查看。`, tag: 'agenct-tasks' }); } catch { /* Title remains available when OS notifications are unsupported. */ }
    }
  }
  notificationTasks = incoming;
}

$('enable-notifications').onclick = async () => {
  if (!current(session())) return;
  if (typeof Notification === 'undefined') { status('当前浏览器不支持系统通知；标签页标题仍会显示待办数。'); return; }
  const context = session();
  if (notificationsEnabled) { notificationsEnabled = false; $('enable-notifications').textContent = '开启系统通知'; return; }
  try {
    const permission = await Notification.requestPermission();
    if (!current(context)) return;
    notificationsEnabled = permission === 'granted';
    $('enable-notifications').textContent = notificationsEnabled ? '关闭系统通知' : '开启系统通知';
    if (!notificationsEnabled) status('系统通知未开启；标签页标题仍会显示待办数。');
  } catch { if (current(context)) status('无法开启系统通知；标签页标题仍会显示待办数。'); }
};
