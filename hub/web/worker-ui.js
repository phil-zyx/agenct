/* Owner-only queue receipts inside chat. Never starts execution or publication. */
function createWorkerUI({ api, current, article, conversation }) {
  let generation = 0, card = null, active = null, saving = false;
  let tasks = new Map(), pending = new Map();
  const notice = '仅登记待办队列，不运行代码、不调用模型、不发布 PR/MR，也不会自动交付。执行器保持禁用。';
  const signature = task => JSON.stringify([task.id, task.owner, task.revision, task.status]);
  function reset() { generation++; card = null; active = null; saving = false; tasks = new Map(); pending = new Map(); }
  function update(context, rows) {
    tasks = new Map(rows.filter(t => t.owner === context.user).map(t => [t.id, t]));
    if (active && (!tasks.has(active.id) || signature(tasks.get(active.id)) !== signature(active))) {
      generation++; active = null; saving = false;
      if (card) card.replaceChildren(article('任务状态已变更，旧队列操作已失效。请从任务重新查看。' + notice));
    }
  }
  function append(entries) { if (card) entries.push(card); }
  function attach(element, context, task) {
    if (task.owner !== context.user) return;
    const b = document.createElement('button'); b.type = 'button'; b.textContent = '查看待办队列（不执行）';
    b.onclick = async () => {
      if (current(context) && !saving && tasks.has(task.id) && signature(tasks.get(task.id)) === signature(task)) await open(context, task);
    };
    element.append(b);
  }
  async function open(context, task) {
    const version = ++generation;
    let view = 0;
    active = { ...task }; saving = false;
    if (card) card.replaceChildren(article('旧队列卡已关闭。'));
    const panel = article('正在读取待办队列…'); card = panel; conversation(task.id).append(panel); panel.scrollIntoView?.({ block: 'nearest' });
    const valid = () => current(context) && version === generation && tasks.has(task.id) && signature(tasks.get(task.id)) === signature(task);
    function button(text, action) {
      const viewAtCreation = view;
      const b = document.createElement('button'); b.type = 'button'; b.textContent = text;
      b.onclick = async () => { if (valid() && viewAtCreation === view && !saving && !b.disabled) await action(); };
      return b;
    }
    function close() { generation++; active = null; card = null; panel.replaceChildren(article('队列卡已关闭，未发送新的请求。')); }
    const route = 'tasks/' + encodeURIComponent(task.id) + '/worker';
    async function mutate(path, body, success, key) {
      saving = true; view++;
      panel.replaceChildren(article('正在提交，请勿重复操作。' + notice));
      try {
        const response = await api(context, path, 'POST', body);
        if (!valid()) return;
        if (response.execution_enabled !== false || response.publication_enabled !== false) throw Error('服务端返回模式不符');
        if (key) {
          const job = response.job;
          if (!job || job.owner !== context.user || !job.request
              || job.request.task_id !== task.id || job.request.task_revision !== body.expected_revision
              || job.request.repository !== body.repository || job.request.base_commit !== body.base_commit
              || job.request.request_id !== body.request_id || !['queued','cancelled','failed'].includes(job.status)) {
            throw Error('入队回执与已确认请求不符');
          }
          pending.delete(key);
          success = job.status === 'queued' ? '已登记队列，尚未执行。'
            : `原请求已结束（${job.status}），未重新入队。重新读取后可单独确认新请求。`;
        } else if (response.status !== 'cancelled' || path !== route + '/' + encodeURIComponent(response.id) + '/cancel') {
          throw Error('取消回执与请求不符');
        }
        panel.replaceChildren(article(success + '\n' + notice));
      } catch (error) {
        if (!valid()) return;
        panel.replaceChildren(article('请求结果未确认：' + error.message + '。请重新读取核对，不会自动重试。'));
      } finally {
        if (valid()) {
          saving = false;
          panel.append(button('重新读取队列', () => open(context, task)));
          panel.append(button('关闭队列卡', close));
        }
      }
    }
    let data;
    try { data = await api(context, route); }
    catch (error) {
      if (valid()) {
        panel.replaceChildren(article('读取队列失败：' + error.message));
        panel.append(button('重新读取队列', () => open(context, task)));
        panel.append(button('关闭队列卡', close));
      }
      return;
    }
    if (!valid()) return;
    panel.replaceChildren(article('任务待办队列 · ' + task.summary + '\n' + notice));
    if (data.execution_enabled !== false || data.publication_enabled !== false || !Array.isArray(data.jobs)) {
      panel.append(article('服务端模式或返回数据不符合队列预览要求，操作已禁用。'));
      panel.append(button('关闭队列卡', close)); return;
    }
    // Reconcile only matching owner/task/scope receipts. A known receipt resolves
    // an uncertain submission; terminal receipts require separate new consent.
    for (const job of data.jobs) {
      if (job.owner === context.user && job.request?.task_id === task.id) {
        const key = JSON.stringify([task.id, job.request.task_revision, job.request.repository, job.request.base_commit]);
        if (pending.get(key) === job.request.request_id) pending.delete(key);
      }
      const row = article(`队列记录：${job.id}\n状态：${job.status} · 尝试次数：${job.attempt}\n仓库：${job.request.repository}\n基线：${job.request.base_commit}`);
      if (['queued', 'running', 'review_ready'].includes(job.status)) {
        row.append(button('取消此队列记录', () => mutate(route + '/' + encodeURIComponent(job.id) + '/cancel', {}, '已取消此队列记录。')));
      }
      panel.append(row);
    }
    const scope = data.scope;
    const matched = scope && task.status === 'working' && scope.task_id === task.id && scope.owner === context.user && scope.task_revision === task.revision;
    if (matched && !data.jobs.some(job => ['queued', 'running', 'review_ready'].includes(job.status))) {
      // Preserve this key after an uncertain POST; rereads never silently create a new request.
      const key = JSON.stringify([task.id, task.revision, scope.repository, scope.base_commit]);
      const reviewed = { expected_revision: task.revision, repository: scope.repository, base_commit: scope.base_commit };
      panel.append(article(`请核对仅入队范围：\n任务：${task.id}\n任务修订：${task.revision}\n仓库：${scope.repository}\n不可变基线：${scope.base_commit}\n${notice}`));
      const terminal = data.jobs.some(job => ['cancelled', 'failed'].includes(job.status));
      const unresolved = pending.has(key);
      if (terminal && !unresolved) panel.append(article('已有请求结束。下面的确认会创建一个独立的新请求，不会恢复旧记录。'));
      if (unresolved) panel.append(article('上次请求结果尚未查明，确认将沿用原请求编号，不会创建新的重复请求。'));
      panel.append(button(terminal && !unresolved ? '确认新请求仅入队，不执行' : '确认仅入队，不执行', () => {
        if (!pending.has(key)) pending.set(key, crypto.randomUUID());
        return mutate(route, { ...reviewed, request_id: pending.get(key), confirm_queue_only: true }, '已登记队列，尚未执行。', key);
      }));
    } else if (!matched) panel.append(article('当前任务没有可用的服务端授权范围，不能入队。'));
    panel.append(button('重新读取队列', () => open(context, task)));
    panel.append(button('关闭队列卡', close));
  }
  return { reset, update, attach, append };
}
