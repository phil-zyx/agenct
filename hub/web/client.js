let token = '', user = '', epoch = 0, refreshVersion = 0;
let controller = new AbortController(), pendingSend = null;
let pollTimer = null, pollDelay = 5000, refreshFlight = null, snapshot = '', renderedBusy = '';
const POLL_INTERVAL = 5000, MAX_POLL_DELAY = 60000, READ_TIMEOUT = 15000;
const busy = new Set();
const $ = id => document.getElementById(id);
const status = text => { $('status').textContent = text; };
const session = () => ({ epoch, token, user, signal: controller.signal });
const current = context => context.epoch === epoch && context.token === token && !!token;

async function api(context, path, method = 'GET', body) {
  if (!current(context)) throw Error('身份已变更，请重新操作');
  const response = await fetch('/api/' + path, {
    method,
    signal: context.signal,
    headers: { Authorization: 'Bearer ' + context.token, 'Content-Type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined
  });
  const data = await response.json();
  if (!current(context)) throw Error('身份已变更，请重新操作');
  if (!response.ok) {
    const error = Error(data.error || response.status);
    error.httpStatus = response.status;
    throw error;
  }
  return data;
}

function article(text) {
  const element = document.createElement('article');
  element.textContent = text;
  return element;
}

const profileUI = createProfileUI({ api, current, article, conversation: () => $('conversation'), status });

function resetIdentity() {
  clearTimeout(pollTimer);
  pollTimer = null;
  refreshFlight = null;
  snapshot = '';
  renderedBusy = '';
  pollDelay = POLL_INTERVAL;
  $('last-refresh').textContent = '';
  $('sync-notice').textContent = '';
  profileUI.reset();
  controller.abort();
  controller = new AbortController();
  epoch++;
  refreshVersion++;
  token = '';
  user = '';
  pendingSend = null;
  busy.clear();
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
      resetIdentity();
      status('登录已失效，请重新输入访问令牌。');
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
  if (!document.hidden && token && user) void poll(session());
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
    const element = article(`任务 · ${task.status}\n${task.requester} → ${task.owner}\n${task.summary}${task.result ? '\n交付：' + task.result : ''}`);
    const actions = [];
    if (task.owner === context.user && task.status === 'awaiting_owner') actions.push(['accept', '接单']);
    if (task.owner === context.user && task.status === 'working') actions.push(['deliver', '提交人工交付']);
    if (task.requester === context.user && task.status === 'ready_for_acceptance') actions.push(['approve', '验收'], ['return', '退回']);
    if (!['completed', 'canceled'].includes(task.status)) actions.push(['cancel', '取消']);
    addActions(element, context, 'tasks', task, actions);
    entries.push(element);
  }
  profileUI.append(entries);
  $('conversation').replaceChildren(...entries);

}

$('enter').onclick = async () => {
  const enteredToken = $('token').value;
  $('token').value = '';
  resetIdentity();
  token = enteredToken;
  const context = session();
  status('');
  try {
    const me = await api(context, 'me');
    if (!current(context)) return;
    user = me.user;
    context.user = user;
    $('identity').textContent = `${user} 的管家 · 运行时 ${me.runtime} · 已验证身份`;
    $('login').classList.add('hide');
    $('room').classList.remove('hide');
    await refresh(context);
    schedulePoll(context);
  } catch (error) {
    if (context.epoch !== epoch) return;
    resetIdentity();
    status(error.message);
  }
};
$('logout').onclick = () => {
  resetIdentity();
  $('last-refresh').textContent = '';
  status('已清除本页身份和显示内容。已发出的服务端操作不会因退出而撤销。');
};
$('refresh').onclick = async () => {
  const context = session();
  try { await refresh(context); }
  catch (error) { if (current(context)) status(error.message); }
};
$('form').onsubmit = async event => {
  event.preventDefault();
  const context = session();
  const text = $('text').value.trim();
  if (!text || !current(context) || busy.has('send')) return;
  busy.add('send');
  $('send').disabled = true;
  status('');
  // Reuse the request ID after an uncertain send of the unchanged draft.
  if (!pendingSend || pendingSend.text !== text) pendingSend = { text, id: crypto.randomUUID() };
  const requestId = pendingSend.id;
  try {
    if (await profileUI.handle(context, text)) {
      if (current(context)) {
        if ($('text').value.trim() === text) $('text').value = '';
        pendingSend = null;
      }
      return;
    }
    const delegation = text.match(/^\/delegate\s+([a-zA-Z0-9_-]+)\s+([\s\S]+)$/);
    await api(context, delegation ? 'proposals' : 'messages', 'POST', delegation
      ? { owner: delegation[1], summary: delegation[2], request_id: requestId }
      : { message: text, request_id: requestId });
    if (!current(context)) return;
    pendingSend = null;
    if ($('text').value.trim() === text) $('text').value = '';
    await refresh(context);
  } catch (error) {
    if (!current(context)) return;
    status(error.message + '。发送结果可能需要核对，请刷新查看。');
    await refresh(context).catch(() => {});
  } finally {
    if (current(context)) {
      busy.delete('send');
      $('send').disabled = false;
    }
  }
};
