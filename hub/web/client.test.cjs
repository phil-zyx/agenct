const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { webcrypto } = require('node:crypto');

class Element {
  constructor() {
    this.value = '';
    this.textContent = '';
    this.children = [];
    this.disabled = false;
    const classes = new Set();
    this.classList = { add: value => classes.add(value), remove: value => classes.delete(value), contains: value => classes.has(value) };
  }
  append(element) { this.children.push(element); }
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
  let postTurnStatus = 'complete', checkTurnStatus = 'complete', turnFailure = 0;
  let tasks = settings.tasks || [];
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
    createElement: () => new Element(),
    addEventListener: (name, callback) => listeners.set(name, callback)
  };
  let failNextPost = false;
  const $ = id => {
    if (!elements.has(id)) elements.set(id, new Element());
    return elements.get(id);
  };
  const context = vm.createContext({
    document,
    setTimeout: (callback, delay) => { const id = ++timerId; timers.set(id, { callback, delay }); return id; },
    clearTimeout: id => timers.delete(id),
    AbortController,
    TextEncoder,
    crypto: webcrypto,
    Date,
    prompt: () => '',
    confirm: text => { confirmations.push(text); return confirmation; },
    sessionStorage: {
      getItem: key => { if (settings.storageFailure) throw Error('Storage unavailable'); return storage.get(key) || null; },
      setItem: (key, value) => { if (settings.storageFailure) throw Error('Storage unavailable'); storage.set(key, value); },
      removeItem: key => { if (settings.storageFailure) throw Error('Storage unavailable'); storage.delete(key); }
    },
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
        data = [{ id: 1, owner: 'bob', summary: 'Exact <summary>\nsecond line', status: 'pending', revision }];
      }
      if (url === '/api/tasks' && options.method === 'POST') {
        return { ok: false, status: 405, json: async () => ({ error: 'Method Not Allowed' }) };
      }
      if (url === '/api/tasks' && options.method === 'GET') data = tasks;
      if (url === '/api/messages' && options.method === 'POST') {
        const requestId = JSON.parse(options.body).request_id;
        data = { reply: postTurnStatus === 'complete' ? 'Saved chat reply' : null,
          runtime_profile: responseProfileOverride === undefined ? turnProfiles.get(requestId) : responseProfileOverride,
          task_context: responseContextOverride === undefined ? turnContexts.get(requestId) : responseContextOverride,
          turn: { request_id: requestId, status: postTurnStatus } };
      }
      if (url.startsWith('/api/turns/')) {
        if (turnFailure) return { ok: false, status: turnFailure, json: async () => ({ error: 'Receipt unavailable' }) };
        const requestId = decodeURIComponent(url.split('/').at(-1));
        data = { request_id: requestId, status: checkTurnStatus,
          response: checkTurnStatus === 'complete' ? { reply: 'Saved chat reply',
            runtime_profile: responseProfileOverride === undefined ? turnProfiles.get(requestId) : responseProfileOverride,
            task_context: responseContextOverride === undefined ? turnContexts.get(requestId) ?? null : responseContextOverride,
            turn: { request_id: requestId, status: 'complete' } } : null };
      }
      if (url.endsWith('/profile/catalog')) data = { roles: ['coordinator', 'researcher'], models: [{ id: 'server-default' }], max_agents: 8 };
      if (url.endsWith('/profile')) data = profile;
      return { ok: true, json: async () => data };
    }
  });
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'profiles-ui.js'), 'utf8'), context);
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'worker-ui.js'), 'utf8'), context);
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'client.js'), 'utf8'), context);
  return {
    $, calls, timers, storage, confirmations, turnContexts, turnProfiles,
    profile(value) { profile = value; },
    responseProfile(value) { responseProfileOverride = value; },
    role(id) { return $('role-options').children.find(button => button.textContent.endsWith(' · ' + id)); },
    async chooseRole(id = 'research') { await $('choose-role').onclick(); this.role(id).onclick(); },
    tasks(value) { tasks = value; },
    responseContext(value) { responseContextOverride = value; },
    messageFailure(status, code) { postFailure = status ? { status, code } : null; },
    discussion(id) {
      const card = $('conversation').children.find(card => card.textContent.startsWith('任务 ·') && card.textContent.includes('任务编号：' + id + ' ·'));
      return card?.children.find(button => button.textContent === '讨论此任务');
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

async function test(name, run) {
  await run();
  console.log('PASS ' + name);
}

const task = (overrides = {}) => ({ id: 'task-one', revision: 3, requester: 'alice', owner: 'bob',
  status: 'working', summary: 'Shared <summary>\nsecond line', result: 'Shared delivery result', ...overrides });
const chatPosts = f => f.calls.filter(call => call.url === '/api/messages' && call.options.method === 'POST');

(async () => {
  await test('exact proposal text, explicit confirmation and revision', async () => {
    const f = fixture();
    await f.login('alice');
    const proposal = f.$('conversation').children[0];
    assert.match(proposal.textContent, /接收人：bob/);
    assert.match(proposal.textContent, /Exact <summary>\nsecond line/);
    assert.equal(proposal.children[1].textContent, '确认共享并委派');
    assert.equal(f.calls.filter(call => call.options.method === 'POST').length, 0);
    await proposal.children[1].onclick();
    const call = f.calls.find(call => call.url === '/api/proposals/1/actions');
    assert.deepEqual(JSON.parse(call.options.body), { action: 'confirm', expected_revision: 0 });
  });

  await test('reject proposal without creating a task', async () => {
    const f = fixture();
    await f.login('alice');
    await f.$('conversation').children[0].children[2].onclick();
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
    await oldCard.children[1].onclick();
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
    assert([...f.timers.values()].some(timer => timer.delay === 5000));
  });

  await test('hidden tabs pause polling; logout clears timers and stale polling', async () => {
    const f = fixture();
    await f.login('alice');
    f.visible(false);
    assert.equal(f.timers.size, 0);
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
    for (const delay of [10000, 20000, 40000, 60000, 60000]) {
      await f.tick();
      assert([...f.timers.values()].some(timer => timer.delay === delay));
    }
    assert.equal(f.$('conversation').children[0], original);
    assert.match(f.$('last-refresh').textContent, /无法同步/);
    f.failReads(0);
    await f.tick();
    assert([...f.timers.values()].some(timer => timer.delay === 5000));
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
    assert.equal(f.calls.length, count + 3, 'Manual request receives a fresh batch after in-flight read');
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
    assert([...f.timers.values()].some(value => value.delay === 10000));
  });
  await test('profile command stays in single chat and survives refresh without sending to model', async () => {
    const f = fixture(); await f.login('alice'); await f.send('/agents');
    assert.equal(f.calls.filter(call => call.options.method === 'POST' || call.options.method === 'PUT').length, 0);
    const panel = f.$('conversation').children.at(-1);
    assert.match(panel.textContent, /个人 Agent 配置/);
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
    const count = f.calls.length; await card.children[1].onclick();
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
    assert.match(html, /<details><summary>能力边界、委派示例与恢复说明<\/summary>/);
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
      assert.equal(reloaded.$('send').disabled, true);
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
    const reloaded = fixture(auth, { storage: new Map(f.storage) }); await reloaded.ready();
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
    assert.equal(f.$('send').disabled, true);
    assert.equal([...f.storage.values()][0], receipt);
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
    assert.equal(f.$('send').disabled, true);
    assert.equal([...f.storage.values()][0], receipt);
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
      const f = fixture({ enabled: false }, { tasks: [task()] }); await f.login('alice');
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
    const reloaded = fixture({ enabled: false }, { tasks: [task({ revision: 9 })], storage: f.storage, turnContexts: f.turnContexts });
    await reloaded.login('alice');
    assert.equal(reloaded.$('task-context-details').textContent, '');
    assert.equal(reloaded.discussion('task-one').disabled, true);
    reloaded.discussion('task-one').onclick(); await reloaded.send('another question');
    assert.equal(chatPosts(reloaded).length, 0);
    await reloaded.$('check-turn').onclick();
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
    await second.login('alice'); assert(second.$('choose-role').disabled);
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
    f.failReads(0); f.profile(roles(9)); await f.login('alice');
    assert(f.$('choose-role').disabled); assert(f.$('role-details').textContent.includes('版本 3'));
    await f.$('check-turn').onclick(); assert.equal(f.storage.size, 0);
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
  console.log('Client regression tests passed');

})().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
