let token = '', user = '', epoch = 0, refreshVersion = 0;
let browserMode = null, csrfToken = '', logoutPending = false;
let controller = new AbortController(), pendingSend = null, pendingTurn = null;
let turnStorageAvailable = true;
let selectedTask = null, visibleTasks = new Map(), discussionButtons = [], taskView = 0;
const TURN_STORAGE_KEY = 'agenct.pending-chat-turn.v1';
let pollTimer = null, pollDelay = 5000, refreshFlight = null, snapshot = '', renderedBusy = '';
const POLL_INTERVAL = 5000, MAX_POLL_DELAY = 60000, READ_TIMEOUT = 15000, CHAT_SEND_TIMEOUT = 90000;
const busy = new Set();
const $ = id => document.getElementById(id);
const status = text => { $('status').textContent = text; };
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

const profileUI = createProfileUI({ api, current, article, conversation: () => $('conversation'), status });

const workerUI = createWorkerUI({ api, current, article, conversation: () => $('conversation') });

const taskSignature = task => JSON.stringify([
  task.id, task.revision, task.requester, task.owner, task.status, task.summary, task.result
]);

function renderTaskContext() {
  const task = pendingTurn?.selectedTask || selectedTask;
  const locked = !!pendingTurn || busy.has('send') || busy.has('turn-check');
  $('task-context').classList[task ? 'remove' : 'add']('hide');
  $('task-context-details').textContent = task
    ? (pendingTurn ? '本次发送的任务上下文已锁定，核对结果前不能更换。'
      : task.invalidated ? '所选任务已变更或不再可见，此选择已失效。请重新选择任务，或清除后再发送。'
        : '仅用于下一条普通聊天；选择本身不会发送消息。')
      + `\n任务编号：${task.id}\n版本：${task.revision}\n${task.requester} → ${task.owner} · ${task.status}`
      + '\n上下文范围：仅此任务的参与人、状态、共享摘要和交付结果；服务端核实后提供，长文本可能截断。不包含其他任务或完整私聊，不授权任何任务操作或执行。'
      + `\n共享摘要：\n${task.summary}\n交付结果：\n${task.result ?? '尚无交付结果'}`
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
    status('已选择此任务作为下一条普通聊天的上下文。请填写问题后点击发送；尚未发送消息或执行任务操作。');
  };
  discussionButtons.push(button);
  element.append(button);
}

$('clear-task-context').onclick = () => {
  if (!current(session()) || pendingTurn || busy.has('send') || busy.has('turn-check')) return;
  selectedTask = null;
  renderTaskContext();
  status('已清除任务上下文，下一条聊天不会自动附带任务。');
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
      id: pendingTurn.id, user: context.user, browserMode: context.browserMode
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
    pendingTurn = { id: saved.id, status: 'uncertain', text: null };
    renderTurn();
  } else {
    forgetTurn();
  }
}

function renderTurn() {
  const waiting = !!pendingTurn;
  $('turn-recovery').classList[waiting ? 'remove' : 'add']('hide');
  $('turn-status').textContent = waiting
    ? (pendingTurn.status === 'pending' ? '消息仍在处理中，尚未确认完成。' : '上次发送的结果尚未确认，可能已经处理。')
      + '\n请求编号：' + pendingTurn.id + '\n不会自动重发。请先核对发送状态。'
      + (turnStorageAvailable ? '' : '\n当前浏览器无法保存恢复凭据，请先记下请求编号，刷新后将无法自动恢复。')
    : '';
  $('check-turn').disabled = !waiting || busy.has('send') || busy.has('turn-check');
  $('new-turn').disabled = !waiting || busy.has('send') || busy.has('turn-check');
  $('send').disabled = waiting || busy.has('send');
  renderTaskContext();
}

async function completeTurn(context, response) {
  const { text, selectedTask: expectedTask } = pendingTurn;
  const task = response.task_context;
  if (text !== null && (expectedTask
    ? !task || task.id !== expectedTask.id || task.revision !== expectedTask.revision
    : task != null)) throw Error('任务上下文回执与本次选择不符');
  const receiptContext = task
    ? `\n本次任务上下文：${task.id} · 版本 ${task.revision}。`
      + (task.summary_truncated ? '共享摘要已截断。' : '')
      + (task.result_truncated ? '交付结果已截断。' : '')
    : '';
  forgetTurn();
  if (text !== null && $('text').value.trim() === text) $('text').value = '';
  status('已核实消息处理完成。' + receiptContext);
  try { await refresh(context); }
  catch (error) {
    if (current(context)) status('消息处理已完成，但会话刷新失败：' + error.message + receiptContext
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
  visibleTasks = new Map();
  discussionButtons = [];
  taskView++;
  busy.clear();
  if (!preserveRecovery) forgetTurn();
  else renderTurn();
  $('conversation').replaceChildren();
  $('identity').textContent = '';
  $('text').value = '';
  $('send').disabled = false;
  $('room').classList.add('hide');
  $('login').classList.remove('hide');
}

function addActions(element, context, kind, item, actions) {
  const key = kind + ':' + item.id;
  const buttons = actions.map(([action, label]) => {
    const button = document.createElement('button');
    button.textContent = label;
    button.disabled = busy.has(key);
    button.onclick = async () => {
      if (!current(context) || busy.has(key)) return;
      let result;
      if (action === 'deliver') {
        result = prompt('填写人工交付结果（不会执行任何代码）');
        if (!result || !current(context)) return;
      }
      busy.add(key);
      buttons.forEach(control => { control.disabled = true; });
      status('');
      try {
        await api(context, kind + '/' + encodeURIComponent(item.id) + '/actions', 'POST', {
          action, result, expected_revision: item.revision
        });
        if (!current(context)) return;
        status(kind === 'proposals'
          ? (action === 'confirm' ? '已确认共享摘要并创建任务。' : '已拒绝提案，未创建任务。')
          : '任务状态已更新。');
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
  let messages, proposals, tasks;
  try {
    [messages, proposals, tasks] = await Promise.all([
      api({ ...context, signal: reads.signal }, 'messages'),
      api({ ...context, signal: reads.signal }, 'proposals'),
      api({ ...context, signal: reads.signal }, 'tasks')
    ]);
  } finally {
    clearTimeout(timeout);
    context.signal.removeEventListener('abort', abort);
    reads.abort();
  }
  if (!current(context) || version !== refreshVersion) return;
  workerUI.update(context, tasks);
  updateTaskContext(context, tasks);
  const nextSnapshot = JSON.stringify([messages, proposals, tasks]);
  const changed = snapshot !== '' && snapshot !== nextSnapshot;
  if (changed) $('sync-notice').textContent = '会话或任务有更新 · ' + new Date().toLocaleTimeString();
  $('last-refresh').textContent = (changed ? '有新的会话或任务更新。' : '')
    + '已同步：' + new Date().toLocaleTimeString() + '。页面可见时每 5 秒自动检查。';
  // Keep focused controls, selection and drafts intact when data did not change.
  const nextBusy = JSON.stringify([...busy].filter(key => key !== 'send').sort());
  if (snapshot === nextSnapshot && renderedBusy === nextBusy) return;
  snapshot = nextSnapshot;
  renderedBusy = nextBusy;
  taskView++;
  discussionButtons = [];
  const entries = messages.map(message => article(`${message.role} · ${message.status}\n${message.content}`));
  for (const proposal of proposals) {
    const element = article(`委派提案 · ${proposal.status}\n接收人：${proposal.owner}\n将共享的完整任务摘要（原文）：\n${proposal.summary}\n\n确认后，仅将所示接收人与任务摘要用于跨人委派；对方会看到发起人身份和任务记录，不会收到你的完整私聊。${proposal.task_id ? '\n任务编号：' + proposal.task_id : ''}`);
    if (proposal.status === 'pending') {
      element.append(article('尚未发送给对方，也未创建任务。请核对接收人及摘要后确认。'));
      addActions(element, context, 'proposals', proposal, [['confirm', '确认共享并委派'], ['reject', '拒绝，不共享']]);
    }
    entries.push(element);
  }
  for (const task of tasks) {
    const element = article(`任务 · ${task.status}\n任务编号：${task.id} · 版本：${task.revision}\n${task.requester} → ${task.owner}\n${task.summary}${task.result ? '\n交付：' + task.result : ''}`);
    const actions = [];
    if (task.owner === context.user && task.status === 'awaiting_owner') actions.push(['accept', '接单']);
    if (task.owner === context.user && task.status === 'working') actions.push(['deliver', '提交人工交付']);
    if (task.requester === context.user && task.status === 'ready_for_acceptance') actions.push(['approve', '验收'], ['return', '退回']);
    if (!['completed', 'canceled'].includes(task.status)) actions.push(['cancel', '取消']);
    addActions(element, context, 'tasks', task, actions);
    addDiscussion(element, context, task);
    workerUI.attach(element, context, task);
    entries.push(element);
  }
  profileUI.append(entries);
  workerUI.append(entries);
  $('conversation').replaceChildren(...entries);
  renderTaskContext();

}

async function enterRoom(context) {
  const me = await api(context, 'me');
  if (!current(context)) return;
  if (context.browserMode && me.user !== context.user) throw Error('登录身份已变更，请重新加载页面');
  user = me.user;
  context.user = user;
  restoreTurn(context);
  $('identity').textContent = `${user} 的管家 · 运行时 ${me.runtime} · 已验证身份`;
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
      status(receipt.status === 'pending' ? '服务端仍在处理。稍后可再次核对，未重发消息。' : '服务端无法确认上次处理结果。未重发消息。');
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
  status('已开始新尝试。请核对输入内容，如需任务上下文请重新选择，再点击发送；将使用新的请求编号。上次操作不会被撤销。');
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
      if (selectedTask && (selectedTask.invalidated || !visibleTasks.has(selectedTask.id)
          || taskSignature(visibleTasks.get(selectedTask.id)) !== taskSignature(selectedTask))) {
        selectedTask.invalidated = true;
        status('任务上下文已失效，未发送消息。请重新选择任务，或清除任务上下文后再发送。');
        return;
      }
      chatTurn = { text, id: crypto.randomUUID(), status: 'pending', selectedTask };
      selectedTask = null;
      pendingTurn = chatTurn;
      rememberTurn(context);
      renderTurn();
      const response = await postChat(context, { message: text, request_id: chatTurn.id,
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
      if (rejectedSelection) {
        // These two server codes guarantee rejection before receipt reservation
        // and provider dispatch. Generic conflicts/not-found stay uncertain.
        selectedTask = { ...chatTurn.selectedTask, invalidated: true };
        forgetTurn();
        status(error.message + '。服务端尚未处理本次消息，任务上下文已失效。请重新选择任务，或清除后再发送。未自动重发。');
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
