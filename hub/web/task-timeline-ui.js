/* Participant-authorized shared history; no draft or model endpoint calls. */
function createTaskTimelineUI({ api, current, article, conversation }) {
  let generation = 0, panel = null, selected = null, tasks = new Map();
  const signature = t => JSON.stringify([t.id, t.owner, t.requester, t.revision, t.status]);
  function reset() { generation++; panel = null; selected = null; tasks = new Map(); }
  function update(context, rows) {
    tasks = new Map(rows.filter(t => t.owner === context.user || t.requester === context.user).map(t => [t.id, t]));
    if (selected && (!tasks.has(selected.id) || signature(tasks.get(selected.id)) !== signature(selected))) {
      generation++; selected = null;
      if (panel) panel.replaceChildren(article('任务已更新或不可见，旧时间线已隐藏。请从任务卡重新读取。'));
    }
  }
  function append(entries) { if (panel) entries.push(panel); }
  function attach(element, context, task) {
    if (task.owner !== context.user && task.requester !== context.user) return;
    const button = document.createElement('button'); button.type = 'button'; button.textContent = '查看共享时间线';
    button.onclick = async () => {
      if (!current(context) || !tasks.has(task.id) || signature(tasks.get(task.id)) !== signature(task)) return;
      await open(context, task);
    };
    element.append(button);
  }
  async function open(context, task) {
    const version = ++generation;
    selected = { ...task };
    if (panel) panel.replaceChildren(article('旧时间线已关闭。'));
    const card = article('正在读取共享任务记录…'); panel = card; conversation(task.id).append(card); card.scrollIntoView?.({ block: 'nearest' });
    const valid = () => current(context) && version === generation && tasks.has(task.id)
      && signature(tasks.get(task.id)) === signature(task);
    const abort = new AbortController(), stop = () => abort.abort();
    context.signal.addEventListener('abort', stop, { once: true });
    let timer;
    try {
      const detail = await Promise.race([
        api({ ...context, signal: abort.signal }, 'tasks/' + encodeURIComponent(task.id)),
        new Promise((_, reject) => { timer = setTimeout(() => { stop(); reject(Error('读取超时')); }, 15000); })
      ]);
      if (!valid()) return;
      if (!detail.task || signature(detail.task) !== signature(task)) throw Error('任务已改变，请刷新任务后重新读取');
      const timelineData = detail.timeline;
      if (!timelineData || typeof timelineData !== 'object') throw Error('缺少共享时间线');
      const allowed = new Set([task.owner, task.requester]);
      if (!Array.isArray(timelineData.events) || timelineData.events.some(e => !allowed.has(e.actor))
          || !Array.isArray(timelineData.deliveries) || timelineData.deliveries.some(d => d.author !== task.owner)) throw Error('共享记录响应无效');
      const timeline = prepareTaskTimeline(detail.task, timelineData.events, timelineData.deliveries, timelineData.audit);
      card.replaceChildren(article(`共享时间线 · 任务 ${task.id} · 版本 ${task.revision}`), article(timeline.notice));
      for (const text of timeline.rows) card.append(article(text));
      for (const text of timeline.legacyDeliveries) card.append(article(text));
      if (!timeline.rows.length && !timeline.legacyDeliveries.length) card.append(article('没有可展示的共享历史记录。'));
      card.append(article('这里只展示已经提交的交付及任务状态变更，不展示私密草稿、私聊或模型推理。'));
    } catch (error) {
      if (valid()) card.replaceChildren(article('时间线未加载：' + error.message + '。未更改任务，可稍后重新读取。'));
    } finally {
      clearTimeout(timer); context.signal.removeEventListener('abort', stop); abort.abort();
      if (valid()) {
        const retry = document.createElement('button'); retry.type = 'button'; retry.textContent = '重新读取共享时间线';
        retry.onclick = () => { if (valid()) return open(context, task); };
        card.append(retry);
      }
    }
  }
  return { reset, update, append, attach };
}
