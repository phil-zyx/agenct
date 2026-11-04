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
    this.classList = { add() {}, remove() {} };
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
  let stallReads = false;
  let revision = 0;
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
      if (stallReads && options.method === 'GET') await new Promise((resolve, reject) => {
        options.signal.addEventListener('abort', () => reject(Error('Read timed out')), { once: true });
      });
      if (failReads && options.method === 'GET') throw Object.assign(Error('Read unavailable'), { httpStatus: failReads });
      const identity = (options.headers.Authorization || auth.user || '').endsWith('bob') ? 'bob' : 'alice';
      let data = url.endsWith('/me') ? { user: identity, runtime: 'mock' } : [];
      if (url.endsWith('/proposals') && options.method === 'GET') {
        data = [{ id: 1, owner: 'bob', summary: 'Exact <summary>\nsecond line', status: 'pending', revision }];
      }
      if (url.endsWith('/profile/catalog')) data = { roles: ['coordinator', 'researcher'], models: [{ id: 'server-default' }], max_agents: 8 };
      if (url.endsWith('/profile')) data = { revision: 0, agents: [{ id: 'butler', name: '管家', role: 'coordinator', model_ref: 'server-default', enabled: true }] };
      return { ok: true, json: async () => data };
    }
  });
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'profiles-ui.js'), 'utf8'), context);
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'client.js'), 'utf8'), context);
  return {
    $, calls, timers,
    ready: () => new Promise(resolve => setImmediate(resolve)),
    change() { revision++; },
    stallReads() { stallReads = true; },
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
  console.log('21 client regression tests passed');
})().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
