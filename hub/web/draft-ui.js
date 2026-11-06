/* Private text drafting only. Delivery remains a separate human task action. */
function createDraftUI({ api, current, article, conversation }) {
  const storageKey = 'agenct.private-draft-request.v1';
  const codes = new Set(['draft_task_stale_before_reservation', 'draft_task_state_before_reservation',
    'draft_role_unavailable_before_reservation', 'draft_busy_before_reservation']);
  let generation = 0, card = null, tasks = new Map(), edit = null, editors = [];
  const key = value => typeof value === 'string' && /^[A-Za-z0-9_-]{1,128}$/.test(value);
  function reset({ preserveRecovery = false } = {}) {
    generation++; card = null; tasks = new Map(); edit = null; editors = [];
    if (!preserveRecovery) { try { sessionStorage.removeItem(storageKey); } catch {} }
  }
  function saved(context) {
    try {
      const data = JSON.parse(sessionStorage.getItem(storageKey));
      if (!data) return null;
      if (data.owner !== context.user || data.mode !== context.browserMode) {
        sessionStorage.removeItem(storageKey); return null;
      }
      return key(data.task) && key(data.id) ? data : null;
    } catch { return null; }
  }
  function update(context, rows) {
    saved(context);
    tasks = new Map(rows.filter(t => t.owner === context.user).map(t => [t.id, t]));
    if (edit && (tasks.get(edit.task)?.revision !== edit.revision || tasks.get(edit.task)?.status !== 'working')) {
      edit = null;
      if (card) card.append(article('任务已变更，草稿不可用于当前交付。请重新读取并人工审阅。'));
    }
  }
  function reviewedText(task) {
    return edit && edit.task === task.id && edit.revision === task.revision && task.status === 'working'
      ? edit.input.value : '';
  }
  function append(entries) { if (card) entries.push(card); }
  function attach(element, context, task) {
    if (task.owner !== context.user) return;
    const b = document.createElement('button'); b.type = 'button'; b.textContent = '我的私密交付草稿';
    b.onclick = () => { if (current(context) && tasks.has(task.id)) return open(context, task.id); };
    element.append(b);
  }
  async function open(context, taskId) {
    if (editors.some(({input, original}) => input.value !== original)
        && !confirm('重新打开会丢弃此卡中尚未交付的人工编辑。确认丢弃并重新读取？')) return;
    editors = [];
    const version = ++generation;
    edit = null;
    const panel = article('私密文本草稿：不会执行、交付或验收。只发送本任务的共享摘要、委派人确认附带的共享材料、交付结果及所选角色，不发送任何私聊；长文本可能截断。');
    if (card) card.replaceChildren(article('已关闭旧草稿视图。'));
    card = panel; conversation().append(panel); panel.scrollIntoView?.({ block: 'nearest' });
    const openedRevision = tasks.get(taskId)?.revision;
    let working = false, view = 0, unresolved = saved(context), role = null, roleInvalid = false;
    if (unresolved && unresolved.task !== taskId) {
      panel.append(article('另一任务有未核对的起草请求，请先打开该任务恢复。')); return;
    }
    const valid = () => current(context) && generation === version && tasks.has(taskId);
    const route = 'tasks/' + encodeURIComponent(taskId) + '/drafts';
    function button(label, fn) {
      const stamp = view, b = document.createElement('button'); b.type = 'button'; b.textContent = label;
      // The panel's main actions read as primary; the rest stay quiet.
      if (/^让我的 Agent 起草|^使用此草稿填入人工交付/.test(label)) b.className = 'primary';
      b.onclick = async () => { if (valid() && stamp === view && !working) await fn(); };
      panel.append(b); return b;
    }
    async function request(path, method = 'GET', body) {
      const abort = new AbortController(), stop = () => abort.abort();
      context.signal.addEventListener('abort', stop, { once: true });
      let timer;
      try {
        return await Promise.race([api({ ...context, signal: abort.signal }, path, method, body),
          new Promise((_, reject) => { timer = setTimeout(() => { stop(); reject(Error('等待超时；不代表远端取消')); }, method === 'POST' ? 90000 : 15000); })]);
      } finally { clearTimeout(timer); context.signal.removeEventListener('abort', stop); }
    }
    function display(receipt, expected) {
      if (receipt.kind !== 'draft_delivery' || receipt.task_id !== taskId || !key(receipt.request_id)
          || (expected && receipt.request_id !== expected) || !['pending','uncertain','complete'].includes(receipt.status)) throw Error('草稿回执不匹配');
      if (receipt.status !== 'complete') {
        panel.append(article(receipt.status === 'pending' ? '服务端已记录，尚未确认完成。' : '结果未知，不会自动重发。'));
        return false;
      }
      const response = receipt.response, draft = response?.draft;
      if (!draft || typeof draft.content !== 'string' || !draft.content.trim()
          || new TextEncoder().encode(draft.content).length > 8000
          || !Number.isSafeInteger(draft.task_revision) || typeof receipt.current_stale !== 'boolean'
          || response.task_context?.id !== taskId) throw Error('草稿响应无效');
      const stale = receipt.current_stale || tasks.get(taskId).revision !== draft.task_revision || tasks.get(taskId).status !== 'working';
      panel.append(article(`模型生成文本，未经验证。任务版本 ${draft.task_revision}；角色配置版本 ${response.runtime_profile?.revision ?? '未知'}。`
        + (stale ? ' 已过期，仅供查看，不能直接交付。' : ' 请编辑审阅，再点击任务上的「提交人工交付」并确认。')));
      const truncated = [
        response.task_context.summary_truncated && '共享摘要',
        response.task_context.details_truncated && '共享材料',
        response.task_context.result_truncated && '交付结果'
      ].filter(Boolean);
      if (truncated.length) {
        panel.append(article(`起草时${truncated.join('、')}已截断；模型未读取完整任务，请核对遗漏后再交付。`));
      }
      const input = document.createElement('textarea'); input.value = draft.content;
      input.setAttribute('aria-label', '私密交付草稿编辑'); panel.append(input);
      editors.push({ input, original: draft.content });
      if (!stale) button('使用此草稿填入人工交付', () => {
        edit = { task: taskId, revision: draft.task_revision, input };
        panel.append(article('已选择编辑稿；尚未交付。请点击任务上的人工交付按钮审阅并确认。'));
      });
      return true;
    }
    function controls() {
      button('重新读取私密草稿', () => open(context, taskId));
      if (unresolved) {
        button('核对起草请求', async () => {
          working = true;
          try {
            const receipt = await request(route + '/turns/' + encodeURIComponent(unresolved.id));
            if (!valid()) return;
            if (display(receipt, unresolved.id)) { sessionStorage.removeItem(storageKey); unresolved = null; }
          } catch (error) { if (valid()) panel.append(article('仍未核实：' + error.message)); }
          finally { if (valid()) { working = false; controls(); } }
        });
        button('明确开始新的起草尝试', () => {
          if (!confirm('原请求可能已调用模型并产生费用。新请求可能重复工作。确认仅允许下一次手动起草？')) return;
          sessionStorage.removeItem(storageKey); unresolved = null; controls();
        });
        return;
      }
      if (tasks.get(taskId)?.status !== 'working') return;
      button('明确使用默认管家起草', () => { role = null; roleInvalid = false; panel.append(article('已明确选择默认管家，尚未起草。')); });
      button('选择起草角色（可选）', async () => {
        working = true;
        try {
          const profile = await request('profile');
          if (!valid()) return;
          if (!Number.isSafeInteger(profile.revision) || profile.revision < 0 || !Array.isArray(profile.agents)) throw Error('角色响应无效');
          for (const agent of profile.agents.filter(a => a.enabled && key(a.id))) {
            button('起草角色：' + agent.name, () => { role = { agent_id: agent.id, expected_profile_revision: profile.revision }; roleInvalid = false; panel.append(article('已选择 ' + agent.name + '，尚未起草。')); });
          }
        } catch (error) { if (valid()) panel.append(article(error.message)); }
        finally { if (valid()) working = false; }
      });
      button('让我的 Agent 起草（不交付）', async () => {
        if (unresolved) return;
        if (roleInvalid) { panel.append(article('所选角色已失效，请重新选择或明确改用默认管家。')); return; }
        const task = tasks.get(taskId);
        if (task?.status !== 'working' || task.revision !== openedRevision) { panel.append(article('任务版本已改变，请重新打开并审阅。')); return; }
        const id = crypto.randomUUID(), body = { request_id: id, expected_revision: task.revision, ...role };
        unresolved = { owner: context.user, mode: context.browserMode, task: taskId, id };
        try { sessionStorage.setItem(storageKey, JSON.stringify(unresolved)); }
        catch { unresolved = null; panel.append(article('无法保存恢复编号，未发送。')); return; }
        view++; working = true;
        panel.append(article('正在发送，尚未收到服务端回执。'));
        try {
          const receipt = await request(route, 'POST', body);
          if (!valid()) return;
          if (display(receipt, id)) { sessionStorage.removeItem(storageKey); unresolved = null; }
        } catch (error) {
          if (!valid()) return;
          if (error.httpStatus === 409 && codes.has(error.code)) {
            sessionStorage.removeItem(storageKey); unresolved = null;
            if (error.code === 'draft_role_unavailable_before_reservation') roleInvalid = true;
            panel.append(article('请求未登记、未调用模型。请重新核对任务和角色后手动操作。'));
          } else panel.append(article('起草结果未知：' + error.message + '。只核对，不自动重发。'));
        } finally { if (valid()) { working = false; controls(); } }
      });
    }
    function nextPage(cursor) {
      if (!key(cursor)) throw Error('分页游标无效');
      button('读取更多私密草稿', async () => {
        working = true;
        try {
          const page = await request(route + '?limit=20&after=' + encodeURIComponent(cursor));
          if (!valid()) return;
          if (page.kind !== 'draft_delivery' || page.task_id !== taskId || !Array.isArray(page.items)) throw Error('列表响应无效');
          for (const receipt of page.items) {
            display(receipt);
            if (!unresolved && ['pending', 'uncertain'].includes(receipt.status)) {
              unresolved = { owner: context.user, mode: context.browserMode, task: taskId, id: receipt.request_id };
              sessionStorage.setItem(storageKey, JSON.stringify(unresolved));
            }
          }
          if (page.next_cursor && page.next_cursor !== cursor) nextPage(page.next_cursor);
        } catch (error) { if (valid()) panel.append(article('读取失败：' + error.message)); }
        finally { if (valid()) { working = false; controls(); } }
      });
    }
    working = true;
    try {
      const page = await request(route + '?limit=20');
      if (!valid()) return;
      if (page.kind !== 'draft_delivery' || page.task_id !== taskId || !Array.isArray(page.items)) throw Error('列表响应无效');
      for (const receipt of page.items) {
        display(receipt);
        if (!unresolved && ['pending', 'uncertain'].includes(receipt.status)) {
          unresolved = { owner: context.user, mode: context.browserMode, task: taskId, id: receipt.request_id };
          sessionStorage.setItem(storageKey, JSON.stringify(unresolved));
        }
      }
      if (page.next_cursor) nextPage(page.next_cursor);
    } catch (error) { if (valid()) panel.append(article('读取失败：' + error.message)); }
    finally { if (valid()) { working = false; controls(); } }
  }
  return { reset, update, append, attach, reviewedText };
}
