const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { webcrypto } = require('node:crypto');

class Element {
  // Like the DOM: textContent concatenates descendants, and setting it removes children.
  get textContent() { return this.ownText + this.children.map(child => child.textContent).join(''); }
  set textContent(value) { this.ownText = String(value); this.children = []; }
  constructor() {
    this.value = '';
    this.ownText = '';
    this.children = [];
    this.disabled = false;
    const classes = new Set();
    this.classList = { add: value => classes.add(value), remove: value => classes.delete(value), contains: value => classes.has(value) };
  }
  focus() { this.focused = true; }
  setAttribute() {}
  append(...elements) { this.children.push(...elements); }
  replaceChildren(...elements) { this.children = elements; }
}

function fixture(auth = { enabled: false }, settings = {}) {
  const elements = new Map();
  const calls = [];
  const waiting = [];
  let hold = false;
  let failReads = 0;
  let stallReads = false, stallPosts = false;
  let revision = 0;
  let postTurnStatus = 'complete', checkTurnStatus = settings.receipt || 'complete', turnFailure = 0;
  let tasks = settings.tasks || [];
  let repositoryPreview = null;
  const repositoryTurns = settings.repositoryTurns || new Map();
  const turnContexts = settings.turnContexts || new Map();
  let responseContextOverride, responseProfileOverride;
  const defaultProfile = { revision: 0, agents: [{ id: 'butler', name: '管家', role: 'coordinator', model_ref: 'server-default', enabled: true }] };
  let profile = settings.profile || defaultProfile;
  const turnProfiles = settings.turnProfiles || new Map();
  let postFailure = null;
  let confirmation = false;
  const confirmations = [];
  const storage = settings.storage || new Map();
  const timers = new Map();
  const listeners = new Map();
  let timerId = 0;
  const document = {
    hidden: false,
    getElementById: id => $(id),
    createElement: tag => { const node = new Element(); node.tagName = tag; return node; },
    addEventListener: (name, callback) => listeners.set(name, callback)
  };
  let failNextPost = false;
  const $ = id => {
    if (!elements.has(id)) elements.set(id, new Element());
    return elements.get(id);
  };
  const context = vm.createContext({
    document,
    innerWidth: settings.width ?? 1200,
    setTimeout: (callback, delay) => { const id = ++timerId; timers.set(id, { callback, delay }); return id; },
    clearTimeout: id => timers.delete(id),
    AbortController,
    TextEncoder, TextDecoder, btoa,
    crypto: webcrypto,
    Date,
    prompt: (label, value) => settings.prompt ? settings.prompt(label, value) : '',
    confirm: text => { confirmations.push(text); return confirmation; },
    sessionStorage: {
      getItem: key => { if (settings.storageFailure) throw Error('Storage unavailable'); return storage.get(key) || null; },
      setItem: (key, value) => { if (settings.storageFailure) throw Error('Storage unavailable'); storage.set(key, value); },
      removeItem: key => { if (settings.storageFailure) throw Error('Storage unavailable'); storage.delete(key); }
    },
    localStorage: { getItem: key => settings.preferences?.get(key) || null, setItem: (key, value) => settings.preferences?.set(key, value) },
    window: { location: { assign: url => calls.push({ redirect: url }) } },
    fetch: async (url, options) => {
      calls.push({ url, options });
      if (url === '/auth/session') {
        if (settings.holdSession) await new Promise(resolve => waiting.push(resolve));
        return { ok: !settings.sessionFailure, json: async () => auth };
      }
      if (url === '/auth/logout') {
        if (settings.logoutFailure) throw Error('Network outcome unknown');
        return { ok: true, status: 204 };
      }
      // Capture the context at dispatch, including an eventually lost response.
      if (url === '/api/messages' && options.method === 'POST') {
        const body = JSON.parse(options.body), selected = body.selected_task;
        repositoryTurns.set(body.request_id, /^\/repo(?:-search)? /.test(body.message) ? repositoryPreview : null);
        const agent = profile.agents?.find(agent => body.agent_id ? agent.id === body.agent_id : agent.role === 'coordinator');
        turnProfiles.set(body.request_id, agent ? { revision: profile.revision, agent_id: agent.id,
          role: agent.role, model_ref: agent.model_ref } : null);
        const task = selected && tasks.find(task => task.id === selected.task_id);
        turnContexts.set(body.request_id, task ? { ...task, summary_truncated: false, result_truncated: false } : null);
      }
      // Intentionally ignore abort to also exercise responses already in flight.
      if (hold) await new Promise(resolve => waiting.push(resolve));
      if (failNextPost && options.method === 'POST') {
        failNextPost = false;
        throw Error('Simulated uncertain network outcome');
      }
      if (postFailure && url === '/api/messages' && options.method === 'POST') {
        return { ok: false, status: postFailure.status,
          json: async () => ({ error: 'Selected task request rejected', code: postFailure.code }) };
      }
      if (stallPosts && options.method === 'POST') await new Promise((resolve, reject) => {
        options.signal.addEventListener('abort', () => reject(Error('Send aborted')), { once: true });
      });
      if (stallReads && options.method === 'GET') await new Promise((resolve, reject) => {
        options.signal.addEventListener('abort', () => reject(Error('Read timed out')), { once: true });
      });
      if (failReads && options.method === 'GET') throw Object.assign(Error('Read unavailable'), { httpStatus: failReads });
      const identity = (options.headers.Authorization || auth.user || '').endsWith('bob') ? 'bob' : 'alice';
      let data = url.endsWith('/me') ? { user: identity, runtime: 'mock' } : [];
      if (url.endsWith('/proposals') && options.method === 'GET') {
        data = settings.proposals || [{ id: 1, owner: 'bob', summary: 'Exact <summary>\nsecond line', status: 'pending', revision }];
      }
      if (url === '/api/tasks' && options.method === 'POST') {
        return { ok: false, status: 405, json: async () => ({ error: 'Method Not Allowed' }) };
      }
      if (url === '/api/tasks' && options.method === 'GET') data = tasks;
      if (url === '/api/repositories') data = settings.repositories || [];
      if (url === '/api/repositories/context') {
        const choice = JSON.parse(options.body);
        repositoryPreview = { ...choice, path: choice.path ?? null, query: choice.query ?? null, snapshot_digest: 'b'.repeat(64), read_only: true, text: 'Private repository source', truncated: false };
        data = repositoryPreview;
      }
      if (url === '/api/activity') data = settings.activity || [];
      if (url === '/api/messages' && options.method === 'GET' && settings.messages) data = settings.messages;
      if (url === '/api/messages' && options.method === 'POST') {
        const requestId = JSON.parse(options.body).request_id;
        data = { repository_context: settings.repositoryMismatch ? null : repositoryTurns.get(requestId), reply: postTurnStatus === 'complete' ? 'Saved chat reply' : null,
          runtime_profile: responseProfileOverride === undefined ? turnProfiles.get(requestId) : responseProfileOverride,
          task_context: responseContextOverride === undefined ? turnContexts.get(requestId) : responseContextOverride,
          task_action: settings.taskAction ? settings.taskAction(JSON.parse(options.body)) : null,
          turn: { request_id: requestId, status: postTurnStatus } };
      }
      if (url.startsWith('/api/turns/')) {
        if (turnFailure) return { ok: false, status: turnFailure, json: async () => ({ error: 'Receipt unavailable' }) };
        const requestId = decodeURIComponent(url.split('/').at(-1));
        data = { request_id: requestId, status: checkTurnStatus,
          response: checkTurnStatus === 'complete' ? { task_action: settings.recoveredTaskAction || null, repository_context: settings.repositoryMismatch ? null : repositoryTurns.get(requestId), reply: 'Saved chat reply',
            runtime_profile: responseProfileOverride === undefined ? turnProfiles.get(requestId) : responseProfileOverride,
            task_context: responseContextOverride === undefined ? turnContexts.get(requestId) ?? null : responseContextOverride,
            turn: { request_id: requestId, status: 'complete' } } : null };
      }
      if (url.startsWith('/api/tasks/') && options.method === 'GET' && !url.includes('/drafts') && !url.includes('/worker')) {
        const task = tasks.find(t => url === '/api/tasks/' + t.id);
        if (task) data = { task, timeline: {events: [], deliveries: [], audit: {schema_version:1,legacy_history_incomplete:true,legacy_notice:'Earlier history unknown'}} };
      }
      if (settings.draftPostFailure && url.endsWith('/drafts') && options.method === 'POST') throw Error('Draft outcome unknown');
      if (url.includes('/drafts/turns/') && options.method === 'GET') {
        const task = tasks[0], requestId = decodeURIComponent(url.split('/').pop());
        data = {kind:'draft_delivery',task_id:task.id,request_id:requestId,status:'complete',current_stale:false,
          response:{draft:{id:settings.draftId || 'auto-draft-id',seq:1,content:'Auto draft 需要补充的信息',task_revision:task.revision},task_context:{id:task.id},runtime_profile:{revision:0}}};
      } else if (url.includes('/drafts')) {
        const task = tasks[0];
        data = options.method === 'POST'
          ? {kind:'draft_delivery',task_id:task.id,request_id:JSON.parse(options.body).request_id,status:settings.draftPostStatus || 'complete',current_stale:false,response:{draft:{id:'fixture-generated-draft',content:'Generated draft',task_revision:task.revision},task_context:{id:task.id},runtime_profile:{revision:0}}}
          : {kind:'draft_delivery',task_id:task.id,items:[],next_cursor:null};
      }
      if (url.endsWith('/profile/catalog')) data = { roles: ['coordinator', 'researcher'], models: [{ id: 'server-default' }], max_agents: 8 };
      if (url.endsWith('/profile')) data = profile;
      return { ok: true, json: async () => data };
    }
  });
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'profiles-ui.js'), 'utf8'), context);
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'worker-ui.js'), 'utf8'), context);
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'draft-ui.js'), 'utf8'), context);
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'task-timeline-model.js'), 'utf8'), context);
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'task-timeline-ui.js'), 'utf8'), context);
  for (const name of ['activity-ui.js', 'chat-ui.js', 'task-ui.js', 'repo-ui.js', 'client.js', 'task-chat.js', 'client-chrome.js']) vm.runInContext(fs.readFileSync(path.join(__dirname, name), 'utf8'), context);
  return {
    $, calls, timers, storage, confirmations, turnContexts, turnProfiles, document,
    key(event) { listeners.get('keydown')?.({ preventDefault() {}, ...event }); },
    profile(value) { profile = value; },
    responseProfile(value) { responseProfileOverride = value; },
    role(id) { return $('role-options').children.find(button => button.textContent.endsWith(' · ' + id)); },
    async chooseRole(id = 'research') { await $('choose-role').onclick(); this.role(id).onclick(); },
    tasks(value) { tasks = value; },
    responseContext(value) { responseContextOverride = value; },
    messageFailure(status, code) { postFailure = status ? { status, code } : null; },
    discussion(id) {
      const card = $('tasks').children.find(card => card.taskId === id);
      return card && findButton(card, '讨论此任务');
    },
    confirm(value) { confirmation = value; },
    postTurn(status) { postTurnStatus = status; },
    receipt(status) { checkTurnStatus = status; },
    failTurn(status) { turnFailure = status; },
    ready: () => new Promise(resolve => setImmediate(resolve)),
    change() { revision++; },
    stallReads() { stallReads = true; },
    stallPosts() { stallPosts = true; },
    failReads(status) { failReads = status; },
    visible(value) { document.hidden = !value; listeners.get('visibilitychange')(); },
    async tick() {
      const timer = [...timers.entries()].find(([, value]) => value.delay !== 15000);
      assert(timer, 'Expected a scheduled poll');
      timers.delete(timer[0]);
      await timer[1].callback();
    },
    hold() { hold = true; },
    release() { hold = false; waiting.splice(0).forEach(resolve => resolve()); },
    unhold() { hold = false; },
    failNextPost() { failNextPost = true; },
    async login(identity) { await new Promise(resolve => setImmediate(resolve)); $('token').value = identity; await $('enter').onclick(); },
    async send(text) { $('text').value = text; await $('form').onsubmit({ preventDefault() {} }); }
  };
}

function findButton(node, label) {
  for (const child of node.children) {
    if (child.textContent === label && typeof child.onclick === 'function') return child;
    const found = findButton(child, label);
    if (found) return found;
  }
  return undefined;
}

async function test(name, run) {
  await run();
  console.log('PASS ' + name);
}

const task = (overrides = {}) => ({ id: 'task-one', revision: 3, requester: 'alice', owner: 'bob',
  status: 'working', summary: 'Shared <summary>\nsecond line', result: 'Shared delivery result', ...overrides });
const chatPosts = f => f.calls.filter(call => call.url === '/api/messages' && call.options.method === 'POST');

(async () => {

  await test('work index reveals filtered tasks without changing composer, selection or task state', async () => {
    for (const [filter, status, requester, expected] of [
      ['active', 'completed', 'alice', 'completed'], ['active', 'canceled', 'alice', 'completed'],
      ['completed', 'working', 'alice', 'active'], ['mine', 'working', 'carol', 'active'],
      ['waiting', 'working', 'alice', 'active']
    ]) {
      const t = task({ id: 'target', status, requester, owner: requester === 'carol' ? 'alice' : 'bob' });
      const f = fixture(undefined, { tasks: [t], activity: [{ kind: 'task', id: 'target', title: 'Target', status, latest: {} }] });
      await f.login('alice'); f.$('text').value = 'Unsent private question'; f.discussion('target').onclick();
      const context = f.$('task-context-details').textContent;
      f.$('task-filter').value = filter; f.$('task-filter').onchange();
      const card = f.$('tasks').children.find(e => e.taskId === 'target'); assert(card.classList.contains('hide'));
      const row = f.$('activity').children.find(e => e.className.startsWith('activity-item'));
      row.onclick(); row.onclick();
      assert(!card.classList.contains('hide')); assert(card.classList.contains('flash')); assert(card.focused);
      assert.equal(f.$('task-filter').value, expected);
      assert.equal(f.$('text').value, 'Unsent private question'); assert.equal(f.$('task-context-details').textContent, context);
      assert.equal(f.calls.filter(c => c.options?.method === 'POST').length, 0);
      await f.$('logout').onclick(); await f.login('bob'); f.$('tab-activity').onclick();
      row.onclick(); assert(f.$('tasks').classList.contains('hide'), 'old identity callback cannot navigate');
    }
  });
  await test('work index expands old proposal ancestors and preserves unsent input without dispatch', async () => {
    const f = fixture(undefined, { width: 400,
      proposals: [{ id: 'old', owner: 'bob', summary: 'Old', status: 'pending', revision: 0, created_at: '2020-01-01 00:00:00' }],
      activity: [{ kind: 'proposal', id: 'old', title: 'Old', status: 'pending', latest: {} }] });
    await f.login('alice'); f.$('text').value = 'Keep this draft'; f.$('toggle-tasks').onclick();
    const group = f.$('conversation').children.find(e => e.className === 'old-proposals');
    const target = group.children.find(e => e.proposalId === 'old'); assert.equal(group.open, false);
    const row = f.$('activity').children.find(e => e.className.startsWith('activity-item'));
    row.onclick(); row.onclick();
    assert.equal(group.open, true); assert(target.classList.contains('flash')); assert(target.focused);
    assert(!f.$('room').classList.contains('tasks-open')); assert.equal(f.$('text').value, 'Keep this draft');
    assert.equal(f.calls.filter(c => c.options?.method === 'POST').length, 0);
    await f.$('logout').onclick(); await f.login('bob'); f.$('toggle-tasks').onclick();
    row.onclick(); assert(f.$('room').classList.contains('tasks-open'));
  });

  await test('only routine feedback expires; errors and saved-reply fallback remain keyboard dismissible', async () => {
    const f = fixture(undefined, { tasks: [task()] }); await f.login('alice');
    f.discussion('task-one').onclick(); assert(f.$('status').classList.contains('show'));
    f.failReads(500); await f.$('refresh').onclick();
    assert.match(f.$('status').textContent, /Read unavailable/);
    assert(!f.$('status').classList.contains('show'), 'critical status must not receive fading animation');
    f.$('status').onkeydown({ key: 'Escape', preventDefault() {} });
    assert.equal(f.$('status').textContent, '');
    await f.send('Reply must stay readable');
    assert.match(f.$('status').textContent, /已保存的回复：Saved chat reply/);
    assert(!f.$('status').classList.contains('show'));
    assert.equal(f.storage.size, 0, 'no private reply text retained in storage');
  });
  await test('task invalidation is announced outside collapsed context details', async () => {
    const f = fixture(undefined, { tasks: [task()] }); await f.login('alice');
    f.discussion('task-one').onclick();
    assert.match(f.$('task-context-status').textContent, /已选择讨论任务/);
    f.tasks([task({ revision: 4 })]); await f.$('refresh').onclick();
    assert.match(f.$('task-context-status').textContent, /已失效/);
    f.$('clear-task-context').onclick(); assert.equal(f.$('task-context-status').textContent, '');
    const html = fs.readFileSync(path.join(__dirname, 'index.html'), 'utf8');
    assert.match(html, /id="task-context-status"[^>]*role="status"[^>]*aria-live="polite"/);
    assert(html.indexOf('id="task-context-status"') < html.indexOf('<section id="task-context"'));
    f.discussion('task-one').onclick();
    let writes = 0, content = f.$('task-context-status').textContent;
    Object.defineProperty(f.$('task-context-status'), 'textContent', {get: () => content, set: value => { writes++; content = value; }});
    await f.$('refresh').onclick(); assert.equal(writes, 0, 'unchanged polls do not repeat live announcements');
  });
  await test('each task panel retains its owner across other tool clicks and snapshot rerenders', async () => {
    const all = ['A', 'B', 'C'].map(id => task({ id, summary: 'Task ' + id }));
    const f = fixture(undefined, { tasks: all }); await f.login('bob');
    const card = id => f.$('tasks').children.find(e => e.taskId === id);
    const slot = id => card(id).children.find(e => e.className === 'card-panels');
    await findButton(card('A'), '我的私密交付草稿').onclick();
    const draft = slot('A').children[0]; assert(draft);
    await findButton(draft, '让我的 Agent 起草（不交付）').onclick();
    const walk = e => [e, ...e.children.flatMap(walk)];
    const editor = walk(draft).find(e => e.value === 'Generated draft'); editor.value = 'Private edited A';
    await findButton(card('B'), '查看共享时间线').onclick();
    const timeline = slot('B').children[0]; assert(timeline);
    await findButton(card('C'), '查看待办队列（不执行）').onclick();
    const queue = slot('C').children[0]; assert(queue);
    f.discussion('B').onclick(); f.change(); await f.$('refresh').onclick();
    assert.deepEqual(slot('A').children, [draft]);
    assert.deepEqual(slot('B').children, [timeline]);
    assert.deepEqual(slot('C').children, [queue]);
    assert.equal(editor.value, 'Private edited A');
    assert(!slot('B').textContent.includes('Private edited A'));
    assert.equal(f.calls.filter(c => c.url?.endsWith('/actions')).length, 0);
    await f.$('logout').onclick(); await f.login('alice');
    assert(!f.$('tasks').textContent.includes('私密文本草稿'));
  });
  await test('narrow task discussion and navigation return to chat without dispatch', async () => {
    const f = fixture({ enabled: false }, { width: 400, tasks: [task()] });
    await f.login('alice');
    f.$('toggle-tasks').onclick();
    assert(f.$('room').classList.contains('tasks-open'));
    f.discussion('task-one').onclick();
    assert(!f.$('room').classList.contains('tasks-open'));
    assert(f.$('text').focused);
    assert.match(f.$('task-context-details').textContent, /task-one/);
    f.$('toggle-tasks').onclick(); f.$('open-chat').onclick();
    assert(!f.$('room').classList.contains('tasks-open'));
    f.$('toggle-tasks').onclick(); f.$('open-help').onclick();
    assert(!f.$('room').classList.contains('tasks-open'));
    assert.equal(f.$('help').open, true);
    assert.equal(chatPosts(f).length, 0);
  });
  await test('role shortcut preserves unsent text and task selection without model or mutation', async () => {
    const f = fixture({ enabled: false }, { width: 400, tasks: [task()] });
    await f.login('alice'); f.discussion('task-one').onclick();
    f.$('text').value = 'Unsent private message';
    f.$('toggle-tasks').onclick();
    await f.$('open-agents').onclick();
    assert.equal(f.$('text').value, 'Unsent private message');
    assert.match(f.$('task-context-details').textContent, /task-one/);
    assert(!f.$('room').classList.contains('tasks-open'));
    assert.match(f.$('conversation').textContent, /个人角色设置/);
    assert.equal(f.calls.filter(call => call.options?.method === 'POST' || call.options?.method === 'PUT').length, 0);
  });
  await test('desktop discussion keeps optional task dock and role shortcut respects unresolved send', async () => {
    const f = fixture({ enabled: false }, { width: 1200, tasks: [task()] });
    await f.login('alice'); f.discussion('task-one').onclick();
    assert(f.$('room').classList.contains('tasks-open'));
    f.postTurn('uncertain'); await f.send('Question about this task');
    f.$('text').value = 'Preserve while recovering'; const reads = f.calls.length;
    await f.$('open-agents').onclick();
    assert.equal(f.calls.length, reads);
    assert.equal(f.$('text').value, 'Preserve while recovering');
  });
  await test('role shortcut coalesces reads and late callback cannot affect new identity', async () => {
    const f = fixture(); await f.login('alice'); f.$('text').value = 'Private draft'; f.hold();
    const pending = f.$('open-agents').onclick();
    const reads = f.calls.length;
    await f.$('open-agents').onclick(); assert.equal(f.calls.length, reads);
    await f.$('logout').onclick(); f.release(); await pending;
    assert.equal(f.$('conversation').textContent, '');
    assert.equal(f.$('open-agents').disabled, false);
  });
  const cancelButton = f => {
    const card = f.$('tasks').children.find(card => card.taskId);
    return card && findButton(card, '取消任务');
  };
  const taskPosts = f => f.calls.filter(call => call.url === '/api/tasks/task-one/actions' && call.options.method === 'POST');
  await test('cancellation controls match the approved actor and state matrix', async () => {
    for (const who of ['alice', 'bob']) for (const state of ['awaiting_owner', 'working', 'ready_for_acceptance', 'completed', 'canceled', 'unknown']) {
      const f = fixture({ enabled: false }, { tasks: [task({ status: state })] });
      await f.login(who);
      const allowed = ['awaiting_owner', 'working'].includes(state) || (who === 'alice' && state === 'ready_for_acceptance');
      assert.equal(Boolean(cancelButton(f)), allowed, `${who}/${state}`);
      assert.equal(taskPosts(f).length, 0);
    }
    const f = fixture({ enabled: false }, { tasks: [task({ requester: 'other', owner: 'someone' })] });
    await f.login('alice'); assert.equal(cancelButton(f), undefined);
  });
  const walk = node => [node, ...node.children.flatMap(walk)];
  const editor = f => walk(f.$('tasks')).find(node => node.className === 'action-editor');
  const editorInput = f => walk(editor(f)).find(node => node.rows === 6);
  await test('cancel inline editor requires valid content and preview before any mutation', async () => {
    for (const value of ['', ' \n\t ', 'x'.repeat(501), '🦀'.repeat(501)]) {
      const f = fixture({ enabled: false }, { tasks: [task()] });
      await f.login('alice'); await cancelButton(f).onclick();
      assert.match(editor(f).textContent, /双方/); assert.match(editor(f).textContent, /模型请求可能继续/);
      editorInput(f).value = value; findButton(editor(f), '预览').onclick();
      assert.equal(findButton(editor(f), '确认取消任务').disabled, true);
      assert.equal(taskPosts(f).length, 0);
    }
  });
  await test('only previewed cancellation reason is shared once', async () => {
    const f = fixture({ enabled: false }, { tasks: [task()] }); await f.login('alice');
    await cancelButton(f).onclick(); editorInput(f).value = '  Shared reason <script>literal</script>  ';
    findButton(editor(f), '预览').onclick(); f.hold();
    const submit = findButton(editor(f), '确认取消任务'), pending = submit.onclick();
    await submit.onclick(); assert.equal(taskPosts(f).length, 1);
    assert.deepEqual(JSON.parse(taskPosts(f)[0].options.body), { action: 'cancel', reason: 'Shared reason <script>literal</script>', expected_revision: 3 });
    f.release(); await pending; assert.equal(taskPosts(f).length, 1);
  });
  await test('uncertain cancellation keeps input and is not automatically retried', async () => {
    const f = fixture({ enabled: false }, { tasks: [task()] }); await f.login('alice');
    await cancelButton(f).onclick(); editorInput(f).value = 'Shared reason'; findButton(editor(f), '预览').onclick();
    f.failNextPost(); await findButton(editor(f), '确认取消任务').onclick();
    assert.equal(editorInput(f).value, 'Shared reason');
    assert.equal(taskPosts(f).length, 1); await f.tick(); assert.equal(taskPosts(f).length, 1);
  });

  async function chooseRepository(f, search = false) {
    await f.$('choose-repository').onclick();
    const inputs = walk(f.$('repository-options')).filter(node => node.tagName === 'input');
    inputs[0].value = 'a'.repeat(40); inputs[1].value = search ? 'literal word' : 'README.md';
    if (search) walk(f.$('repository-options')).filter(node => node.tagName === 'select')[1].value = 'query';
    await findButton(f.$('repository-options'),'预览仓库内容').onclick();
    assert(f.$('repository-options').textContent.includes('Private repository source'));
    findButton(f.$('repository-options'),'确认用于下一条聊天').onclick();
  }
  await test('repository context requires preview and explicit confirmation, attaches once and never goes to proposals', async () => {
    for (const search of [false,true]) {
      const f = fixture(undefined,{repositories:[{id:'repo',label:'My repository'}]}); await f.login('alice');
      await chooseRepository(f, search); assert.equal(chatPosts(f).length,0);
      await f.send('/delegate bob Share only this summary');
      const proposal = f.calls.find(call=>call.url==='/api/proposals'&&call.options.method==='POST');
      assert.equal(JSON.parse(proposal.options.body).summary,'Share only this summary');
      assert(!f.$('repository-context').classList.contains('hide'));
      await f.send('Explain the selected source');
      const message = JSON.parse(chatPosts(f)[0].options.body).message;
      assert.equal(message, (search?'/repo-search':'/repo')+' repo '+'a'.repeat(40)+' '+(search?'literal word':'README.md')+'\nExplain the selected source');
      assert(f.$('repository-context').classList.contains('hide')); assert.equal(f.storage.size,0);
      await f.send('Ordinary next question'); assert.equal(JSON.parse(chatPosts(f)[1].options.body).message,'Ordinary next question');
    }
  });
  await test('repository selection clears across identities and late reads cannot restore it', async () => {
    const f = fixture(undefined,{repositories:[{id:'repo',label:'My repository'}]}); await f.login('alice'); await chooseRepository(f);
    await f.login('bob'); assert(f.$('repository-context').classList.contains('hide')); await f.send('No repository');
    assert.equal(JSON.parse(chatPosts(f)[0].options.body).message,'No repository');
    f.hold(); const pending=f.$('choose-repository').onclick(); await f.ready(); f.$('logout').onclick(); f.release(); await pending;
    assert.equal(f.$('repository-options').textContent,''); assert(f.$('repository-picker').classList.contains('hide'));
  });
  await test('mismatched repository receipt stays unresolved without repeating the command', async () => {
    const f = fixture(undefined,{repositories:[{id:'repo',label:'My repository'}],repositoryMismatch:true}); await f.login('alice'); await chooseRepository(f);
    await f.send('Question'); assert.equal(f.storage.size,1); assert(f.$('status').textContent.includes('仓库回执'));
    assert(!JSON.stringify([...f.storage]).includes('Private repository source'));
    await f.$('check-turn').onclick(); assert.equal(chatPosts(f).length,1); assert.equal(f.storage.size,1);
    f.confirm(true); f.$('new-turn').onclick(); await f.send('Fresh ordinary question');
    assert.equal(JSON.parse(chatPosts(f)[1].options.body).message,'Fresh ordinary question');
  });
  await test('handwritten delivery never claims a private draft source', async () => {
    const f=fixture(undefined,{tasks:[task()],taskAction:()=>({action:'deliver',task_id:'task-one',expected_revision:3})}); await f.login('bob');
    await f.send('直接交付吧'); await new Promise(r=>setImmediate(r));
    await findButton(f.$('conversation'),'填写交付内容').onclick();
    const box = walk(f.$('conversation')).find(node => node.className === 'action-editor');
    walk(box).find(node => node.tagName === 'textarea').value = 'Handwritten delivery';
    await findButton(box,'确认交付').onclick();
    assert.equal(Object.hasOwn(JSON.parse(taskPosts(f)[0].options.body),'source_draft_id'),false);
  });
  await test('task filters archive finished work while retaining explicit access', async () => {
    const f = fixture(undefined, {tasks:[task(), task({id:'done',status:'completed'}), task({id:'owned',owner:'alice',requester:'bob'})]}); await f.login('alice');
    const card = id => f.$('tasks').children.find(node => node.taskId === id);
    assert(card('done').classList.contains('hide')); assert(!card('task-one').classList.contains('hide'));
    f.$('task-filter').value = 'waiting'; f.$('task-filter').onchange(); assert(card('task-one').classList.contains('hide')); assert(!card('owned').classList.contains('hide'));
    f.$('task-filter').value = 'mine'; f.$('task-filter').onchange(); assert(!card('task-one').classList.contains('hide')); assert(card('owned').classList.contains('hide'));
    f.$('task-filter').value = 'completed'; f.$('task-filter').onchange(); assert(!card('done').classList.contains('hide'));
    assert.equal(f.calls.filter(call => call.options.method !== 'GET').length, 0);
  });
  await test('message delegation creates only a proposal and repeat-question never sends', async () => {
    const message = {role:'user',content:'Only this selected message',status:'complete'};
    const f = fixture(undefined, {messages:[message]}); await f.login('alice');
    const bubble = f.$('conversation').children.find(node => node.className.includes('msg-user'));
    await findButton(bubble, '重新问').onclick(); assert.equal(f.$('text').value, message.content); assert.equal(chatPosts(f).length, 0);
    await findButton(bubble, '把这段交给某人').onclick();
    const panel = walk(bubble).find(node => node.className === 'message-delegation');
    walk(panel).find(node => node.tagName === 'input').value = 'bob';
    await findButton(panel, '生成委派提案').onclick();
    const writes = f.calls.filter(call => call.options.method === 'POST'); assert.equal(writes.length, 1); assert.equal(writes[0].url, '/api/proposals');
    const body = JSON.parse(writes[0].options.body); assert.equal(body.owner,'bob'); assert.equal(body.summary, message.content); assert(body.request_id);
  });
  await test('file import validates UTF-8 and size without sending or losing existing material', async () => {
    const f = fixture(); await f.login('alice'); const proposal = f.$('conversation').children[0];
    const picker = walk(proposal).find(node => node.type === 'file');
    const file = (name, bytes, size = bytes.length) => ({name,size,arrayBuffer:async()=>Uint8Array.from(bytes).buffer});
    picker.files = [file('notes.md',new TextEncoder().encode('# 文档\n可共享正文'))]; await picker.onchange();
    assert.equal(proposal.detailsBox.value, '# 文档\n可共享正文'); assert.equal(f.calls.filter(call=>call.options.method==='POST').length,0);
    for (const invalid of [file('bad.md',[0xff]), file('binary.txt',[0]), file('huge.md',[65],262145), file('image.png',[65])]) {
      picker.files=[invalid]; await picker.onchange(); assert.equal(proposal.detailsBox.value, '# 文档\n可共享正文');
    }
    await findButton(proposal,'确认共享并委派').onclick();
    assert.equal(JSON.parse(f.calls.find(call=>call.url.endsWith('/actions')).options.body).details, '# 文档\n可共享正文');
  });
  await test('first-login guide is per user and example selection fills without sending', async () => {
    const preferences = new Map(); const f = fixture(undefined,{preferences,proposals:[]}); await f.login('alice');
    assert(!f.$('onboarding').classList.contains('hide')); f.$('onboarding-next').onclick(); assert(f.$('onboarding-step').textContent.startsWith('2 / 3'));
    f.$('onboarding-next').onclick(); f.$('onboarding-next').onclick(); assert.equal(preferences.get('agenct.onboarding.v1:alice'),'done');
    await f.login('alice'); assert(f.$('onboarding').classList.contains('hide'));
    const example = findButton(f.$('conversation'),'帮我梳理今天的工作重点'); assert(example); example.onclick(); assert.equal(f.$('text').value,'帮我梳理今天的工作重点'); assert.equal(chatPosts(f).length,0);
    await f.login('bob'); assert(!f.$('onboarding').classList.contains('hide'));
  });
  await test('keyboard shortcut toggles drawer and Escape returns focus', async () => {
    const f = fixture(undefined,{width:400}); await f.login('alice'); f.$('onboarding-skip').onclick();
    f.key({key:'K',ctrlKey:true,shiftKey:true}); assert(f.$('room').classList.contains('tasks-open')); assert(f.$('tab-activity').focused);
    f.key({key:'Escape'}); assert(!f.$('room').classList.contains('tasks-open')); assert(f.$('toggle-tasks').focused);
    assert.equal(f.calls.filter(call=>call.options.method==='POST').length,0);
  });
  await test('Markdown creates safe structured text, code and tables without HTML execution', async () => {
    const content = '# 标题\n- 条目\n\n| 名称 | 值 |\n| --- | --- |\n| a | b |\n```js\n<script>alert(1)</script>\n```\n<img src=x onerror=alert(1)> **粗体**';
    const f = fixture(undefined, { messages: [{role: 'assistant', status: 'complete', content}] }); await f.login('alice');
    const nodes = walk(f.$('conversation'));
    for (const tag of ['h3', 'ul', 'table', 'code', 'strong']) assert(nodes.some(node => node.tagName === tag), tag);
    assert(!nodes.some(node => ['img', 'script'].includes(node.tagName)));
    assert(f.$('conversation').textContent.includes('<img src=x onerror=alert(1)>'));
    assert(findButton(f.$('conversation'), '复制代码'));
  });
  await test('manual proposal edit previews new recipient and summary and preserves material after refresh', async () => {
    const f = fixture(); await f.login('alice');
    let card = f.$('conversation').children[0]; await findButton(card, '修改后确认').onclick();
    let panel = walk(card).find(node => node.className === 'action-editor');
    const fields = walk(panel); fields.find(node => node.tagName === 'input').value = 'carol';
    fields.find(node => node.tagName === 'textarea').value = 'Manually rewritten summary';
    f.change(); await f.tick(); card = f.$('conversation').children[0];
    assert(walk(card).includes(panel)); card.detailsBox.value = 'New shared material'; card.detailsBox.oninput();
    findButton(panel, '预览').onclick(); assert(panel.textContent.includes('New shared material'));
    await findButton(panel, '确认修改并委派').onclick();
    const body = JSON.parse(f.calls.find(call => call.url === '/api/proposals/1/actions')?.options.body || '{}');
    // A changed proposal revision invalidates an open editor instead of overwriting new server data.
    assert.deepEqual(body, {});
  });
  await test('manual proposal edits require preview and send only explicitly entered fields', async () => {
    const f = fixture(); await f.login('alice'); const card = f.$('conversation').children[0];
    await findButton(card, '修改后确认').onclick(); const panel = walk(card).find(node => node.className === 'action-editor');
    const fields = walk(panel); fields.find(node => node.tagName === 'input').value = 'carol'; fields.find(node => node.tagName === 'textarea').value = 'Human summary';
    assert.equal(f.calls.filter(call => call.options.method === 'POST').length, 0);
    findButton(panel, '预览').onclick(); await findButton(panel, '确认修改并委派').onclick();
    assert.deepEqual(JSON.parse(f.calls.find(call => call.url === '/api/proposals/1/actions').options.body), {action:'confirm',expected_revision:0,owner:'carol',summary:'Human summary'});
  });
  await test('old pending proposals collapse and bulk rejection never shares content', async () => {
    const f = fixture(undefined, { proposals: [{id: 8, owner:'bob', summary:'Old summary', status:'pending', revision:2, created_at:'2020-01-01 00:00:00'}] }); await f.login('alice');
    const group = f.$('conversation').children.find(node => node.className === 'old-proposals');
    assert.equal(group.open, false); assert(group.textContent.includes('1 个未处理的提案'));
    await findButton(group, '全部拒绝，不共享').onclick();
    assert.deepEqual(JSON.parse(f.calls.find(call => call.url === '/api/proposals/8/actions').options.body), {action:'reject',expected_revision:2});
  });
  await test('tab title counts this users pending work and resets on logout', async () => {
    const f = fixture(undefined, {tasks:[task(), task({id:'mine', owner:'alice', requester:'bob'})]}); await f.login('alice');
    assert.equal(f.document.title, '(1) Agenct'); f.$('logout').onclick(); assert.equal(f.document.title, 'Agenct');
  });
  await test('exact proposal text, explicit confirmation and revision', async () => {
    const f = fixture();
    await f.login('alice');
    const proposal = f.$('conversation').children[0];
    assert.match(proposal.textContent, /接收人：bob/);
    assert.match(proposal.textContent, /Exact <summary>\nsecond line/);
    assert(findButton(proposal, '确认共享并委派'));
    assert.equal(f.calls.filter(call => call.options.method === 'POST').length, 0);
    await findButton(proposal, '确认共享并委派').onclick();
    const call = f.calls.find(call => call.url === '/api/proposals/1/actions');
    assert.deepEqual(JSON.parse(call.options.body), { action: 'confirm', expected_revision: 0 });
  });

  await test('optional shared material is sent only on confirm and kept across re-render', async () => {
    const f = fixture();
    await f.login('alice');
    let proposal = f.$('conversation').children[0];
    const box = proposal.detailsBox;
    box.value = '  README 正文\n第二行  '; box.oninput();
    await f.tick();
    proposal = f.$('conversation').children[0];
    assert.equal(proposal.detailsBox.value, '  README 正文\n第二行  ');
    await findButton(proposal, '确认共享并委派').onclick();
    const call = f.calls.find(call => call.url === '/api/proposals/1/actions');
    assert.deepEqual(JSON.parse(call.options.body), { action: 'confirm', details: '  README 正文\n第二行  ', expected_revision: 0 });
  });

  await test('oversized shared material is refused before any request', async () => {
    const f = fixture();
    await f.login('alice');
    const proposal = f.$('conversation').children[0];
    proposal.detailsBox.value = '材'.repeat(87382);
    await findButton(proposal, '确认共享并委派').onclick();
    assert(!f.calls.some(call => call.url === '/api/proposals/1/actions'));
    assert.match(f.$('status').textContent, /256 KiB/);
  });

  await test('reject never sends shared material', async () => {
    const f = fixture();
    await f.login('alice');
    const proposal = f.$('conversation').children[0];
    proposal.detailsBox.value = 'should stay private';
    await findButton(proposal, '拒绝，不共享').onclick();
    const call = f.calls.find(call => call.url === '/api/proposals/1/actions');
    assert.deepEqual(Object.keys(JSON.parse(call.options.body)).sort(), ['action', 'expected_revision']);
  });

  await test('work index groups by day, describes the latest step and navigates', async () => {
    const now = new Date();
    const utc = d => d.toISOString().slice(0, 19).replace('T', ' ');
    const yesterday = new Date(now.getTime() - 86400000);
    const activity = [
      { kind: 'task', id: 'task-one', title: 'Shared task', status: 'working', role: 'requester', counterpart: 'bob',
        latest: { action: 'accept', actor: 'bob', at: utc(now), reason: null } },
      { kind: 'proposal', id: '1', title: 'Exact', status: 'pending', role: 'requester', counterpart: 'bob',
        latest: { action: 'proposal_pending', actor: 'alice', at: utc(yesterday), reason: null } },
      { kind: 'task', id: 'old', title: 'Old', status: 'canceled', role: 'owner', counterpart: 'carol',
        latest: { action: 'cancel', actor: 'carol', at: null, reason: '不需要了' } }
    ];
    const f = fixture({ enabled: false }, { tasks: [task()], activity });
    await f.login('alice');
    const list = f.$('activity').children;
    assert.deepEqual(list.filter(n => n.className === 'day').map(n => n.textContent), ['今天', '昨天', '更早']);
    const rows = list.filter(n => n.className.startsWith('activity-item'));
    assert.match(rows[0].textContent, /Shared task.*bob 已接手/s);
    assert.match(rows[1].textContent, /等你确认 · 委派给 bob/);
    assert.match(rows[2].textContent, /carol 取消了任务：不需要了/);
    assert(f.$('tasks').classList.contains('hide'), 'index is the default view');
    rows[0].onclick();
    assert(!f.$('tasks').classList.contains('hide'), 'clicking a task item opens the task view');
    assert(f.$('activity').classList.contains('hide'));
    assert.equal(f.calls.filter(call => call.options.method === 'POST').length, 0, 'navigation never mutates');
  });

  await test('narrow proposal index navigation reveals chat and ignores old identity callbacks', async () => {
    const activity=[{kind:'proposal',id:'1',title:'Exact',status:'pending',counterpart:'bob',latest:{action:'proposal_pending',actor:'alice',at:null}}];
    const f=fixture({enabled:false},{width:400,activity});await f.login('alice');
    f.$('text').value='Keep this draft';f.$('toggle-tasks').onclick();
    const row=f.$('activity').children.find(n=>n.className.startsWith('activity-item'));
    row.onclick();assert(!f.$('room').classList.contains('tasks-open'));
    assert.equal(f.$('text').value,'Keep this draft');assert.equal(chatPosts(f).length,0);
    await f.$('logout').onclick();f.$('toggle-tasks').onclick();row.onclick();
    assert(f.$('room').classList.contains('tasks-open'),'stale row must not navigate the new view');
  });

  await test('chat shows time separators only for dated messages with gaps', async () => {
    const utc = d => d.toISOString().slice(0, 19).replace('T', ' ');
    const t0 = new Date(Date.now() - 3600000);
    const messages = [
      { role: 'user', content: 'legacy', status: 'complete' },
      { role: 'user', content: 'a', status: 'complete', created_at: utc(t0) },
      { role: 'assistant', content: 'b', status: 'complete', created_at: utc(new Date(t0.getTime() + 60000)) },
      { role: 'user', content: 'c', status: 'complete', created_at: utc(new Date(t0.getTime() + 30 * 60000)) },
      { role: 'user', content: 'bad', status: 'complete', created_at: 'not-a-time' }
    ];
    const f = fixture({ enabled: false }, { messages });
    await f.login('alice');
    const seps = f.$('conversation').children.filter(n => n.className === 'time-sep');
    assert.equal(seps.length, 2, 'first dated message and the 29-minute gap only');
    assert.match(seps[0].textContent, /^\d{2}:\d{2}$/);
  });

  await test('reject proposal without creating a task', async () => {
    const f = fixture();
    await f.login('alice');
    await findButton(f.$('conversation').children[0], '拒绝，不共享').onclick();
    const call = f.calls.find(call => call.url === '/api/proposals/1/actions');
    assert.equal(JSON.parse(call.options.body).action, 'reject');
    assert(!f.calls.some(call => call.url === '/api/tasks' && call.options.method === 'POST'));
  });

  await test('delegate only creates proposal and sends request ID', async () => {
    const f = fixture();
    await f.login('alice');
    await f.send('/delegate bob review it');
    const call = f.calls.find(call => call.url === '/api/proposals' && call.options.method === 'POST');
    const body = JSON.parse(call.options.body);
    assert.equal(body.owner, 'bob');
    assert.equal(body.summary, 'review it');
    assert.match(body.request_id, /^[a-f0-9-]{36}$/);
    assert(!f.calls.some(call => call.url === '/api/tasks' && call.options.method === 'POST'));
    await f.send('请让 bob 帮我 检查文档');
    const message = f.calls.find(call => call.url === '/api/messages' && call.options.method === 'POST');
    assert.match(JSON.parse(message.options.body).request_id, /^[a-f0-9-]{36}$/);
  });

  await test('unchanged draft retries preserve request ID', async () => {
    const f = fixture();
    await f.login('alice');
    f.failNextPost();
    await f.send('/delegate bob review it');
    await f.send('/delegate bob review it');
    const posts = f.calls.filter(call => call.options.method === 'POST');
    assert.equal(JSON.parse(posts[0].options.body).request_id, JSON.parse(posts[1].options.body).request_id);
  });

  await test('logout prevents pending refresh repopulating private content', async () => {
    const f = fixture();
    await f.login('alice');
    f.hold();
    const refresh = f.$('refresh').onclick();
    f.$('logout').onclick();
    f.release();
    await refresh;
    assert.equal(f.$('conversation').children.length, 0);
    assert.equal(f.$('identity').textContent, '');
  });

  await test('older login and old card cannot affect newer identity', async () => {
    const f = fixture();
    await f.login('alice');
    const oldCard = f.$('conversation').children[0];
    f.hold();
    const oldLogin = f.login('alice');
    f.unhold();
    await f.login('bob');
    f.release();
    await oldLogin;
    assert.match(f.$('identity').textContent, /^bob/);
    assert.match(f.$('identity').textContent, /已验证身份/);
    assert.doesNotMatch(f.$('identity').textContent, /开发令牌认证/);
    const before = f.calls.length;
    await findButton(oldCard, '确认共享并委派').onclick();
    assert.equal(f.calls.length, before, 'Stale card must not send under new identity');
  });

  await test('old send completion cannot erase new account draft', async () => {
    const f = fixture();
    await f.login('alice');
    f.hold();
    const oldSend = f.send('/delegate bob old draft');
    f.$('logout').onclick();
    f.unhold();
    await f.login('bob');
    f.$('text').value = 'new account draft';
    f.release();
    await oldSend;
    assert.equal(f.$('text').value, 'new account draft');
    assert.match(f.$('identity').textContent, /^bob/);
    assert.equal(f.$('send').disabled, false);
  });
  await test('visible polling updates snapshots without mutation and preserves drafts', async () => {
    const f = fixture();
    await f.login('alice');
    const original = f.$('conversation').children[0];
    f.$('text').value = 'unsent private draft';
    await f.tick();
    assert.equal(f.$('conversation').children[0], original, 'Unchanged snapshot preserves DOM');
    f.change();
    await f.tick();
    assert.notEqual(f.$('conversation').children[0], original);
    assert.match(f.$('last-refresh').textContent, /有新的/);
    assert.equal(f.$('text').value, 'unsent private draft');
    assert.equal(f.calls.filter(call => call.options.method === 'POST').length, 0);
    assert([...f.timers.values()].some(timer => timer.delay === 3000));
  });

  await test('hidden tabs retain notification polling; logout clears timers and stale polling', async () => {
    const f = fixture();
    await f.login('alice');
    f.visible(false);
    await f.ready();
    assert([...f.timers.values()].some(timer => timer.delay === 3000));
    f.hold();
    f.visible(true);
    const count = f.calls.length;
    f.visible(true);
    assert.equal(f.calls.length, count, 'Concurrent polls coalesce');
    f.$('logout').onclick();
    f.release();
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(f.timers.size, 0);
    assert.equal(f.$('conversation').children.length, 0);
    assert.equal(f.$('last-refresh').textContent, '');
  });

  await test('read failures back off without changing content and recover automatically', async () => {
    const f = fixture();
    await f.login('alice');
    const original = f.$('conversation').children[0];
    f.failReads(503);
    for (const delay of [6000, 12000, 24000, 48000, 60000]) {
      await f.tick();
      assert([...f.timers.values()].some(timer => timer.delay === delay));
    }
    assert.equal(f.$('conversation').children[0], original);
    assert.match(f.$('last-refresh').textContent, /无法同步/);
    f.failReads(0);
    await f.tick();
    assert([...f.timers.values()].some(timer => timer.delay === 3000));
    assert.match(f.$('last-refresh').textContent, /已同步/);
  });

  await test('expired auth clears private UI and stops polling', async () => {
    const f = fixture();
    await f.login('alice');
    f.failReads(401);
    await f.tick();
    assert.equal(f.timers.size, 0);
    assert.equal(f.$('conversation').children.length, 0);
    assert.match(f.$('status').textContent, /登录已失效/);
  });

  await test('manual refresh during polling obtains a subsequent snapshot', async () => {
    const f = fixture();
    await f.login('alice');
    f.hold();
    const poll = f.tick();
    const count = f.calls.length;
    const manual = f.$('refresh').onclick();
    assert.equal(f.calls.length, count, 'Only one read batch in flight');
    f.release();
    await Promise.all([poll, manual]);
    assert.equal(f.calls.length, count + 4, 'Manual request receives a fresh batch (messages, proposals, tasks, activity) after in-flight read');
  });
  await test('stalled read times out and schedules recovery instead of overlapping requests', async () => {
    const f = fixture();
    await f.login('alice');
    f.stallReads();
    const poll = f.tick();
    const timer = [...f.timers.entries()].find(([, value]) => value.delay === 15000);
    assert(timer);
    f.timers.delete(timer[0]);
    timer[1].callback();
    await poll;
    assert.match(f.$('last-refresh').textContent, /无法同步/);
    assert([...f.timers.values()].some(value => value.delay === 6000));
  });
  await test('profile command stays in single chat and survives refresh without sending to model', async () => {
    const f = fixture(); await f.login('alice'); await f.send('/agents');
    assert.equal(f.calls.filter(call => call.options.method === 'POST' || call.options.method === 'PUT').length, 0);
    const panel = f.$('conversation').children.at(-1);
    assert.match(panel.textContent, /个人角色设置/);
    await f.$('refresh').onclick();
    assert.equal(f.$('conversation').children.at(-1), panel);
    const controls = panel.children[0];
    f.change(); await f.tick();
    assert.equal(f.$('conversation').children.at(-1), panel, 'Changed polling snapshot keeps the same profile card');
    assert.equal(panel.children[0], controls, 'Polling preserves in-progress profile fields');
    f.$('logout').onclick(); await f.login('bob');
    assert(!f.$('conversation').children.includes(panel));
  });
  await test('browser startup restores cookie session without bearer credentials', async () => {
    const f = fixture({ enabled: true, authenticated: true, user: 'alice', csrf_token: 'csrf-only' });
    await f.ready();
    assert.match(f.$('identity').textContent, /^alice/);
    assert(f.calls.filter(call => call.url.startsWith('/api/')).every(call => !('Authorization' in call.options.headers)));
    assert(f.calls.filter(call => call.url.startsWith('/api/')).every(call => call.options.headers['X-CSRF-Token'] === 'csrf-only'));
    assert(f.calls.every(call => call.options.credentials === 'same-origin'));
    await f.send('/delegate bob browser task');
    const post = f.calls.find(call => call.url === '/api/proposals' && call.options.method === 'POST');
    assert.equal(post.options.headers['X-CSRF-Token'], 'csrf-only');
    await f.$('logout').onclick();
    assert.equal(f.$('identity').textContent, '');
    assert.equal(f.$('conversation').children.length, 0);
    const logout = f.calls.find(call => call.url === '/auth/logout');
    assert.equal(logout.options.headers['X-CSRF-Token'], 'csrf-only');
    assert.equal(logout.options.method, 'POST');
    assert.match(f.$('status').textContent, /已退出服务器会话/);
  });
  await test('browser unauthenticated mode redirects and forbids dev token login', async () => {
    const f = fixture({ enabled: true, authenticated: false }); await f.ready();
    await f.login('alice');
    assert(!f.calls.some(call => call.url.startsWith('/api/')));
    f.$('oidc-login').onclick();
    assert.equal(f.calls.at(-1).redirect, '/auth/login');
  });
  await test('unknown startup configuration fails closed', async () => {
    const f = fixture({}); await f.ready(); await f.login('alice');
    assert(!f.calls.some(call => call.url.startsWith('/api/')));
    assert.match(f.$('status').textContent, /响应无效/);
  });
  await test('browser logout prevents pending reads and stale cards restoring identity', async () => {
    const f = fixture({ enabled: true, authenticated: true, user: 'alice', csrf_token: 'csrf-only' });
    await f.ready(); const card = f.$('conversation').children[0];
    f.hold(); const refresh = f.$('refresh').onclick();
    await f.$('logout').onclick(); f.release(); await refresh;
    const count = f.calls.length; await findButton(card, '确认共享并委派').onclick();
    assert.equal(f.calls.length, count);
    assert.equal(f.$('conversation').children.length, 0);
    assert.equal(f.timers.size, 0);
  });
  await test('startup session error never enables bearer fallback', async () => {
    const f = fixture({ enabled: false }, { sessionFailure: true });
    await f.ready(); await f.login('alice');
    assert(!f.calls.some(call => call.url.startsWith('/api/')));
    assert.match(f.$('status').textContent, /无法检查登录状态/);
  });
  await test('logout cancels pending startup identity restoration', async () => {
    const f = fixture({ enabled: true, authenticated: true, user: 'alice', csrf_token: 'csrf-only' }, { holdSession: true });
    await f.$('logout').onclick(); f.release(); await f.ready();
    assert(!f.calls.some(call => call.url.startsWith('/api/')));
    assert.equal(f.$('identity').textContent, '');
  });
  await test('uncertain browser logout clears private UI without claiming server success', async () => {
    const f = fixture({ enabled: true, authenticated: true, user: 'alice', csrf_token: 'csrf-only' }, { logoutFailure: true });
    await f.ready(); await f.$('logout').onclick();
    assert.equal(f.$('conversation').children.length, 0);
    assert.equal(f.$('identity').textContent, '');
    assert.match(f.$('status').textContent, /无法确认服务器会话已退出/);
    assert.equal(f.$('oidc-login').disabled, false);
  });
  await test('uncertain chat blocks unchanged and changed resends, polling only reads', async () => {
    const f = fixture(); await f.login('alice'); f.failNextPost();
    await f.send('private question');
    const post = f.calls.find(call => call.url === '/api/messages' && call.options.method === 'POST');
    const id = JSON.parse(post.options.body).request_id;
    assert.equal(f.$('send').disabled, true);
    assert.equal(f.$('check-turn').disabled, false);
    assert.equal(f.$('turn-recovery').classList.contains('hide'), false);
    assert.match(f.$('turn-status').textContent, new RegExp(id));
    await f.send('private question'); await f.send('changed question'); await f.tick();
    assert.equal(f.calls.filter(call => call.options.method === 'POST').length, 1);
    assert.equal(f.$('text').value, 'changed question');
    assert(!f.calls.some(call => call.url.startsWith('/api/turns/')), 'Status check remains explicit');
    const saved = JSON.parse([...f.storage.values()][0]);
    assert.deepEqual(saved, { id, user: 'alice', browserMode: false });
    assert(!JSON.stringify([...f.storage]).includes('private question'));
  });
  await test('sending does not claim server receipt, then shows only acknowledged state', async () => {
    for (const outcome of ['pending', 'uncertain', 'complete', 'failure']) {
      const f = fixture(); await f.login('alice');
      f.postTurn(outcome === 'failure' ? 'complete' : outcome);
      if (outcome === 'failure') f.failNextPost();
      f.hold();
      const sending = f.send('question');
      await f.ready();
      assert.match(f.$('turn-status').textContent, /正在发送，尚未收到服务端回执/);
      assert.doesNotMatch(f.$('turn-status').textContent, /已记录|处理中|请先核对/);
      assert.equal(f.$('send').disabled, true);
      assert.equal(f.$('check-turn').disabled, true);
      assert.equal(f.$('new-turn').disabled, true);
      await f.send('duplicate');
      assert.equal(chatPosts(f).length, 1);
      f.release(); await sending;
      if (outcome === 'complete') {
        assert.equal(f.$('turn-status').textContent, '');
      } else {
        assert.match(f.$('turn-status').textContent, outcome === 'pending'
          ? /服务端已记录此请求，尚未确认处理完成/ : /结果尚未确认/);
        assert.doesNotMatch(f.$('turn-status').textContent, /正在发送|正在等待响应/);
        assert.equal(f.$('check-turn').disabled, false);
      }
    }
  });
  await test('first-use guidance keeps direct chat primary and details optional', async () => {
    const html = fs.readFileSync(path.join(__dirname, 'index.html'), 'utf8');
    assert.match(html, /直接说你想解决的问题，无需先配置角色/);
    assert.match(html, /aria-describedby="chat-help" placeholder="说说你想解决的问题…"/);
    assert.match(html, /<details[^>]*><summary>能力边界、委派示例与恢复说明<\/summary>/);
    assert.match(html, /Mock 模式仅按固定规则匹配文本，不是真实 AI 理解或自动执行/);
  });
  await test('pending and uncertain receipts only use GET until complete', async () => {
    for (const initialStatus of ['pending', 'uncertain']) {
      const f = fixture(); await f.login('alice'); f.postTurn(initialStatus);
      await f.send('original question');
      const post = f.calls.find(call => call.options.method === 'POST');
      const id = JSON.parse(post.options.body).request_id;
      for (const state of ['pending', 'uncertain']) {
        f.receipt(state); await f.$('check-turn').onclick();
        assert.equal(f.calls.at(-1).url, '/api/turns/' + id);
        assert.equal(f.calls.at(-1).options.method, 'GET');
        assert.equal(f.$('send').disabled, true);
        assert.equal(f.storage.size, 1);
      }
      f.$('text').value = 'new unsent draft';
      f.receipt('complete'); await f.$('check-turn').onclick();
      assert.equal(f.$('send').disabled, false);
      assert.equal(f.$('text').value, 'new unsent draft');
      assert.equal(f.storage.size, 0);
      assert.equal(f.$('turn-recovery').classList.contains('hide'), true);
      assert.equal(f.calls.filter(call => call.options.method === 'POST').length, 1);
      assert.match(f.$('status').textContent, /处理完成/);
    }
  });
  await test('404 or failed receipt checks preserve unresolved state without reposting', async () => {
    const f = fixture(); await f.login('alice'); f.failNextPost(); await f.send('question');
    const saved = [...f.storage.values()][0];
    for (const failure of [404, 503]) {
      f.failTurn(failure); await f.$('check-turn').onclick();
      assert.equal([...f.storage.values()][0], saved);
      assert.equal(f.$('send').disabled, true);
      assert.match(f.$('status').textContent, /未重发/);
    }
    assert.equal(f.calls.filter(call => call.options.method === 'POST').length, 1);
  });
  await test('new attempt requires warning confirmation and next explicit send uses a new ID', async () => {
    const f = fixture(); await f.login('alice'); f.failNextPost(); await f.send('question');
    await f.$('new-turn').onclick();
    assert.match(f.confirmations[0], /结果仍未知.*重复处理/);
    assert.equal(f.$('send').disabled, true);
    assert.equal(f.storage.size, 1);
    f.confirm(true); await f.$('new-turn').onclick();
    assert.equal(f.$('send').disabled, false);
    assert.equal(f.storage.size, 0);
    assert.equal(f.calls.filter(call => call.options.method === 'POST').length, 1, 'New attempt does not itself send');
    await f.send('question');
    const posts = f.calls.filter(call => call.options.method === 'POST');
    assert.equal(posts.length, 2);
    assert.notEqual(JSON.parse(posts[0].options.body).request_id, JSON.parse(posts[1].options.body).request_id);
  });
  await test('refresh restores only the receipt for the verified owner without storing credentials or text', async () => {
    for (const browserMode of [false, true]) {
      const auth = browserMode ? { enabled: true, authenticated: true, user: 'alice', csrf_token: 'private-csrf' } : { enabled: false };
      const f = fixture(auth);
      if (browserMode) await f.ready(); else await f.login('secret-token-alice');
      f.failNextPost(); await f.send('private question');
      const serialized = JSON.stringify([...f.storage]);
      assert(!serialized.includes('private question'));
      assert(!serialized.includes('secret-token'));
      assert(!serialized.includes('private-csrf'));
      const reloaded = fixture(auth, { storage: f.storage }); await reloaded.ready();
      if (!browserMode) {
        assert.equal(reloaded.$('turn-status').textContent, '', 'Receipt is not shown before authenticating');
        await reloaded.login('secret-token-alice');
      }
      assert.equal(reloaded.$('send').disabled, false);
      assert.equal(reloaded.$('text').value, '', 'Private draft is not persisted');
      assert.equal(reloaded.calls.filter(call => call.options.method === 'POST').length, 0);
      await reloaded.$('check-turn').onclick();
      assert.equal(reloaded.storage.size, 0);
      assert.match(reloaded.$('status').textContent, /处理完成/);
    }
  });
  await test('logout, changed owner, and changed auth mode invalidate stored receipts', async () => {
    for (const changed of ['logout', 'owner', 'mode']) {
      const f = fixture(); await f.login('alice'); f.failNextPost(); await f.send('private question');
      if (changed === 'logout') {
        await f.$('logout').onclick();
        assert.equal(f.storage.size, 0);
      } else {
        const auth = changed === 'mode' ? { enabled: true, authenticated: true, user: 'alice', csrf_token: 'csrf' } : { enabled: false };
        const reloaded = fixture(auth, { storage: f.storage }); await reloaded.ready();
        if (changed === 'owner') await reloaded.login('bob');
        assert.equal(reloaded.$('turn-status').textContent, '');
        assert.equal(reloaded.storage.size, 0);
        assert(!reloaded.calls.some(call => call.url.startsWith('/api/turns/')));
      }
    }
  });
  await test('in-flight send receipt is persisted before a response and survives reload', async () => {
    const auth = { enabled: true, authenticated: true, user: 'alice', csrf_token: 'csrf' };
    const f = fixture(auth); await f.ready(); f.hold();
    const send = f.send('question'); await f.ready();
    assert.equal(f.storage.size, 1);
    const reloaded = fixture(auth, { storage: new Map(f.storage), receipt: 'pending' }); await reloaded.ready();
    assert.equal(reloaded.$('send').disabled, true);
    assert.equal(reloaded.calls.filter(call => call.options.method === 'POST').length, 0);
    f.release(); await send;
  });
  await test('late chat and status responses cannot restore logged-out content or erase a new draft', async () => {
    for (const action of ['send', 'check']) {
      const f = fixture(); await f.login('alice');
      if (action === 'check') { f.failNextPost(); await f.send('question'); }
      f.hold(); const operation = action === 'send' ? f.send('question') : f.$('check-turn').onclick();
      await f.ready(); await f.$('logout').onclick(); f.unhold(); await f.login('bob');
      f.$('text').value = 'bob draft'; f.release(); await operation;
      assert.equal(f.$('text').value, 'bob draft');
      assert.equal(f.$('turn-status').textContent, '');
      assert.equal(f.storage.size, 0);
      assert.equal(f.$('send').disabled, false);
      assert.match(f.$('identity').textContent, /^bob/);
    }
  });
  await test('repeated status clicks coalesce and stale check cannot start a new attempt', async () => {
    const f = fixture(); await f.login('alice'); f.failNextPost(); await f.send('question');
    f.hold(); const check = f.$('check-turn').onclick();
    await f.$('check-turn').onclick(); f.confirm(true); await f.$('new-turn').onclick();
    assert.equal(f.calls.filter(call => call.url.startsWith('/api/turns/')).length, 1);
    assert.equal(f.confirmations.length, 0);
    f.release(); await check;
    assert.equal(f.storage.size, 0);
  });
  await test('receipt timeout retains recovery and enables another read-only check', async () => {
    const f = fixture(); await f.login('alice'); f.failNextPost(); await f.send('question');
    f.stallReads(); const check = f.$('check-turn').onclick();
    const timer = [...f.timers.entries()].find(([, value]) => value.delay === 15000);
    assert(timer); f.timers.delete(timer[0]); timer[1].callback(); await check;
    assert.equal(f.storage.size, 1);
    assert.equal(f.$('check-turn').disabled, false);
    assert.equal(f.$('send').disabled, true);
    assert.match(f.$('status').textContent, /未重发/);
  });
  await test('storage unavailable is visible without persisting private data', async () => {
    const f = fixture({ enabled: false }, { storageFailure: true }); await f.login('alice');
    f.failNextPost(); await f.send('question');
    assert.match(f.$('turn-status').textContent, /无法保存恢复凭据/);
    assert.equal(f.$('send').disabled, true);
    assert.equal(f.storage.size, 0);
  });
  await test('expired bearer authentication retains a hidden receipt for same-owner reauthentication', async () => {
    const f = fixture(); await f.login('alice'); f.failNextPost(); await f.send('question');
    const receipt = [...f.storage.values()][0]; f.failReads(401); await f.tick();
    assert.equal([...f.storage.values()][0], receipt);
    assert.equal(f.$('turn-status').textContent, '');
    assert.equal(f.$('room').classList.contains('hide'), true);
    f.failReads(0); await f.login('alice');
    assert.equal(f.$('send').disabled, false);
    assert.equal(f.storage.size, 0);
    assert.equal(f.calls.filter(call => call.options.method === 'POST').length, 1);
    await f.$('check-turn').onclick(); assert.equal(f.storage.size, 0);
  });
  await test('expired and unauthenticated browser sessions preserve a receipt until same-owner login', async () => {
    const auth = { enabled: true, authenticated: true, user: 'alice', csrf_token: 'original-csrf' };
    const f = fixture(auth); await f.ready(); f.failNextPost(); await f.send('question');
    const receipt = [...f.storage.values()][0]; f.failReads(401); await f.tick();
    auth.authenticated = false; f.failReads(0); await f.$('auth-retry').onclick();
    assert.equal([...f.storage.values()][0], receipt);
    assert.equal(f.$('turn-status').textContent, '');
    assert.equal(f.$('room').classList.contains('hide'), true);
    auth.authenticated = true; auth.csrf_token = 'new-csrf'; await f.$('auth-retry').onclick();
    assert.equal(f.$('send').disabled, false);
    assert.equal(f.storage.size, 0);
    await f.$('check-turn').onclick();
    assert.equal(f.calls.findLast(call => call.url.startsWith('/api/turns/')).options.headers['X-CSRF-Token'], 'new-csrf');
    assert.equal(f.storage.size, 0);
    assert.equal(f.calls.filter(call => call.options.method === 'POST').length, 1);
  });
  await test('unauthenticated refresh retains hidden receipt but a different verified owner invalidates it', async () => {
    const auth = { enabled: true, authenticated: true, user: 'alice', csrf_token: 'csrf' };
    const f = fixture(auth); await f.ready(); f.failNextPost(); await f.send('question');
    const receipt = [...f.storage.values()][0]; auth.authenticated = false;
    const reloaded = fixture(auth, { storage: f.storage }); await reloaded.ready();
    assert.equal([...reloaded.storage.values()][0], receipt);
    assert.equal(reloaded.$('turn-status').textContent, '');
    auth.authenticated = true; auth.user = 'bob'; auth.csrf_token = 'bob-csrf';
    await reloaded.$('auth-retry').onclick();
    assert.equal(reloaded.storage.size, 0);
    assert.equal(reloaded.$('turn-status').textContent, '');
    assert(!reloaded.calls.some(call => call.url.startsWith('/api/turns/')));
  });
  await test('stalled chat POST times out to recoverable uncertainty without resending', async () => {
    const f = fixture(); await f.login('alice'); f.stallPosts();
    const send = f.send('question'); await f.ready();
    assert.equal(f.storage.size, 1);
    assert.equal(f.$('check-turn').disabled, true);
    await f.send('question');
    const timer = [...f.timers.entries()].find(([, value]) => value.delay === 90000);
    assert(timer, 'Chat send must have a bounded wait');
    f.timers.delete(timer[0]); timer[1].callback(); await send;
    assert.match(f.$('status').textContent, /等待超时.*结果未知/);
    assert.equal(f.$('check-turn').disabled, false);
    assert.equal(f.$('new-turn').disabled, false);
    assert.equal(f.$('send').disabled, true);
    assert.equal(f.storage.size, 1);
    assert.equal(f.calls.filter(call => call.options.method === 'POST').length, 1);
    await f.$('check-turn').onclick();
    assert.equal(f.storage.size, 0);
    assert.equal(f.$('send').disabled, false);
    assert.equal(f.calls.filter(call => call.options.method === 'POST').length, 1);
  });
  await test('completed chat clears its timeout and logout cancels a stalled send without restoring recovery', async () => {
    const f = fixture(); await f.login('alice'); await f.send('question');
    assert(![...f.timers.values()].some(timer => timer.delay === 90000));
    f.stallPosts(); const send = f.send('another question'); await f.ready();
    await f.$('logout').onclick(); await send;
    assert.equal(f.storage.size, 0);
    assert.equal(f.$('turn-status').textContent, '');
    assert.equal(f.$('room').classList.contains('hide'), true);
    assert(![...f.timers.values()].some(timer => timer.delay === 90000));
  });
  await test('discussion selects exactly one visible task without sending or performing task actions', async () => {
    const one = task(), two = task({ id: 'task-two', revision: 8, summary: 'Other shared summary', result: null });
    const f = fixture({ enabled: false }, { tasks: [one, two] }); await f.login('alice');
    f.$('text').value = 'unsent question';
    const before = f.calls.length;
    f.discussion(one.id).onclick();
    assert.equal(f.calls.length, before);
    assert.equal(f.$('task-context').classList.contains('hide'), false);
    assert.match(f.$('task-context-details').textContent, /任务编号：task-one\n版本：3/);
    assert.match(f.$('task-context-details').textContent, /Shared <summary>\nsecond line/);
    assert.match(f.$('task-context-details').textContent, /Shared delivery result/);
    assert.match(f.$('task-context-details').textContent, /不包含其他任务或完整私聊.*不授权/);
    f.discussion(two.id).onclick();
    assert.match(f.$('task-context-details').textContent, /任务编号：task-two\n版本：8/);
    assert.doesNotMatch(f.$('task-context-details').textContent, /Shared delivery result|task-one/);
    assert.equal(f.calls.length, before);
    f.$('clear-task-context').onclick();
    assert.equal(f.$('task-context').classList.contains('hide'), true);
    assert.equal(f.$('task-context-details').textContent, '');
    assert.equal(f.$('text').value, 'unsent question');
    assert.equal(f.calls.length, before);
  });
  await test('selected task sends only its ID and reviewed revision for one ordinary chat', async () => {
    const f = fixture({ enabled: false }, { tasks: [task(), task({ id: 'task-two' })] }); await f.login('alice');
    f.discussion('task-one').onclick(); await f.send('How is this going?');
    const body = JSON.parse(chatPosts(f)[0].options.body);
    assert.deepEqual(body, { message: 'How is this going?', request_id: body.request_id,
      selected_task: { task_id: 'task-one', expected_revision: 3 } });
    assert.match(body.request_id, /^[a-f0-9-]{36}$/);
    assert(!JSON.stringify(body).includes('Shared'));
    assert.equal(f.$('task-context-details').textContent, '');
    assert.match(f.$('status').textContent, /本次任务上下文：task-one · 版本 3/);
    await f.send('Unrelated question');
    assert(!('selected_task' in JSON.parse(chatPosts(f)[1].options.body)));
    assert(!f.calls.some(call => call.url.includes('/actions') || call.url.includes('/worker')));
  });
  await test('unselected and explicitly cleared chat never includes visible tasks', async () => {
    const f = fixture({ enabled: false }, { tasks: [task()] }); await f.login('alice');
    await f.send('plain question');
    f.discussion('task-one').onclick(); f.$('clear-task-context').onclick(); await f.send('plain after clear');
    assert(chatPosts(f).every(call => !('selected_task' in JSON.parse(call.options.body))));
  });
  await test('profile and explicit delegation commands neither send nor consume selected task context', async () => {
    const f = fixture({ enabled: false }, { tasks: [task()] }); await f.login('alice');
    f.discussion('task-one').onclick(); await f.send('/agents');
    assert.equal(chatPosts(f).length, 0);
    assert.match(f.$('task-context-details').textContent, /task-one/);
    await f.send('/delegate bob new summary');
    const proposal = f.calls.find(call => call.url === '/api/proposals' && call.options.method === 'POST');
    assert(!('selected_task' in JSON.parse(proposal.options.body)));
    assert.match(f.$('task-context-details').textContent, /task-one/);
    await f.send('Discuss the original task');
    assert.deepEqual(JSON.parse(chatPosts(f)[0].options.body).selected_task, { task_id: 'task-one', expected_revision: 3 });
  });
  await test('task revision, content, visibility and participant changes invalidate unsent selection without changing its scope', async () => {
    for (const changed of [[task({ revision: 4 })], [task({ result: 'Changed result' })], [],
      [task({ requester: 'charlie', owner: 'david' })]]) {
      const f = fixture({ enabled: false }, { tasks: [task()] }); await f.login('alice');
      const oldButton = f.discussion('task-one'); oldButton.onclick();
      f.$('text').value = 'draft about selected task'; f.tasks(changed); await f.$('refresh').onclick();
      assert.match(f.$('task-context-details').textContent, /选择已失效/);
      assert.match(f.$('task-context-details').textContent, /版本：3/);
      oldButton.onclick();
      assert.match(f.$('task-context-details').textContent, /选择已失效/);
      await f.send('draft about selected task');
      assert.equal(chatPosts(f).length, 0);
      assert.equal(f.$('text').value, 'draft about selected task');
      assert.match(f.$('status').textContent, /未发送消息/);
      f.$('clear-task-context').onclick(); await f.send('explicitly context-free question');
      assert(!('selected_task' in JSON.parse(chatPosts(f)[0].options.body)));
    }
  });
  await test('reselecting an updated visible task explicitly binds the new revision', async () => {
    const f = fixture({ enabled: false }, { tasks: [task()] }); await f.login('alice');
    f.discussion('task-one').onclick(); f.tasks([task({ revision: 4 })]); await f.tick();
    f.discussion('task-one').onclick();
    assert.match(f.$('task-context-details').textContent, /版本：4/);
    assert.doesNotMatch(f.$('task-context-details').textContent, /选择已失效/);
    await f.send('use updated task');
    assert.equal(JSON.parse(chatPosts(f)[0].options.body).selected_task.expected_revision, 4);
  });
  await test('unchanged polling preserves selection but detached task cards cannot replace it', async () => {
    const f = fixture({ enabled: false }, { tasks: [task(), task({ id: 'task-two' })] }); await f.login('alice');
    const old = f.discussion('task-one'); old.onclick(); await f.tick();
    assert.equal(f.discussion('task-one'), old);
    assert.match(f.$('task-context-details').textContent, /task-one/);
    f.change(); await f.tick(); f.discussion('task-two').onclick(); old.onclick();
    assert.match(f.$('task-context-details').textContent, /task-two/);
    await f.send('question');
    assert.equal(JSON.parse(chatPosts(f)[0].options.body).selected_task.task_id, 'task-two');
  });
  await test('nonparticipants and malformed task identifiers or revisions cannot be selected', async () => {
    for (const invalid of [task({ requester: 'charlie', owner: 'david' }), task({ id: 123 }),
      task({ revision: '3' }), task({ revision: -1 }), task({ revision: 1.5 })]) {
      const f = fixture({ enabled: false }, { tasks: [invalid] }); await f.login('alice');
      assert.equal(f.discussion(invalid.id), undefined);
      await f.send('plain question');
      assert(!('selected_task' in JSON.parse(chatPosts(f)[0].options.body)));
    }
  });
  await test('logout, relogin and auth expiry clear task selection and stale cards cannot restore it', async () => {
    for (const transition of ['logout', 'login', 'expiry']) {
      // A finished task waits on nobody, so no automatic binding masks the check.
      const f = fixture({ enabled: false }, { tasks: [task({ status: 'completed' })] }); await f.login('alice');
      const old = f.discussion('task-one'); old.onclick();
      if (transition === 'logout') await f.$('logout').onclick();
      else if (transition === 'login') await f.login('bob');
      else { f.failReads(401); await f.tick(); }
      old.onclick();
      assert.equal(f.$('task-context-details').textContent, '');
      assert.equal(f.$('task-context').classList.contains('hide'), true);
      f.failReads(0); await f.login('alice'); await f.send('new session question');
      assert(!('selected_task' in JSON.parse(chatPosts(f)[0].options.body)));
    }
  });
  await test('in-flight selected context is frozen through changed tasks and repeated control clicks', async () => {
    const f = fixture({ enabled: false }, { tasks: [task(), task({ id: 'task-two' })] }); await f.login('alice');
    f.discussion('task-one').onclick(); const otherButton = f.discussion('task-two'); f.hold();
    const send = f.send('question'); await f.ready();
    assert.equal(f.$('clear-task-context').disabled, true);
    assert.equal(otherButton.disabled, true);
    otherButton.onclick(); f.$('clear-task-context').onclick(); await f.send('different question');
    f.tasks([task({ revision: 4 }), task({ id: 'task-two' })]); f.unhold(); await f.$('refresh').onclick();
    assert.match(f.$('task-context-details').textContent, /task-one\n版本：3/);
    assert.match(f.$('task-context-details').textContent, /已锁定/);
    f.discussion('task-two').onclick();
    assert.equal(chatPosts(f).length, 1);
    assert.deepEqual(JSON.parse(chatPosts(f)[0].options.body).selected_task, { task_id: 'task-one', expected_revision: 3 });
    f.release(); await send;
    assert.match(f.$('status').textContent, /本次任务上下文：task-one · 版本 3/);
    assert.equal(f.$('task-context-details').textContent, '');
    assert.equal(f.$('text').value, 'different question');
  });
  await test('uncertain context remains locked and read-only recovery keeps the original snapshot', async () => {
    const f = fixture({ enabled: false }, { tasks: [task(), task({ id: 'task-two' })] }); await f.login('alice');
    f.discussion('task-one').onclick(); f.failNextPost(); await f.send('private question');
    f.tasks([task({ revision: 4 }), task({ id: 'task-two' })]); await f.$('refresh').onclick();
    f.discussion('task-two').onclick(); f.$('clear-task-context').onclick();
    assert.match(f.$('task-context-details').textContent, /task-one\n版本：3/);
    assert.equal(f.$('clear-task-context').disabled, true);
    f.receipt('pending'); await f.$('check-turn').onclick();
    assert.equal(f.$('send').disabled, true);
    assert.match(f.$('task-context-details').textContent, /task-one\n版本：3/);
    f.receipt('complete'); await f.$('check-turn').onclick();
    assert.equal(chatPosts(f).length, 1);
    assert.equal(f.$('task-context-details').textContent, '');
    assert.match(f.$('status').textContent, /本次任务上下文：task-one · 版本 3/);
  });
  await test('reloaded recovery stores no selected task data and never guesses or resends context', async () => {
    const f = fixture({ enabled: false }, { tasks: [task()] }); await f.login('alice');
    f.discussion('task-one').onclick(); f.failNextPost(); await f.send('private question');
    const serialized = JSON.stringify([...f.storage]);
    for (const privateText of ['task-one', 'Shared', 'private question', 'revision']) assert(!serialized.includes(privateText));
    const reloaded = fixture({ enabled: false }, { tasks: [task({ revision: 9 })], storage: f.storage, turnContexts: f.turnContexts, receipt: 'pending' });
    await reloaded.login('alice');
    assert.equal(reloaded.$('task-context-details').textContent, '');
    assert.equal(reloaded.discussion('task-one').disabled, true);
    reloaded.discussion('task-one').onclick(); await reloaded.send('another question');
    assert.equal(chatPosts(reloaded).length, 0);
    reloaded.receipt('complete'); await reloaded.$('check-turn').onclick();
    assert.match(reloaded.$('status').textContent, /本次任务上下文：task-one · 版本 3/);
    assert.equal(chatPosts(reloaded).length, 0);
  });
  await test('new attempt drops old selected context only after confirmation and needs a new explicit selection', async () => {
    const f = fixture({ enabled: false }, { tasks: [task()] }); await f.login('alice');
    f.discussion('task-one').onclick(); f.failNextPost(); await f.send('question');
    f.$('new-turn').onclick();
    assert.match(f.$('task-context-details').textContent, /task-one/);
    f.confirm(true); f.$('new-turn').onclick();
    assert.equal(f.$('task-context-details').textContent, '');
    assert.equal(chatPosts(f).length, 1);
    await f.send('question');
    assert.equal(chatPosts(f).length, 2);
    assert(!('selected_task' in JSON.parse(chatPosts(f)[1].options.body)));
    assert.notEqual(JSON.parse(chatPosts(f)[0].options.body).request_id, JSON.parse(chatPosts(f)[1].options.body).request_id);
  });
  await test('mismatched task-context receipt never clears recovery or substitutes task scope', async () => {
    for (const wrong of [null, task({ id: 'other-task' }), task({ revision: 4 })]) {
      const f = fixture({ enabled: false }, { tasks: [task()] }); await f.login('alice');
      f.discussion('task-one').onclick(); f.responseContext(wrong); await f.send('question');
      assert.equal(f.$('send').disabled, true);
      assert.match(f.$('status').textContent, /任务上下文回执与本次选择不符/);
      assert.match(f.$('task-context-details').textContent, /task-one\n版本：3/);
      await f.$('check-turn').onclick();
      assert.equal(f.storage.size, 1);
      assert.equal(chatPosts(f).length, 1);
      f.responseContext(undefined); await f.$('check-turn').onclick();
      assert.equal(f.storage.size, 0);
    }
  });
  await test('unexpected context on unselected receipt stays unresolved and truncation is disclosed', async () => {
    const f = fixture({ enabled: false }, { tasks: [task()] }); await f.login('alice');
    f.responseContext(task()); await f.send('plain question');
    assert.equal(f.$('send').disabled, true);
    assert.match(f.$('status').textContent, /任务上下文回执与本次选择不符/);
    f.responseContext(undefined); await f.$('check-turn').onclick();
    f.discussion('task-one').onclick();
    f.responseContext({ ...task(), summary_truncated: true, result_truncated: true });
    await f.send('selected question');
    assert.match(f.$('status').textContent, /共享摘要已截断。交付结果已截断/);
  });
  await test('known pre-dispatch stale or unavailable selection clears only its unstarted receipt and keeps invalidated scope', async () => {
    for (const [code, httpStatus] of [['selected_task_stale_before_dispatch', 409],
      ['selected_task_unavailable_before_dispatch', 404]]) {
      const f = fixture({ enabled: false }, { tasks: [task()] }); await f.login('alice');
      f.discussion('task-one').onclick(); f.messageFailure(httpStatus, code);
      await f.send('question about original task');
      assert.equal(f.storage.size, 0);
      assert.equal(f.$('turn-recovery').classList.contains('hide'), true);
      assert.equal(f.$('send').disabled, false);
      assert.equal(f.$('clear-task-context').disabled, false);
      assert.equal(f.$('text').value, 'question about original task');
      assert.match(f.$('task-context-details').textContent, /选择已失效/);
      assert.match(f.$('task-context-details').textContent, /task-one\n版本：3/);
      assert.match(f.$('status').textContent, /尚未处理.*未自动重发/);
      assert.equal(chatPosts(f).length, 1);
      await f.send('question about original task');
      assert.equal(chatPosts(f).length, 1, 'Invalid scope requires explicit user action');
      f.tasks([task({ revision: 4 })]); await f.$('refresh').onclick();
      f.discussion('task-one').onclick(); f.messageFailure(0);
      assert.equal(chatPosts(f).length, 1, 'Reselection does not send');
      await f.send('question about updated task');
      const before = JSON.parse(chatPosts(f)[0].options.body), after = JSON.parse(chatPosts(f)[1].options.body);
      assert.notEqual(before.request_id, after.request_id);
      assert.equal(after.selected_task.expected_revision, 4);
    }
  });
  await test('generic or mismatched 409/404 codes never discard a possibly dispatched selected-context receipt', async () => {
    for (const [httpStatus, code] of [[409], [404], [409, 'different_code'],
      [404, 'selected_task_stale_before_dispatch'], [409, 'selected_task_unavailable_before_dispatch'],
      [500, 'selected_task_stale_before_dispatch']]) {
      const f = fixture({ enabled: false }, { tasks: [task()] }); await f.login('alice');
      f.discussion('task-one').onclick(); f.messageFailure(httpStatus, code); await f.send('question');
      assert.equal(f.storage.size, 1);
      assert.equal(f.$('send').disabled, true);
      assert.equal(f.$('clear-task-context').disabled, true);
      assert.match(f.$('task-context-details').textContent, /已锁定/);
      assert.match(f.$('status').textContent, /结果未知/);
      await f.send('another question'); assert.equal(chatPosts(f).length, 1);
    }
    const f = fixture(); await f.login('alice');
    f.messageFailure(409, 'selected_task_stale_before_dispatch'); await f.send('unselected question');
    assert.equal(f.storage.size, 1, 'Selection-specific errors cannot clear an unselected attempt');
  });
  const roles = (revision = 3) => ({ revision, agents: [
    { id: 'butler', name: '管家', role: 'coordinator', model_ref: 'server-default', enabled: true },
    { id: 'research', name: '<研究员>', role: 'researcher', model_ref: 'server-default', enabled: true },
    { id: 'writer', name: '写作者', role: 'writer', model_ref: 'server-default', enabled: false }
  ] });
  await test('role selection reads own profile and sends only one ID with exact revision', async () => {
    const f = fixture(undefined, { profile: roles() }); await f.login('alice');
    const start = f.calls.length; await f.$('choose-role').onclick();
    assert(f.role('research')); assert(!f.role('writer'));
    assert(f.calls.slice(start).every(call => call.options.method === 'GET'));
    f.role('research').onclick(); assert(f.$('role-details').textContent.includes('<研究员>'));
    assert.equal(chatPosts(f).length, 0);
    await f.send('review this');
    const body = JSON.parse(chatPosts(f)[0].options.body);
    assert.equal(body.agent_id, 'research'); assert.equal(body.expected_profile_revision, 3);
    assert.deepEqual(Object.keys(body).sort(), ['agent_id', 'expected_profile_revision', 'message', 'request_id']);
    assert.equal(f.storage.size, 0); assert(f.$('status').textContent.includes('researcher'));
    await f.send('next default'); assert(!('agent_id' in JSON.parse(chatPosts(f)[1].options.body)));
  });
  await test('role picker close, repeated reads and stale callbacks never choose a role', async () => {
    const f = fixture(undefined, { profile: roles() }); await f.login('alice');
    await f.$('choose-role').onclick(); const old = f.role('research');
    f.$('close-role-picker').onclick(); old.onclick();
    assert(f.$('role-details').textContent.includes('默认'));
    await f.$('choose-role').onclick(); const older = f.role('research');
    await f.$('choose-role').onclick(); older.onclick();
    assert(f.$('role-details').textContent.includes('默认'));
    f.hold(); const reading = f.$('choose-role').onclick();
    f.$('close-role-picker').onclick(); f.release(); await reading;
    assert(f.$('role-picker').classList.contains('hide')); assert.equal(f.$('role-options').children.length, 0);
  });
  await test('invalid or disabled role records are never silently offered', async () => {
    for (const mutate of [p => p.revision = -1, p => p.revision = 2 ** 53,
      p => p.agents.push(p.agents[0]), p => p.agents[1].role = 'shell',
      p => p.agents[1].model_ref = 'foreign', p => p.agents[1].id = '../bad',
      p => p.agents[0].enabled = false]) {
      const p = roles(); mutate(p); const f = fixture(undefined, { profile: p }); await f.login('alice');
      await f.$('choose-role').onclick(); assert(!f.role('research'));
      assert(f.$('role-options').children[0].textContent.includes('响应无效'));
    }
  });
  await test('unsent role survives task polling and commands but clear explicitly restores default', async () => {
    const f = fixture(undefined, { profile: roles(), tasks: [task()] }); await f.login('alice');
    await f.chooseRole(); f.discussion('task-one').onclick(); await f.$('refresh').onclick();
    await f.send('/agents'); await f.send('/delegate bob summary');
    assert(f.$('role-details').textContent.includes('researcher')); assert.equal(chatPosts(f).length, 0);
    await f.send('discuss'); const body = JSON.parse(chatPosts(f)[0].options.body);
    assert.equal(body.agent_id, 'research'); assert.equal(body.selected_task.task_id, 'task-one');
    await f.chooseRole(); f.$('clear-role').onclick(); await f.send('default');
    assert(!('agent_id' in JSON.parse(chatPosts(f)[1].options.body)));
  });
  await test('rereading changed profile invalidates selection without silent fallback', async () => {
    const f = fixture(undefined, { profile: roles() }); await f.login('alice'); await f.chooseRole();
    f.profile(roles(4)); await f.$('choose-role').onclick(); f.$('close-role-picker').onclick();
    await f.send('keep intended role'); assert.equal(chatPosts(f).length, 0);
    assert(f.$('role-details').textContent.includes('失效'));
    await f.chooseRole(); await f.send('new revision');
    assert.equal(JSON.parse(chatPosts(f)[0].options.body).expected_profile_revision, 4);
  });
  await test('identity changes and late profile reads cannot restore another owners selection', async () => {
    const f = fixture(undefined, { profile: roles() }); await f.login('alice');
    await f.$('choose-role').onclick(); const button = f.role('research');
    f.hold(); const reading = f.$('choose-role').onclick();
    await f.$('logout').onclick(); f.release(); await reading;
    f.profile({ revision: 0, agents: [roles().agents[0]] }); await f.login('bob'); button.onclick();
    assert(f.$('role-details').textContent.includes('默认'));
    await f.$('choose-role').onclick(); assert(!f.role('research'));
    await f.send('bob'); assert(!('agent_id' in JSON.parse(chatPosts(f)[0].options.body)));
  });
  await test('inflight and uncertain role stays frozen across clicks and profile edits', async () => {
    const f = fixture(undefined, { profile: roles() }); await f.login('alice'); await f.chooseRole();
    f.hold(); const sending = f.send('private original'); await f.ready();
    assert(f.$('choose-role').disabled); f.$('clear-role').onclick();
    f.profile(roles(4)); f.postTurn('uncertain'); f.release(); await sending;
    assert(f.$('role-details').textContent.includes('版本 3')); assert(f.$('choose-role').disabled);
    const n = chatPosts(f).length; await f.$('check-turn').onclick();
    assert.equal(chatPosts(f).length, n); assert.equal(f.storage.size, 0);
    assert(f.$('status').textContent.includes('版本 3'));
  });
  await test('selected role receipt binding survives reload without names or message text', async () => {
    const f = fixture(undefined, { profile: roles() }); await f.login('alice'); await f.chooseRole();
    f.failNextPost(); await f.send('secret draft');
    const saved = [...f.storage.values()][0];
    assert(saved.includes('research')); assert(!saved.includes('secret draft')); assert(!saved.includes('<研究员>'));
    const second = fixture(undefined, { profile: roles(8), storage: f.storage, turnProfiles: f.turnProfiles });
    await second.login('alice'); assert(!second.$('choose-role').disabled);
    await second.$('check-turn').onclick(); assert.equal(second.storage.size, 0);
    assert(second.$('status').textContent.includes('版本 3')); assert.equal(chatPosts(second).length, 0);
  });
  await test('mismatched selected role receipt preserves uncertainty and no automatic retry', async () => {
    for (const changed of [null, { agent_id: 'writer', revision: 3 }, { agent_id: 'research', revision: 4 },
      { agent_id: 'research', revision: 3, role: 'writer', model_ref: 'server-default' }]) {
      const f = fixture(undefined, { profile: roles() }); await f.login('alice'); await f.chooseRole();
      f.responseProfile(changed); await f.send('original');
      assert.equal(f.storage.size, 1); assert(f.$('status').textContent.includes('角色回执'));
      await f.$('check-turn').onclick(); assert.equal(f.storage.size, 1); assert.equal(chatPosts(f).length, 1);
    }
  });
  await test('only exact known agent pre-dispatch errors release recovery and preserve task choice', async () => {
    for (const [status, code] of [[409, 'selected_agent_stale_before_dispatch'], [400, 'selected_agent_unavailable_before_dispatch']]) {
      const f = fixture(undefined, { profile: roles(), tasks: [task()] }); await f.login('alice');
      await f.chooseRole(); f.discussion('task-one').onclick(); f.messageFailure(status, code); await f.send('question');
      assert.equal(f.storage.size, 0); assert(f.$('role-details').textContent.includes('失效'));
      assert(f.$('task-context-details').textContent.includes('task-one'));
      f.messageFailure(null); await f.send('question'); assert.equal(chatPosts(f).length, 1);
      f.profile(roles(4)); await f.chooseRole(); await f.send('question');
      assert.equal(chatPosts(f).length, 2); assert.equal(JSON.parse(chatPosts(f)[1].options.body).selected_task.task_id, 'task-one');
    }
    for (const [status, code] of [[409, undefined], [404, 'selected_agent_unavailable_before_dispatch'], [400, 'selected_agent_stale_before_dispatch']]) {
      const f = fixture(undefined, { profile: roles() }); await f.login('alice'); await f.chooseRole();
      f.messageFailure(status, code); await f.send('question'); assert.equal(f.storage.size, 1);
    }
  });
  await test('role read failure or timeout preserves selection and cancelled late results stay closed', async () => {
    const f = fixture(undefined, { profile: roles() }); await f.login('alice'); await f.chooseRole();
    f.failReads(503); await f.$('choose-role').onclick();
    assert(f.$('role-details').textContent.includes('researcher'));
    assert(f.$('role-options').children[0].textContent.includes('无法读取'));
    f.failReads(0); f.hold(); const read = f.$('choose-role').onclick();
    const timeout = [...f.timers.values()].find(timer => timer.delay === 15000); timeout.callback();
    f.release(); await read;
    assert(f.$('role-options').children[0].textContent.includes('超时'));
    f.$('close-role-picker').onclick();
    await f.send('still selected'); assert.equal(JSON.parse(chatPosts(f)[0].options.body).agent_id, 'research');
  });
  await test('same-owner reauthentication restores frozen role but different owner cannot inherit it', async () => {
    const f = fixture(undefined, { profile: roles() }); await f.login('alice'); await f.chooseRole();
    f.failNextPost(); await f.send('private draft'); f.failReads(401); await f.tick();
    assert(f.$('room').classList.contains('hide')); assert(!f.$('role-details').textContent.includes('research'));
    f.failReads(0); f.profile(roles(9)); f.receipt('pending'); await f.login('alice');
    assert(f.$('choose-role').disabled); assert(f.$('role-details').textContent.includes('版本 3'));
    f.receipt('complete'); await f.$('check-turn').onclick(); assert.equal(f.storage.size, 0);
    await f.chooseRole(); f.failNextPost(); await f.send('another draft');
    await f.login('bob'); assert.equal(f.storage.size, 0);
    assert(!f.$('role-details').textContent.includes('research'));
  });
  await test('confirmed new attempt discards frozen role and requires explicit reselection', async () => {
    const f = fixture(undefined, { profile: roles() }); await f.login('alice'); await f.chooseRole();
    f.failNextPost(); await f.send('question'); const original = JSON.parse(chatPosts(f)[0].options.body);
    f.confirm(false); f.$('new-turn').onclick(); assert(f.$('choose-role').disabled);
    f.confirm(true); f.$('new-turn').onclick(); assert(f.$('role-details').textContent.includes('默认'));
    await f.send('new question'); const next = JSON.parse(chatPosts(f)[1].options.body);
    assert.notEqual(original.request_id, next.request_id); assert(!('agent_id' in next));
  });
  await test('confirmed profile editor save invalidates an unsent role via its real callback', async () => {
    const f = fixture(undefined, { profile: roles() }); await f.login('alice'); await f.chooseRole();
    await f.send('/agents');
    const descendants = element => [element, ...element.children.flatMap(descendants)];
    const button = text => descendants(f.$('conversation')).find(element => element.textContent === text);
    await button('检查更改').onclick();
    f.profile(roles(4)); await button('确认保存个人配置').onclick();
    assert(f.calls.some(call => call.url === '/api/profile' && call.options.method === 'PUT'));
    assert(f.$('role-details').textContent.includes('失效'));
    await f.send('must not fallback'); assert.equal(chatPosts(f).length, 0);
  });
  await test('stale task rejection preserves selected role for explicit task reselection', async () => {
    const f = fixture(undefined, { profile: roles(), tasks: [task()] }); await f.login('alice');
    await f.chooseRole(); f.discussion('task-one').onclick();
    f.messageFailure(409, 'selected_task_stale_before_dispatch'); await f.send('question');
    assert.equal(f.storage.size, 0); assert(f.$('role-details').textContent.includes('researcher'));
    assert(!f.$('role-details').textContent.includes('失效'));
    f.tasks([task({ revision: 4 })]); f.messageFailure(null); await f.$('refresh').onclick();
    f.discussion('task-one').onclick(); await f.send('question');
    const body = JSON.parse(chatPosts(f)[1].options.body);
    assert.equal(body.agent_id, 'research'); assert.equal(body.expected_profile_revision, 3);
    assert.equal(body.selected_task.expected_revision, 4);
  });
  await test('malformed restored role binding cannot be cleared by a completion receipt', async () => {
    const f = fixture(undefined, { profile: roles() }); await f.login('alice'); await f.chooseRole();
    f.failNextPost(); await f.send('question');
    const [key, raw] = [...f.storage.entries()][0]; const saved = JSON.parse(raw);
    saved.agent.revision = '3'; f.storage.set(key, JSON.stringify(saved));
    const next = fixture(undefined, { storage: f.storage, turnProfiles: f.turnProfiles }); await next.login('alice');
    await next.$('check-turn').onclick(); assert.equal(next.storage.size, 1);
    assert(next.$('status').textContent.includes('角色回执'));
    assert.equal(chatPosts(next).length, 0);
  });
  await test('private generated edits reach delivery only through explicit manual confirmation', async () => {
    let reviewed = null;
    const task = {id:'drafttask',owner:'bob',requester:'alice',revision:1,status:'working',summary:'Shared task'};
    const f = fixture(undefined, {tasks:[task],prompt:(label,value)=>{reviewed=value;return value;}});
    await f.login('bob');
    const walk=e=>[e,...e.children.flatMap(walk)];
    const button=text=>[f.$('conversation'), f.$('tasks')].flatMap(walk).filter(e=>e.textContent===text).at(-1);
    await button('我的私密交付草稿').onclick();
    await button('让我的 Agent 起草（不交付）').onclick();
    assert.equal(f.calls.filter(c=>c.url?.endsWith('/actions')).length,0);
    const input=[f.$('conversation'), f.$('tasks')].flatMap(walk).find(e=>e.value==='Generated draft'); input.value='Human reviewed edit';
    await button('使用此草稿填入人工交付').onclick();
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(f.calls.filter(c=>c.url?.endsWith('/actions')).length,0);
    const box = walk(f.$('conversation')).find(node => node.className === 'action-editor');
    assert.equal(walk(box).find(node => node.tagName === 'textarea').value, 'Human reviewed edit');
    assert.equal(f.calls.filter(c=>c.url?.endsWith('/actions')).length, 0);
    await findButton(box, '确认交付').onclick();
    const action=f.calls.find(c=>c.url?.endsWith('/actions'));
    assert.equal(JSON.parse(action.options.body).result,'Human reviewed edit');
    assert.equal(JSON.parse(action.options.body).source_draft_id,'fixture-generated-draft');
    assert.equal(JSON.parse(action.options.body).expected_revision,1);
  });
  await test('shared timeline and private draft hooks coexist in task cards', async () => {
    const task={id:'timeline-task',owner:'bob',requester:'alice',revision:1,status:'working',summary:'Shared'};
    const owner=fixture(undefined,{tasks:[task]});await owner.login('bob');
    const walk=e=>[e,...e.children.flatMap(walk)];
    let nodes=walk(owner.$('tasks'));
    assert(nodes.some(e=>e.textContent==='我的私密交付草稿'));
    await nodes.find(e=>e.textContent==='查看共享时间线').onclick();
    nodes=walk(owner.$('tasks'));assert(nodes.some(e=>e.textContent.includes('无法恢复')));
    assert.equal(owner.calls.filter(c=>c.options?.method==='POST').length,0);
    const requester=fixture(undefined,{tasks:[task]});await requester.login('alice');
    nodes=walk(requester.$('tasks'));
    assert(nodes.some(e=>e.textContent==='查看共享时间线'));
    assert(!nodes.some(e=>e.textContent==='我的私密交付草稿'));
  });
  const settle = async () => { for (let i = 0; i < 6; i++) await new Promise(resolve => setImmediate(resolve)); };
  const posts = (f, suffix) => f.calls.filter(c => c.options?.method === 'POST' && c.url.endsWith(suffix));
  const said = f => JSON.parse(chatPosts(f).at(-1).options.body);
  await test('plain replies go to the task waiting on me; accepting takes one confirmation', async () => {
    const f = fixture(undefined, { tasks: [task({ status: 'awaiting_owner', revision: 0 })],
      taskAction: body => body.selected_task && /接手/.test(body.message) ? { action: 'accept', task_id: 'task-one', expected_revision: 0 } : null });
    await f.login('bob'); await settle();
    const walk = e => [e, ...e.children.flatMap(walk)];
    assert(!walk(f.$('conversation')).some(e => e.tagName === 'button' && /接手/.test(e.textContent)), 'no action buttons before asked');
    assert.equal(f.$('task-context').classList.contains('hide'), false, 'composer is bound to the waiting task');
    await f.send('好的我接手'); await settle();
    assert.deepEqual(said(f).selected_task, { task_id: 'task-one', expected_revision: 0 });
    assert.equal(posts(f, '/actions').length, 0, 'understanding alone changes nothing');
    await findButton(f.$('conversation'), '确认接手').onclick();
    assert.deepEqual(JSON.parse(posts(f, '/tasks/task-one/actions')[0].options.body), { action: 'accept', expected_revision: 0 });
    // × detaches the composer from that task version.
    const g = fixture(undefined, { tasks: [task({ status: 'awaiting_owner', revision: 0 })] }); await g.login('bob'); await settle();
    g.$('clear-task-context').onclick(); await g.send('随便聊聊'); await settle();
    assert.equal(said(g).selected_task, undefined);
  });
  await test('agent draft is supplemented and delivered by talking to it', async () => {
    const draft = { status: 'complete', automatic: true, request_id: 'auto-task-one-r3' };
    const f = fixture(undefined, { tasks: [task({ result: null, my_draft: draft })],
      taskAction: body => /交付/.test(body.message) ? { action: 'deliver', task_id: 'task-one', expected_revision: 3 }
        : { action: 'revise', task_id: 'task-one', expected_revision: 3, instruction: body.message } });
    await f.login('bob'); await settle();
    assert.match(f.$('conversation').textContent, /Auto draft 需要补充的信息/);
    assert.match(f.$('conversation').textContent, /仅你可见/);
    await f.send('我是 bob，用于面试，语气正式'); await settle();
    const revision = JSON.parse(posts(f, '/tasks/task-one/drafts')[0].options.body);
    assert.deepEqual(revision.revision, { base_draft_id: 'auto-draft-id', instruction: '我是 bob，用于面试，语气正式' });
    assert.equal(posts(f, '/actions').length, 0, 'drafting never delivers');
    await f.send('可以，交付吧'); await settle();
    assert.equal(posts(f, '/actions').length, 0, 'delivery waits for one confirmation');
    await findButton(f.$('conversation'), '确认交付').onclick();
    const body = JSON.parse(posts(f, '/tasks/task-one/actions')[0].options.body);
    assert.deepEqual([body.action, body.source_draft_id, body.expected_revision, body.result], ['deliver', 'auto-draft-id', 3, 'Auto draft 需要补充的信息']);
  });
  await test('a typed yes carries out the waiting step and a typed no drops it, without a model turn', async () => {
    const f = fixture(undefined, { tasks: [task({ status: 'ready_for_acceptance', result: 'done' })],
      taskAction: body => /退回/.test(body.message) ? { action: 'return', task_id: 'task-one', expected_revision: 3 } : null });
    await f.login('alice'); await settle();
    await f.send('退回，改一下开头'); await settle();
    assert.match(f.$('conversation').textContent, /可以直接回复「确认」或「算了」/);
    const turns = chatPosts(f).length;
    await f.send('算了'); await settle();
    assert.equal(chatPosts(f).length, turns, 'no model turn for a plain no');
    assert.equal(posts(f, '/actions').length, 0);
    assert(!findButton(f.$('conversation'), '确认退回'), 'the step is withdrawn');
    await f.send('退回，改一下开头'); await settle();
    await f.send('确认'); await settle();
    assert.equal(chatPosts(f).length, turns + 1, 'only the request itself reached the model');
    assert.equal(JSON.parse(posts(f, '/tasks/task-one/actions')[0].options.body).action, 'return');
    assert.equal(f.$('text').value, '');
    // Without a waiting step, the same word is an ordinary message.
    const g = fixture(); await g.login('alice'); await g.send('确认'); await settle();
    assert.equal(chatPosts(g).length, 1);
  });
  await test('a waiting step sits at the end of the conversation and survives a reload, not a logout', async () => {
    const storage = new Map();
    const settings = { storage, tasks: [task({ status: 'ready_for_acceptance', result: 'done' })],
      messages: [{ role: 'user', content: 'later question', status: 'complete', created_at: '2099-01-01 00:00:00' }],
      activity: [{ kind: 'task', id: 'task-one', latest: { action: 'deliver', actor: 'bob', at: '2000-01-01 00:00:00' } }],
      taskAction: body => /退回/.test(body.message) ? { action: 'return', task_id: 'task-one', expected_revision: 3 } : null };
    const f = fixture(undefined, settings); await f.login('alice'); await settle();
    const cardIndex = g => g.$('conversation').children.findIndex(node => node.taskId === 'task-one');
    assert(cardIndex(f) < f.$('conversation').children.length - 1, 'an older step stays in place');
    await f.send('退回'); await settle();
    assert.equal(cardIndex(f), f.$('conversation').children.length - 1, 'the card asking for an answer is last');
    const again = fixture(undefined, settings); await again.login('alice'); await settle();
    assert(findButton(again.$('conversation'), '确认退回'), 'the waiting step survives a reload');
    await again.$('logout').onclick();
    const after = fixture(undefined, settings); await after.login('alice'); await settle();
    assert(!findButton(after.$('conversation'), '确认退回'), 'logout drops it');
  });
  await test('the composer clears as soon as a message is sent', async () => {
    const f = fixture(); await f.login('alice'); f.stallPosts();
    const send = f.send('long question'); await f.ready();
    assert.equal(f.$('text').value, '', 'the words now sit in the conversation');
    f.release?.(); void send;
  });
  await test('requester accepts a delivery by saying so and never reads drafts', async () => {
    const f = fixture(undefined, { tasks: [task({ status: 'ready_for_acceptance', result: '**交付**内容' })],
      taskAction: () => ({ action: 'approve', task_id: 'task-one', expected_revision: 3 }) });
    await f.login('alice'); await settle();
    assert.match(f.$('conversation').textContent, /bob 交付了结果/);
    await f.send('验收通过'); await settle();
    await findButton(f.$('conversation'), '确认验收').onclick();
    assert.equal(JSON.parse(posts(f, '/tasks/task-one/actions')[0].options.body).action, 'approve');
    const g = fixture(undefined, { tasks: [task({ my_draft: undefined })] }); await g.login('alice'); await settle();
    assert.match(g.$('conversation').textContent, /bob 已接手/);
    assert.equal(g.calls.filter(c => c.url.includes('/drafts')).length, 0);
  });

  await test('typed confirmation binds only selected task and matching action; detached replies stay ordinary', async () => {
    for (const mode of ['detach', 'other', 'wrong-action']) {
      const f = fixture(undefined, { tasks: [task({status:'ready_for_acceptance'}), task({id:'other',status:'working'})],
        taskAction: () => ({action:'approve',task_id:'task-one',expected_revision:3}) });
      await f.login('alice'); await settle(); await f.send('验收'); await settle();
      if (mode === 'detach') f.$('clear-task-context').onclick();
      if (mode === 'other') f.discussion('other').onclick();
      await f.send(mode === 'wrong-action' ? '确认退回' : '好的'); await settle();
      assert.equal(posts(f,'/actions').length,0);
      if (mode === 'wrong-action') assert.equal(f.$('text').value,'确认退回');
    }
  });
  await test('polling keeps typed input bound to its snapshot and preserves stale invalidation', async () => {
    const f=fixture(undefined,{tasks:[task({id:'A'})]}); await f.login('bob'); await settle();
    f.$('text').value='Private reply intended for A';
    f.tasks([task({id:'B'}),task({id:'A'})]); await f.$('refresh').onclick(); await settle();
    assert.match(f.$('task-context-details').textContent,/任务编号：A/);
    assert.equal(f.$('text').value,'Private reply intended for A');
    f.tasks([task({id:'B'}),task({id:'A',revision:4})]); await f.$('refresh').onclick(); await settle();
    assert.match(f.$('task-context-details').textContent,/失效/);
    await f.$('form').onsubmit({preventDefault(){}}); assert.equal(chatPosts(f).length,0);
    const g=fixture(undefined,{tasks:[]});await g.login('bob');g.$('text').value='Ordinary private chat';
    g.tasks([task()]);await g.$('refresh').onclick();assert(g.$('task-context').classList.contains('hide'));
  });
  await test('unavailable draft retains revision instruction until explicit retry with loaded base', async () => {
    const instruction='Use formal tone';
    const f=fixture(undefined,{tasks:[task()],taskAction:()=>({action:'revise',task_id:'task-one',expected_revision:3,instruction})});
    await f.login('bob');await settle();await f.send(instruction);await settle();
    assert.equal(posts(f,'/tasks/task-one/drafts').length,0);
    assert.match(f.$('conversation').textContent,/Use formal tone/);
    assert(findButton(f.$('conversation'),'用保留的要求修改草稿').disabled);
    f.tasks([task({my_draft:{status:'complete',request_id:'auto-existing'}})]);await f.$('refresh').onclick();await settle();
    assert.equal(posts(f,'/tasks/task-one/drafts').length,0,'no automatic retry');
    await findButton(f.$('conversation'),'用保留的要求修改草稿').onclick();await settle();
    const body=JSON.parse(posts(f,'/tasks/task-one/drafts')[0].options.body);
    assert.equal(body.revision.instruction,instruction);
    assert.equal(body.request_id,said(f).request_id);
    await f.$('logout').onclick();await f.login('alice');assert(!f.$('conversation').textContent.includes(instruction));
  });
  await test('the same recovered chat turn uses one draft receipt key across tabs and changing base drafts', async () => {
    const id='12345678-1234-4234-8234-123456789abc';
    const saved=JSON.stringify({id,user:'bob',browserMode:false});
    const action={action:'revise',task_id:'task-one',expected_revision:3,instruction:'Formal tone'};
    const run=async(draftId)=>{
      const storage=new Map([['agenct.pending-chat-turn.v1',saved]]);
      const f=fixture(undefined,{draftId,storage,tasks:[task({my_draft:{status:'complete',request_id:'auto-existing'}})],recoveredTaskAction:action});
      await f.login('bob');await settle();
      const retry=findButton(f.$('conversation'),'用保留的要求修改草稿');if(retry&&!retry.disabled)await retry.onclick();
      await settle();return f;
    };
    const a=await run('base-one'),b=await run('base-two');
    const bodies=[a,b].map(f=>JSON.parse(posts(f,'/tasks/task-one/drafts')[0].options.body));
    assert.equal(bodies[0].request_id,id);assert.equal(bodies[1].request_id,id);
    assert.equal(bodies[0].revision.instruction,'Formal tone');
    assert.notEqual(bodies[0].revision.base_draft_id,bodies[1].revision.base_draft_id);
    // The server's owner/request-id binding rejects a changed base, never dispatches a second turn.
    assert.equal(a.storage.size,0);assert.equal(b.storage.size,0);
  });
  await test('retained confirmation callback cannot execute a replacement action', async () => {
    const f=fixture(undefined,{tasks:[task({status:'ready_for_acceptance'})],
      taskAction:body=>({action:body.message.includes('退回')?'return':'approve',task_id:'task-one',expected_revision:3})});
    await f.login('alice');await settle();await f.send('验收');await settle();
    const old=findButton(f.$('conversation'),'确认验收');await f.send('退回修改');await settle();
    await old.onclick();assert.equal(posts(f,'/actions').length,0);
  });

  await test('failed or uncertain deferred revision retains instruction and checks original key without resending', async () => {
    for (const uncertain of [false,true]) {
      const f=fixture(undefined,{tasks:[task()],draftPostStatus:uncertain?'uncertain':'complete',
        taskAction:()=>({action:'revise',task_id:'task-one',expected_revision:3,instruction:'Keep this exact instruction'})});
      await f.login('bob');await settle();await f.send('Keep this exact instruction');await settle();
      f.tasks([task({my_draft:{status:'complete',request_id:'base'}})]);await f.$('refresh').onclick();await settle();
      if(!uncertain)f.failNextPost();
      await findButton(f.$('conversation'),'用保留的要求修改草稿').onclick();await settle();
      assert.match(f.$('conversation').textContent,/Keep this exact instruction/);
      const request=JSON.parse(posts(f,'/tasks/task-one/drafts')[0].options.body);
      await findButton(f.$('conversation'),'核对这次修订').onclick();await settle();
      assert.equal(posts(f,'/tasks/task-one/drafts').length,1);
      assert(f.calls.some(c=>c.url==='/api/tasks/task-one/drafts/turns/'+request.request_id));
      assert.equal(f.storage.size,0,'private instructions remain memory only');
    }
  });

  await test('direct revision failure keeps same-key read-only recovery', async () => {
    const f=fixture(undefined,{tasks:[task({my_draft:{status:'complete',request_id:'base'}})],draftPostFailure:true,
      taskAction:()=>({action:'revise',task_id:'task-one',expected_revision:3,instruction:'Retain direct instruction'})});
    await f.login('bob');await settle();await f.send('Retain direct instruction');await settle();
    assert.match(f.$('conversation').textContent,/Retain direct instruction/);
    await findButton(f.$('conversation'),'核对这次修订').onclick();await settle();
    assert.equal(posts(f,'/tasks/task-one/drafts').length,1);
  });

  await test('later revision instructions cannot replace an uncertain receipt or silently dispatch', async () => {
    const f=fixture(undefined,{tasks:[task({my_draft:{status:'complete',request_id:'base'}})],draftPostFailure:true,
      taskAction:body=>({action:'revise',task_id:'task-one',expected_revision:3,instruction:body.message})});
    await f.login('bob');await settle();await f.send('First instruction');await settle();
    const first=JSON.parse(posts(f,'/tasks/task-one/drafts')[0].options.body);
    await f.send('Second instruction');await settle();
    assert.equal(posts(f,'/tasks/task-one/drafts').length,1);
    assert.match(f.$('conversation').textContent,/First instruction/);assert.match(f.$('conversation').textContent,/Second instruction/);
    await findButton(f.$('conversation'),'核对这次修订').onclick();await settle();
    assert(f.calls.some(c=>c.url==='/api/tasks/task-one/drafts/turns/'+first.request_id));
    assert.equal(posts(f,'/tasks/task-one/drafts').length,1);
    assert.match(f.$('conversation').textContent,/Second instruction/);
    assert(findButton(f.$('conversation'),'用保留的要求修改草稿'));
  });

  await test('restored delivery confirmation stays bound to the reviewed draft version', async () => {
    const storage=new Map(),tasks=[task({my_draft:{status:'complete',request_id:'base'}})];
    const f=fixture(undefined,{storage,tasks,draftId:'reviewed',
      taskAction:()=>({action:'deliver',task_id:'task-one',expected_revision:3})});
    await f.login('bob');await settle();await f.send('交付');await settle();
    assert.equal(JSON.parse(storage.get('agenct.task-actions.v1')).items[0][1].draftId,'reviewed');
    assert(!storage.get('agenct.task-actions.v1').includes('Auto draft'));
    const again=fixture(undefined,{storage,tasks,draftId:'newer'});await again.login('bob');await settle();
    await findButton(again.$('conversation'),'确认交付').onclick();await settle();
    assert.equal(posts(again,'/actions').length,0);
    assert.match(again.$('status').textContent,/草稿版本已变更/);
  });

  await test('successful confirmed action advances an empty composer but never retargets in-flight input', async () => {
    for (const typed of [false,true]) {
      const f=fixture(undefined,{tasks:[task({status:'awaiting_owner',revision:0})],
        taskAction:()=>({action:'accept',task_id:'task-one',expected_revision:0})});
      await f.login('bob');await settle();await f.send('接手');await settle();
      f.tasks([task({status:'working',revision:1})]);
      f.hold();
      const action = findButton(f.$('conversation'),'确认接手').onclick(); await f.ready();
      if(typed)f.$('text').value='Written against old version';
      f.release(); await action; await settle();
      assert.match(f.$('task-context-details').textContent,typed?/失效/:/版本：1/);
      if(typed)assert.equal(f.$('text').value,'Written against old version');
    }
  });

  await test('integrated task confirmation errors stay readable and stale automatic context is announced', async () => {
    const f=fixture(undefined,{tasks:[task({status:'ready_for_acceptance'})],
      taskAction:()=>({action:'approve',task_id:'task-one',expected_revision:3})});
    await f.login('alice');await settle();await f.send('验收');await settle();
    await f.send('确认退回');await settle();
    assert.match(f.$('status').textContent,/动作与当前/);
    assert(!f.$('status').classList.contains('show'));
    assert.equal(posts(f,'/actions').length,0);
    f.tasks([task({status:'ready_for_acceptance',revision:4})]);await f.$('refresh').onclick();await settle();
    assert.match(f.$('task-context-status').textContent,/失效/);
    assert.equal(f.$('text').value,'确认退回');
    f.$('status').onkeydown({key:'Escape',preventDefault(){}});assert.equal(f.$('status').textContent,'');
  });
  console.log('Client regression tests passed');

})().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
