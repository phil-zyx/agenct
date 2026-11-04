/* Real browser regression suite with a local fake API, never a model/worker.
 * Requires Playwright and its Chromium browser (or CHROMIUM_EXECUTABLE_PATH).
 * Run: node hub/web/client.browser.test.cjs
 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const { chromium } = require('playwright');

const STORAGE_KEY = 'agenct.pending-chat-turn.v1';
const server = http.createServer((request, response) => {
  const name = request.url === '/' ? 'index.html' : request.url.slice(1);
  if (!['index.html', 'style.css', 'client.js', 'profiles-ui.js', 'worker-ui.js'].includes(name)) {
    response.writeHead(404).end(); return;
  }
  response.setHeader('Content-Type', name.endsWith('.js') ? 'text/javascript' : name.endsWith('.css') ? 'text/css' : 'text/html');
  response.end(fs.readFileSync(path.join(__dirname, name)));
});

async function fixture(browser, origin, browserMode = false, useClock = false) {
  const context = await browser.newContext();
  const page = await context.newPage();
  if (useClock) await page.clock.install();
  const calls = [], errors = [];
  const state = { user: 'alice', authenticated: true, post: 'complete', receipt: 'complete', receiptCode: 200 };
  let messages = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.route('**/auth/**', async route => {
    if (route.request().url().endsWith('/logout')) {
      state.authenticated = false;
      await route.fulfill({ status: 204 }); return;
    }
    await route.fulfill({ json: { enabled: browserMode, authenticated: state.authenticated, user: state.user, csrf_token: 'fixture-csrf' } });
  });
  await page.route('**/api/**', async route => {
    const request = route.request();
    const pathname = new URL(request.url()).pathname;
    const method = request.method(), body = request.postDataJSON();
    calls.push({ pathname, method, body });
    const user = browserMode ? state.user : request.headers().authorization.slice('Bearer '.length);
    let data = [];
    if (pathname === '/api/me') data = { user, runtime: 'mock' };
    if (pathname === '/api/messages') {
      if (method === 'POST') {
        if (state.post === 'stall') return; // Leave this POST unanswered until the browser aborts.
        if (state.post === 'network-failure') { await route.abort('failed'); return; }
        if (state.post === 'complete') messages.push({ role: 'assistant', status: 'complete', content: 'Saved reply' });
        data = { turn: { request_id: body.request_id, status: state.post }, reply: state.post === 'complete' ? 'Saved reply' : null };
      } else data = messages;
    }
    if (pathname.startsWith('/api/turns/')) {
      if (state.receiptCode !== 200) {
        await route.fulfill({ status: state.receiptCode, json: { error: 'Not found' } }); return;
      }
      const id = pathname.split('/').at(-1);
      data = { request_id: id, status: state.receipt, response: state.receipt === 'complete'
        ? { turn: { request_id: id, status: 'complete' }, reply: 'Recovered saved reply' } : null };
      if (state.receipt === 'complete') messages = [{ role: 'assistant', status: 'complete', content: 'Recovered saved reply' }];
    }
    if (pathname === '/api/proposals' && method === 'GET') data = [{ id: 1, owner: 'bob', summary: 'Share only this exact summary', status: 'pending', revision: 0 }];
    await route.fulfill({ json: data });
  });
  await page.goto(origin);
  async function login(user = 'alice') {
    await page.locator('#token').fill(user); await page.locator('#enter').click();
    await page.waitForFunction(user => document.getElementById('identity').textContent.startsWith(user), user);
  }
  if (!browserMode) await login(); else await page.waitForSelector('#room:not(.hide)');
  return {
    page, context, calls, state, errors, login,
    posts: () => calls.filter(call => call.pathname === '/api/messages' && call.method === 'POST'),
    stored: () => page.evaluate(key => sessionStorage.getItem(key), STORAGE_KEY),
    async send(text) { await page.locator('#text').fill(text); await page.locator('#send').click(); },
    async uncertain(text = 'private question') {
      state.post = 'network-failure'; await this.send(text);
      await page.waitForFunction(() => !document.getElementById('check-turn').disabled);
    },
    async close() { assert.deepEqual(errors, []); await context.close(); }
  };
}

(async () => {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const browser = await chromium.launch({ headless: true,
    ...(process.env.CHROMIUM_EXECUTABLE_PATH ? { executablePath: process.env.CHROMIUM_EXECUTABLE_PATH } : {}) });
  try {
    {
      const f = await fixture(browser, origin);
      await f.uncertain();
      const id = f.posts()[0].body.request_id;
      assert.equal(await f.page.locator('#send').isDisabled(), true);
      assert.match(await f.page.locator('#turn-status').textContent(), new RegExp(id));
      assert.deepEqual(JSON.parse(await f.stored()), { id, user: 'alice', browserMode: false });
      await f.page.locator('#text').fill('changed unsent draft');
      await f.page.locator('#text').evaluate(element => element.form.requestSubmit());
      await f.page.locator('#refresh').click();
      assert.equal(f.posts().length, 1);
      for (const state of ['pending', 'uncertain']) {
        f.state.receipt = state; await f.page.locator('#check-turn').click();
        await f.page.waitForFunction(() => !document.getElementById('check-turn').disabled);
        assert.equal(await f.page.locator('#send').isDisabled(), true);
      }
      f.state.receiptCode = 404; await f.page.locator('#check-turn').click();
      await f.page.waitForFunction(() => document.getElementById('status').textContent.includes('未找到'));
      assert.equal(await f.page.locator('#send').isDisabled(), true);
      f.state.receiptCode = 200; f.state.receipt = 'complete';
      await f.page.locator('#check-turn').click();
      await f.page.waitForSelector('#turn-recovery.hide', { state: 'attached' });
      await f.page.waitForFunction(() => document.getElementById('conversation').textContent.includes('Recovered saved reply'));
      assert.equal(await f.page.locator('#text').inputValue(), 'changed unsent draft');
      assert.equal(await f.stored(), null);
      assert.equal(f.posts().length, 1);
      // Recovery never bypasses the existing sharing boundary.
      assert(!f.calls.some(call => call.pathname === '/api/tasks' && call.method === 'POST'));
      assert(!f.calls.some(call => call.pathname.includes('/actions')));
      await f.page.getByRole('button', { name: '确认共享并委派', exact: true }).click();
      await f.page.waitForFunction(() => document.getElementById('status').textContent.includes('已确认共享摘要'));
      assert(f.calls.some(call => call.pathname === '/api/proposals/1/actions' && call.body.action === 'confirm'));
      await f.close(); console.log('PASS Chromium uncertain/pending/404/complete recovery and proposal boundary');
    }
    {
      const f = await fixture(browser, origin); await f.uncertain();
      const original = f.posts()[0].body.request_id;
      f.page.once('dialog', async dialog => { assert.match(dialog.message(), /结果仍未知.*重复处理/); await dialog.dismiss(); });
      await f.page.locator('#new-turn').click(); assert.equal(await f.page.locator('#send').isDisabled(), true);
      f.page.once('dialog', dialog => dialog.accept()); await f.page.locator('#new-turn').click();
      assert.equal(f.posts().length, 1, 'Starting over must not automatically send');
      assert.equal(await f.stored(), null);
      f.state.post = 'complete'; await f.send('private question');
      await f.page.waitForFunction(() => !document.getElementById('send').disabled);
      assert.equal(f.posts().length, 2); assert.notEqual(f.posts()[1].body.request_id, original);
      await f.close(); console.log('PASS Chromium new-attempt cancellation/confirmation and fresh request ID');
    }
    {
      const f = await fixture(browser, origin, true);
      await f.uncertain(); const receipt = await f.stored();
      f.state.authenticated = false; await f.page.reload();
      await f.page.waitForSelector('#oidc-login:not(.hide)');
      assert.equal(await f.page.locator('#room').isVisible(), false);
      assert.equal(await f.stored(), receipt);
      f.state.authenticated = true; await f.page.reload();
      await f.page.waitForSelector('#room:not(.hide)');
      assert.equal(await f.page.locator('#send').isDisabled(), true);
      assert.equal(await f.stored(), receipt);
      assert.equal(f.posts().length, 1);
      await f.page.locator('#check-turn').click();
      await f.page.waitForSelector('#turn-recovery.hide', { state: 'attached' });
      assert.equal(await f.stored(), null);
      await f.close(); console.log('PASS Chromium unauthenticated reload and same-owner reauthentication retain recovery');
    }
    {
      const f = await fixture(browser, origin, false, true); f.state.post = 'stall';
      await f.send('stalled question');
      await f.page.waitForFunction(key => !!sessionStorage.getItem(key), STORAGE_KEY);
      await f.page.clock.fastForward(90001);
      await f.page.waitForFunction(() => document.getElementById('status').textContent.includes('等待超时'));
      assert.equal(await f.page.locator('#check-turn').isDisabled(), false);
      assert.equal(await f.page.locator('#new-turn').isDisabled(), false);
      assert.equal(await f.page.locator('#send').isDisabled(), true);
      assert.equal(f.posts().length, 1);
      await f.page.locator('#check-turn').click();
      await f.page.waitForSelector('#turn-recovery.hide', { state: 'attached' });
      assert.equal(await f.stored(), null);
      assert.equal(f.posts().length, 1);
      await f.close(); console.log('PASS Chromium stalled POST times out to read-only receipt recovery');
    }
    for (const browserMode of [false, true]) {
      const f = await fixture(browser, origin, browserMode); await f.uncertain();
      const receipt = await f.stored(); await f.page.reload();
      if (!browserMode) {
        assert.equal(await f.page.locator('#room').isVisible(), false);
        assert.equal(await f.page.locator('#token').inputValue(), '');
        await f.login();
      } else await f.page.waitForSelector('#room:not(.hide)');
      assert.equal(await f.stored(), receipt);
      assert.equal(await f.page.locator('#send').isDisabled(), true);
      assert.equal(await f.page.locator('#text').inputValue(), '');
      assert.equal(f.posts().length, 1);
      if (browserMode) {
        f.state.user = 'bob'; await f.page.reload(); await f.page.waitForSelector('#room:not(.hide)');
      } else {
        await f.page.locator('#logout').click(); await f.login('bob');
      }
      assert.equal(await f.stored(), null);
      assert.equal(await f.page.locator('#turn-recovery').isVisible(), false);
      assert.match(await f.page.locator('#identity').textContent(), /^bob/);
      await f.uncertain('bob question'); await f.page.locator('#logout').click();
      assert.equal(await f.stored(), null);
      assert.equal(await f.page.locator('#room').isVisible(), false);
      await f.close(); console.log(`PASS Chromium ${browserMode ? 'cookie' : 'bearer'} refresh/logout/owner isolation`);
    }
  } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; }).finally(() => server.close());
