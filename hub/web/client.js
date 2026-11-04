let token = '', user = '', epoch = 0, refreshVersion = 0;
let controller = new AbortController(), pendingSend = null;
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
  if (!response.ok) throw Error(data.error || response.status);
  return data;
}

function article(text) {
  const element = document.createElement('article');
  element.textContent = text;
  return element;
}

function resetIdentity() {
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

async function refresh(context = session()) {
  if (!current(context)) return;
  const version = ++refreshVersion;
  const [messages, proposals, tasks] = await Promise.all([
    api(context, 'messages'), api(context, 'proposals'), api(context, 'tasks')
  ]);
  if (!current(context) || version !== refreshVersion) return;
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
  $('conversation').replaceChildren(...entries);
  $('last-refresh').textContent = '本账号服务端状态快照：' + new Date().toLocaleTimeString() + '。对方操作后请手动刷新；没有实时推送。';
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
    $('identity').textContent = `${user} 的管家 · 运行时 ${me.runtime} · 开发令牌认证`;
    $('login').classList.add('hide');
    $('room').classList.remove('hide');
    await refresh(context);
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
