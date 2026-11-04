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
      // Intentionally ignore abort to also exercise responses already in flight.
      if (hold) await new Promise(resolve => waiting.push(resolve));
      if (failNextPost && options.method === 'POST') {
        failNextPost = false;
        throw Error('Simulated uncertain network outcome');
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
      if (url === '/api/messages' && options.method === 'POST') {
        const requestId = JSON.parse(options.body).request_id;
        data = { reply: postTurnStatus === 'complete' ? 'Saved chat reply' : null, turn: { request_id: requestId, status: postTurnStatus } };
      }
      if (url.startsWith('/api/turns/')) {
        if (turnFailure) return { ok: false, status: turnFailure, json: async () => ({ error: 'Receipt unavailable' }) };
        const requestId = decodeURIComponent(url.split('/').at(-1));
        data = { request_id: requestId, status: checkTurnStatus,
          response: checkTurnStatus === 'complete' ? { reply: 'Saved chat reply', turn: { request_id: requestId, status: 'complete' } } : null };
      }
      if (url.endsWith('/profile/catalog')) data = { roles: ['coordinator', 'researcher'], models: [{ id: 'server-default' }], max_agents: 8 };
      if (url.endsWith('/profile')) data = { revision: 0, agents: [{ id: 'butler', name: '管家', role: 'coordinator', model_ref: 'server-default', enabled: true }] };
      return { ok: true, json: async () => data };
    }
  });
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'profiles-ui.js'), 'utf8'), context);
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'worker-ui.js'), 'utf8'), context);
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'client.js'), 'utf8'), context);
  return {
    $, calls, timers, storage, confirmations,
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
  console.log('Client regression tests passed');

})().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
