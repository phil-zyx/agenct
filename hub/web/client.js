let token = '', user = '', epoch = 0, refreshVersion = 0;
let browserMode = null, csrfToken = '', logoutPending = false;
let controller = new AbortController(), pendingSend = null, pendingTurn = null;
let turnStorageAvailable = true;
let selectedAgent = null, roleGeneration = 0, roleRead = null, roleButtons = [];
let selectedTask = null, visibleTasks = new Map(), discussionButtons = [], taskView = 0;
const TURN_STORAGE_KEY = 'agenct.pending-chat-turn.v1';
const detailDrafts = new Map(); // Unsent shared-material text survives poll re-renders.
let lastActivity = [];
let pollTimer = null, pollDelay = 5000, refreshFlight = null, snapshot = '', renderedBusy = '';
const POLL_INTERVAL = 5000, MAX_POLL_DELAY = 60000, READ_TIMEOUT = 15000, CHAT_SEND_TIMEOUT = 90000;
const busy = new Set();
const $ = id => document.getElementById(id);
// Errors, uncertainty and reply fallbacks remain readable until dismissed.
// Only explicitly marked routine feedback may expire. No text is persisted.
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
  awaiting_owner: ['待接单', 'wait'], working: ['进行中', 'work'], ready_for_acceptance: ['待验收', 'review'],
  completed: ['已完成', 'done'], canceled: ['已取消', 'muted']
};
const PROPOSAL_STATUS = { pending: ['待你确认', 'review'], confirmed: ['已确认', 'done'], rejected: ['已拒绝', 'muted'] };
const MESSAGE_STATUS = { pending: '发送中…', failed: '未发送成功', interrupted: '已中断', uncertain: '未确认' };
const ACTION_STYLE = { confirm: 'primary', accept: 'primary', deliver: 'primary', approve: 'primary', cancel: 'danger' };
function badge(table, value) {
  const [label, tone] = table[value] || [value, 'muted'];
  return el('span', 'badge badge-' + tone, label);
}
// Server times are UTC "YYYY-MM-DD HH:MM:SS"; anything else is treated as unknown.
function parseUtc(value) {
  if (typeof value !== 'string' || !/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/.test(value)) return null;
  const date = new Date(value.replace(' ', 'T') + 'Z');
  return Number.isNaN(date.getTime()) ? null : date;
}
const pad = n => String(n).padStart(2, '0');
function dayLabel(date) {
  const today = new Date();
  const start = d => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const days = Math.round((start(today) - start(date)) / 86400000);
  if (days === 0) return '今天';
  if (days === 1) return '昨天';
  return `${date.getMonth() + 1}月${date.getDate()}日`;
}
const clock = date => `${pad(date.getHours())}:${pad(date.getMinutes())}`;
const formatWhen = date => (dayLabel(date) === '今天' ? '' : dayLabel(date) + ' ') + clock(date);

const ACTIVITY_ICON = {
  pending: '○', awaiting_owner: '○', working: '◔', ready_for_acceptance: '◑', completed: '✓', canceled: '✕', rejected: '✕'
};
function activityLine(item, me) {
  const l = item.latest || {};
  const who = l.actor === me ? '你' : l.actor + ' ';
  switch (l.action) {
    case 'proposal_pending': return `等你确认 · 委派给 ${item.counterpart}`;
    case 'proposal_rejected': return '已拒绝，未共享';
    case 'created': return item.role === 'owner' ? `${item.counterpart} 委派给你，待接单` : `已委派给 ${item.counterpart}，等对方接单`;
    case 'accept': return `${who}已接单`;
    case 'deliver': return `${who}提交了交付`;
    case 'approve': return `${who}验收通过`;
    case 'return': return `${who}退回了交付`;
    case 'cancel': return `${who}取消了任务` + (l.reason ? `：${l.reason}` : '');
    case 'draft_complete': return '你的 Agent 起草了交付草稿（仅你可见）';
    case 'draft_uncertain': return '起草结果未知，可在草稿中核对';
    case 'draft_pending': return '你的 Agent 正在起草…';
    default: return String(l.action || '');
  }
}
function flash(element) {
  if (!element) return;
  element.scrollIntoView?.({ block: 'center', behavior: 'smooth' });
  element.classList.remove('flash');
  void element.offsetWidth;
  element.classList.add('flash');
}
function renderActivity(context, items) {
  const groups = [];
  for (const item of Array.isArray(items) ? items : []) {
    if (!item || typeof item.id !== 'string') continue;
    const at = parseUtc(item.latest?.at);
    const label = at ? dayLabel(at) : '更早';
    if (!groups.length || groups[groups.length - 1].label !== label) groups.push({ label, items: [] });
    groups[groups.length - 1].items.push({ item, at });
  }
  const nodes = [];
  for (const group of groups) {
    nodes.push(el('h3', 'day', group.label));
    for (const { item, at } of group.items) {
      const row = el('button', 'activity-item status-' + item.status);
      row.type = 'button';
      const icon = el('span', 'activity-icon', ACTIVITY_ICON[item.status] || '•');
      const body = el('span', 'activity-body');
      body.append(el('span', 'activity-title', item.title), el('span', 'activity-line', activityLine(item, context.user)));
      row.append(icon, body, el('span', 'activity-time', at ? clock(at) : ''));
      row.onclick = () => {
        if (!current(context)) return;
        if (item.kind === 'task') {
          setPaneTab('tasks');
          flash([...$('tasks').children].find(card => card.taskId === item.id));
        } else {
          returnToChatOnNarrowScreen();
          flash([...$('conversation').children].find(card => card.proposalId === item.id));
        }
      };
      nodes.push(row);
    }
  }
  $('activity').replaceChildren(...nodes);
}
function setPaneTab(tab) {
  const tasks = tab === 'tasks';
  $('tasks').classList[tasks ? 'remove' : 'add']('hide');
  $('activity').classList[tasks ? 'add' : 'remove']('hide');
  $('tab-tasks').classList[tasks ? 'add' : 'remove']('on');
  $('tab-activity').classList[tasks ? 'remove' : 'add']('on');
  $('tab-tasks').setAttribute('aria-selected', String(tasks));
  $('tab-activity').setAttribute('aria-selected', String(!tasks));
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
    ? `本次角色：${agent.name || agent.id} · ${agent.role || '以原始回执为准'} · 配置版本 ${agent.revision}`
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
      throw Error('个人角色配置响应无效');
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
const MAX_DETAILS_BYTES = 12000;

function renderTaskContext() {
  const task = pendingTurn?.selectedTask || selectedTask;
  const locked = !!pendingTurn || busy.has('send') || busy.has('turn-check');
  $('task-context').classList[task ? 'remove' : 'add']('hide');
  const contextNotice = !task ? '' : task.invalidated
    ? '讨论任务已失效，请重新选择或清除后发送。'
    : pendingTurn ? '本次发送的讨论任务已锁定，核对结果前不能更换。'
      : '已选择讨论任务：' + task.summary;
  // Unchanged polling must not repeatedly announce the same selection.
  if ($('task-context-status').textContent !== contextNotice) $('task-context-status').textContent = contextNotice;
  $('task-context-title').textContent = task ? (task.invalidated ? '（已失效）' : '') + task.summary : '';
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
    pendingTurn = { id: saved.id, status: 'uncertain', text: null,
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
  const inFlight = waiting && busy.has('send');
  $('turn-recovery').classList[inFlight ? 'add' : 'remove']('quiet');
  $('typing').classList[inFlight ? 'remove' : 'add']('hide');
  if (inFlight) { const scroller = $('scroller'); scroller.scrollTop = scroller.scrollHeight; }
  $('turn-status').textContent = waiting
    ? (pendingTurn.status === 'sending' ? '正在发送，尚未收到服务端回执。'
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
}

async function completeTurn(context, response) {
  const { text, selectedTask: expectedTask, selectedAgent: expectedAgent } = pendingTurn;
  const profile = response.runtime_profile;
  if (pendingTurn.invalidAgentBinding || (expectedAgent && (!profile
      || profile.agent_id !== expectedAgent.id || profile.revision !== expectedAgent.revision
      || (expectedAgent.role && profile.role !== expectedAgent.role)
      || (expectedAgent.model_ref && profile.model_ref !== expectedAgent.model_ref)))) {
    throw Error('角色回执与本次选择不符，保留恢复凭据');
  }
  const receiptRole = profile ? `\n本次角色：${profile.agent_id} · ${profile.role} · 配置版本 ${profile.revision}` : '';
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
  $('identity').textContent = '';
  $('text').value = '';
  $('open-agents').disabled = false;
  $('send').disabled = false;
  $('room').classList.add('hide');
  $('login').classList.remove('hide');
}

function addActions(element, context, kind, item, actions) {
  const key = kind + ':' + item.id;
  const buttons = actions.map(([action, label]) => {
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = label;
    if (ACTION_STYLE[action]) button.className = ACTION_STYLE[action];
    button.disabled = busy.has(key);
    button.onclick = async () => {
      if (!current(context) || busy.has(key)) return;
      let result, reason, details;
      if (kind === 'proposals' && action === 'confirm') {
        const entered = element.detailsBox?.value ?? '';
        if (entered.trim()) {
          if (new TextEncoder().encode(entered).length > MAX_DETAILS_BYTES) {
            status('共享材料最多 12000 字节（约 4000 个汉字）。尚未确认。'); return;
          }
          details = entered;
        }
      }
      if (action === 'deliver') {
        result = prompt('填写并确认人工交付结果（不会执行任何代码；确认后对方可见）', draftUI.reviewedText(item));
        if (!result || !current(context)) return;
      }
      if (action === 'cancel') {
        const entered = prompt('取消任务：请填写 1–500 字的简短原因，确认后双方可见。已有交付记录会保留；已发出的模型请求可能继续运行并产生费用。');
        if (entered === null || !current(context)) return;
        reason = entered.trim();
        if (!reason || Array.from(reason).length > 500) { status('取消原因须为 1–500 字。尚未发送取消请求。'); return; }
      }
      busy.add(key);
      buttons.forEach(control => { control.disabled = true; });
      status('');
      try {
        await api(context, kind + '/' + encodeURIComponent(item.id) + '/actions', 'POST', {
          action, result, ...(reason === undefined ? {} : { reason }),
          ...(details === undefined ? {} : { details }), expected_revision: item.revision
        });
        if (!current(context)) return;
        status(kind === 'proposals'
          ? (action === 'confirm' ? (details ? '已确认共享摘要和共享材料，并创建任务。' : '已确认共享摘要并创建任务。') : '已拒绝提案，未创建任务。')
          : '已更新。', { transient: true });
      } catch (error) {
        if (current(context)) status(error.message + '。请刷新核对服务端状态后再操作。');
      } finally {
        if (current(context)) {
          busy.delete(key);
          buttons.forEach(control => { control.disabled = false; });
          await refresh(context).catch(error => { if (current(context)) status(error.message); });
        }
      }
    };
    element.append(button);
    return button;
  });
}

// Only read requests are repeated. Mutations still require an explicit user action.
function schedulePoll(context = session()) {
  clearTimeout(pollTimer);
  pollTimer = null;
  if (!current(context) || document.hidden) return;
  pollTimer = setTimeout(() => poll(context), pollDelay);
}

async function poll(context) {
  if (!current(context) || document.hidden) return;
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
  if (!document.hidden && current(session()) && user) void poll(session());
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
  if (!Array.isArray(activity)) activity = lastActivity;
  lastActivity = activity;
  const nextSnapshot = JSON.stringify([messages, proposals, tasks, activity]);
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
  const stream = $('conversation');
  const scroller = $('scroller');
  const stick = !(scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight > 160);
  const separators = new Map();
  let previous = null;
  messages.forEach((message, index) => {
    const at = parseUtc(message.created_at);
    if (!at) return;
    if (!previous || at - previous >= 10 * 60 * 1000 || at.toDateString() !== previous.toDateString()) {
      separators.set(index, el('div', 'time-sep', formatWhen(at)));
    }
    previous = at;
  });
  const entries = messages.map(message => {
    const notice = message.role !== 'user' && /^\[(系统提示|MOCK)/.test(message.content);
    const bubble = el('article', 'msg ' + (message.role === 'user' ? 'msg-user' : notice ? 'msg-notice' : 'msg-assistant')
      + (message.status === 'failed' ? ' msg-failed' : ''));
    bubble.append(el('div', 'msg-body', message.content));
    if (message.status !== 'complete') bubble.append(el('div', 'msg-status', MESSAGE_STATUS[message.status] || message.status));
    return bubble;
  });
  // Place each proposal right after the exchange that produced it. A proposal's
  // summary is an exact excerpt of its source user message; unmatched proposals
  // (e.g. /delegate or test-double turns) stay at the end as before.
  // Each proposal claims the latest unclaimed matching user message, so the
  // result does not depend on the order the proposals API returns.
  const after = new Map();
  const claimed = new Set();
  const trailing = [];
  for (const proposal of proposals) {
    let at = -1;
    for (let i = messages.length - 1; i >= 0; i--) {
      if (!claimed.has(i) && messages[i].role === 'user' && typeof proposal.summary === 'string' && proposal.summary
          && String(messages[i].content).includes(proposal.summary)) { at = i; break; }
    }
    if (at === -1) { trailing.push(proposal); continue; }
    claimed.add(at);
    const anchor = messages[at + 1] && messages[at + 1].role !== 'user' ? at + 1 : at;
    if (!after.has(anchor)) after.set(anchor, []);
    after.get(anchor).push(proposal);
  }
  // Unmatched proposals (e.g. /delegate, or a source message outside the loaded
  // window) are placed by their own timestamp; undated legacy ones predate the
  // window and go first. Nothing piles up at the bottom of the conversation.
  const leading = [];
  for (const proposal of trailing) {
    const at = parseUtc(proposal.created_at);
    let anchor = -1, anyDated = false;
    if (at) messages.forEach((message, index) => {
      const when = parseUtc(message.created_at);
      if (when) { anyDated = true; if (when <= at) anchor = index; }
    });
    if (anchor >= 0) {
      if (!after.has(anchor)) after.set(anchor, []);
      after.get(anchor).push(proposal);
    } else if (at && !anyDated && messages.length) {
      if (!after.has(messages.length - 1)) after.set(messages.length - 1, []);
      after.get(messages.length - 1).push(proposal);
    } else leading.push(proposal);
  }
  const ordered = leading.map(proposal => ({ proposal }));
  entries.forEach((bubble, index) => {
    if (separators.has(index)) ordered.push({ bubble: separators.get(index) });
    ordered.push({ bubble });
    for (const proposal of after.get(index) || []) ordered.push({ proposal });
  });
  entries.length = 0;
  for (const item of ordered) {
    if (item.bubble) { entries.push(item.bubble); continue; }
    const proposal = item.proposal;
    const element = el('article', 'card proposal' + (proposal.status === 'pending' ? '' : ' settled'));
    element.proposalId = proposal.id;
    const head = el('header', 'card-head');
    head.append(el('span', 'card-kicker', '委派提案'), badge(PROPOSAL_STATUS, proposal.status));
    element.append(head, el('div', 'field', '接收人：' + proposal.owner), el('blockquote', 'summary-quote', proposal.summary));
    if (proposal.task_id) element.append(el('div', 'meta', '已创建任务 #' + String(proposal.task_id).slice(0, 8)));
    if (proposal.status === 'pending') {
      element.append(el('p', 'note', '尚未发送给对方，也未创建任务。确认后，对方只会看到接收人、上面的摘要和你附带的材料，不会收到你的私聊。'));
      const label = el('label', null, '附带共享材料（可选）· 会原样发给接收人，对方的 Agent 起草时也会读取；请勿粘贴密钥或私密信息。');
      const box = el('textarea');
      box.rows = 3;
      box.maxLength = MAX_DETAILS_BYTES;
      box.placeholder = '粘贴对方完成任务需要的内容，例如文档正文或需求说明…';
      const key = 'proposals:' + proposal.id;
      box.value = detailDrafts.get(key) ?? '';
      box.oninput = () => detailDrafts.set(key, box.value);
      label.append(box);
      // Collapsed until asked for, unless the user already typed something.
      const reveal = el('button', 'text-btn add-material', '＋ 附带共享材料（可选）');
      reveal.type = 'button';
      if (!box.value) label.classList.add('hide');
      else reveal.classList.add('hide');
      reveal.onclick = () => { label.classList.remove('hide'); reveal.classList.add('hide'); box.focus?.(); };
      const actions = el('div', 'actions');
      actions.detailsBox = box;
      element.detailsBox = box;
      element.append(reveal, label, actions);
      addActions(actions, context, 'proposals', proposal, [['confirm', '确认共享并委派'], ['reject', '拒绝，不共享']]);
    }
    entries.push(element);
  }
  const taskCards = [];
  const slots = new Map();
  for (const task of tasks) {
    const element = el('article', 'card task task-' + task.status);
    element.taskId = task.id;
    const head = el('header', 'card-head');
    const mine = task.owner === context.user ? '我负责' : task.requester === context.user ? '我发起' : '';
    head.append(badge(TASK_STATUS, task.status));
    if (mine) head.append(el('span', 'badge badge-mine', mine));
    const id = el('span', 'card-id', '#' + String(task.id).slice(0, 8));
    id.title = '任务编号：' + task.id + ' · 版本：' + task.revision;
    head.append(id);
    element.append(head, el('h3', 'task-title', task.summary),
      el('div', 'meta', `${task.requester} → ${task.owner}`));
    if (task.details) {
      const material = el('details', 'material');
      material.append(el('summary', null, '共享材料'), el('pre', null, task.details));
      element.append(material);
    }
    if (task.result) {
      // Finished tasks keep the delivery one click away instead of filling the list.
      const finished = ['completed', 'canceled'].includes(task.status);
      const delivery = el(finished ? 'details' : 'div', 'delivery');
      delivery.append(el(finished ? 'summary' : 'span', 'delivery-label', '最新交付'), el('pre', null, task.result));
      element.append(delivery);
    }
    const primary = [];
    if (task.owner === context.user && task.status === 'awaiting_owner') primary.push(['accept', '接单']);
    if (task.owner === context.user && task.status === 'working') primary.push(['deliver', '提交人工交付']);
    if (task.requester === context.user && task.status === 'ready_for_acceptance') primary.push(['approve', '验收'], ['return', '退回']);
    const canCancel = (task.requester === context.user && ['awaiting_owner', 'working', 'ready_for_acceptance'].includes(task.status))
      || (task.owner === context.user && ['awaiting_owner', 'working'].includes(task.status));
    const foot = el('div', 'card-foot');
    const actionRow = el('div', 'actions');
    const tools = el('div', 'tools');
    const more = el('details', 'more');
    const menu = el('div', 'more-menu');
    more.append(el('summary', null, '更多'), menu);
    foot.append(actionRow, tools);
    addActions(actionRow, context, 'tasks', task, primary);
    addDiscussion(tools, context, task);
    draftUI.attach(tools, context, task);
    timelineUI.attach(menu, context, task);
    workerUI.attach(menu, context, task);
    if (canCancel) addActions(menu, context, 'tasks', task, [['cancel', '取消任务']]);
    if (menu.children.length) tools.append(more);
    const slot = el('div', 'card-panels');
    slots.set(task.id, slot);
    element.append(foot, slot);
    taskCards.push(element);
  }
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
  renderActivity(context, activity);
  $('task-count').textContent = tasks.length ? String(tasks.length) : '';
  // The rail badge counts tasks waiting on *this* user, not all tasks.
  const waiting = tasks.filter(task => (task.owner === context.user && ['awaiting_owner', 'working'].includes(task.status))
    || (task.requester === context.user && task.status === 'ready_for_acceptance')).length;
  $('rail-badge').textContent = waiting ? String(waiting) : '';
  if (stick) scroller.scrollTop = scroller.scrollHeight;
  renderTaskContext();

}

async function enterRoom(context) {
  const me = await api(context, 'me');
  if (!current(context)) return;
  if (context.browserMode && me.user !== context.user) throw Error('登录身份已变更，请重新加载页面');
  user = me.user;
  context.user = user;
  restoreTurn(context);
  const mode = me.runtime === 'dsh' ? '真实模型' : me.runtime === 'mock' ? '离线演示' : '运行时 ' + me.runtime;
  $('identity').replaceChildren(el('b', null, `${user} 的管家`), el('span', 'muted', ` · ${mode}`), el('span', 'sr-only', ' · 已验证身份'));
  $('login').classList.add('hide');
  $('room').classList.remove('hide');
  await refresh(context);
  if (current(context)) schedulePoll(context);
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
      chatTurn = { text, id: crypto.randomUUID(), status: 'sending', selectedTask, selectedAgent };
      selectedAgent = null;
      selectedTask = null;
      pendingTurn = chatTurn;
      rememberTurn(context);
      renderTurn();
      const response = await postChat(context, { message: text, request_id: chatTurn.id,
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
        selectedAgent = chatTurn.selectedAgent ? { ...chatTurn.selectedAgent,
          invalidated: rejectedAgent || chatTurn.selectedAgent.invalidated } : null;
        // These two server codes guarantee rejection before receipt reservation
        // and provider dispatch. Generic conflicts/not-found stay uncertain.
        selectedTask = chatTurn.selectedTask ? { ...chatTurn.selectedTask,
          invalidated: rejectedSelection || chatTurn.selectedTask.invalidated } : null;
        forgetTurn();
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

void checkSession();

// Enter sends, Shift+Enter inserts a newline. Never send while an IME is composing
// (Chinese input confirms candidates with Enter).
$('text').onkeydown = event => {
  if (event.key !== 'Enter' || event.shiftKey || event.isComposing || event.keyCode === 229) return;
  event.preventDefault();
  if (!$('send').disabled) $('form').requestSubmit?.();
};

// Presentational chrome only: drawer, composer menu, help, toast, autogrow.
// Never stores conversation content; the drawer preference is a per-viewer nicety.
const preference = {
  get(key) { try { return localStorage.getItem(key); } catch { return null; } },
  set(key, value) { try { localStorage.setItem(key, value); } catch { /* storage unavailable */ } }
};
function setTasksOpen(open) {
  $('room').classList[open ? 'add' : 'remove']('tasks-open');
  $('toggle-tasks').setAttribute('aria-pressed', String(open));
  preference.set('agenct.tasksOpen', open ? '1' : '0');
}
$('toggle-tasks').onclick = () => setTasksOpen(!$('room').classList.contains('tasks-open'));
$('close-tasks').onclick = () => setTasksOpen(false);
function returnToChatOnNarrowScreen() {
  if (globalThis.innerWidth <= 900) setTasksOpen(false);
}
$('open-chat').onclick = () => { setTasksOpen(false); $('text').focus?.(); };
// Default: docked open on wide screens, closed on phones where it would cover the chat.
const savedTasksOpen = preference.get('agenct.tasksOpen');
setTasksOpen(savedTasksOpen === null ? !(globalThis.innerWidth <= 900) : savedTasksOpen === '1');
function setMenuOpen(open) {
  $('composer-menu').classList[open ? 'remove' : 'add']('hide');
  $('composer-menu-toggle').setAttribute('aria-expanded', String(open));
}
$('composer-menu-toggle').onclick = () => setMenuOpen($('composer-menu').classList.contains('hide'));
$('open-help').onclick = () => { returnToChatOnNarrowScreen(); setMenuOpen(true); $('help').open = true; $('help').focus?.(); };
// Same path as typing /agents: handled locally by the profile UI, never sent to a model.
$('open-agents').onclick = async () => {
  const context = session();
  if (!current(context) || $('send').disabled || $('open-agents').disabled) return;
  returnToChatOnNarrowScreen();
  $('open-agents').disabled = true;
  try {
    // Open the local editor directly. Never replace or submit an unsent message.
    await profileUI.handle(context, '/agents');
    if (current(context)) $('conversation').lastElementChild?.scrollIntoView?.({ block: 'nearest' });
  } finally {
    if (current(context)) $('open-agents').disabled = false;
  }
};
$('status').onclick = () => status('');
$('status').onkeydown = event => { if (event.key === 'Escape') { event.preventDefault(); status(''); } };
$('text').oninput = () => {
  const box = $('text');
  if (!box.style) return;
  box.style.height = 'auto';
  box.style.height = Math.min(box.scrollHeight, 200) + 'px';
};
$('tab-activity').onclick = () => setPaneTab('activity');
$('tab-tasks').onclick = () => setPaneTab('tasks');
setPaneTab('activity');
