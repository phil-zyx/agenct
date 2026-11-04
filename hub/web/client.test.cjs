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

function fixture() {
  const elements = new Map();
  const calls = [];
  const waiting = [];
  let hold = false;
  let failNextPost = false;
  const $ = id => {
    if (!elements.has(id)) elements.set(id, new Element());
    return elements.get(id);
  };
  const context = vm.createContext({
    document: { getElementById: $, createElement: () => new Element() },
    AbortController,
    crypto: webcrypto,
    Date,
    prompt: () => '',
    fetch: async (url, options) => {
      calls.push({ url, options });
      // Intentionally ignore abort to also exercise responses already in flight.
      if (hold) await new Promise(resolve => waiting.push(resolve));
      if (failNextPost && options.method === 'POST') {
        failNextPost = false;
        throw Error('Simulated uncertain network outcome');
      }
      const identity = options.headers.Authorization.endsWith('bob') ? 'bob' : 'alice';
      let data = url.endsWith('/me') ? { user: identity, runtime: 'mock' } : [];
      if (url.endsWith('/proposals') && options.method === 'GET') {
        data = [{ id: 1, owner: 'bob', summary: 'Exact <summary>\nsecond line', status: 'pending', revision: 0 }];
      }
      return { ok: true, json: async () => data };
    }
  });
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'client.js'), 'utf8'), context);
  return {
    $, calls,
    hold() { hold = true; },
    release() { hold = false; waiting.splice(0).forEach(resolve => resolve()); },
    unhold() { hold = false; },
    failNextPost() { failNextPost = true; },
    async login(identity) { $('token').value = identity; await $('enter').onclick(); },
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
  console.log('7 client regression tests passed');
})().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
