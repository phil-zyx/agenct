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
  const state = { user: 'alice', authenticated: true, post: 'complete', receipt: 'complete', receiptCode: 200, tasks: [] };
  const taskContexts = new Map();
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
        const task = state.tasks.find(task => task.id === body.selected_task?.task_id);
        taskContexts.set(body.request_id, task ? {
          id: task.id, revision: task.revision, requester: task.requester, owner: task.owner,
          status: task.status, summary: task.summary, result: task.result || null
        } : null);
        if (state.post === 'stall') return; // Leave this POST unanswered until the browser aborts.
        if (state.post === 'network-failure') { await route.abort('failed'); return; }
        if (state.post === 'complete') messages.push({ role: 'assistant', status: 'complete', content: 'Saved reply' });
        data = { turn: { request_id: body.request_id, status: state.post }, reply: state.post === 'complete' ? 'Saved reply' : null, task_context: taskContexts.get(body.request_id) };
      } else data = messages;
    }
    if (pathname.startsWith('/api/turns/')) {
      if (state.receiptCode !== 200) {
        await route.fulfill({ status: state.receiptCode, json: { error: 'Not found' } }); return;
      }
      const id = pathname.split('/').at(-1);
      data = { request_id: id, status: state.receipt, response: state.receipt === 'complete'
        ? { turn: { request_id: id, status: 'complete' }, reply: 'Recovered saved reply', task_context: taskContexts.get(id) || null } : null };
      if (state.receipt === 'complete') messages = [{ role: 'assistant', status: 'complete', content: 'Recovered saved reply' }];
    }
    if (pathname === '/api/tasks' && method === 'POST') {
      await route.fulfill({ status: 405, json: { error: 'Method Not Allowed' } }); return;
    }
    if (pathname === '/api/tasks' && method === 'GET') data = state.tasks;
    if (pathname === '/api/profile') data = { revision: 0, agents: [] };
    if (pathname === '/api/profile/catalog') data = { roles: [], models: [] };
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

function taskFixture(id = 'task-alpha', revision = 7) {
  return { id, revision, requester: 'alice', owner: 'bob', status: 'ready_for_acceptance',
    summary: `Public summary ${id}`, result: `Delivered result ${id}` };
}

async function showTasks(f, tasks) {
  f.state.tasks = tasks;
  await f.page.locator('#refresh').click();
  await f.page.waitForFunction(count => [...document.querySelectorAll('#conversation button')]
    .filter(button => button.textContent === '讨论此任务').length === count, tasks.length);
}

function discussButtons(f) { return f.page.getByRole('button', { name: '讨论此任务', exact: true }); }

async function settled(f) {
  await f.page.waitForFunction(() => !document.getElementById('send').disabled);
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
      await showTasks(f, [taskFixture()]); await discussButtons(f).click();
      await f.send('stalled question');
      assert.equal(await discussButtons(f).isDisabled(), true);
      assert.equal(await f.page.locator('#clear-task-context').isDisabled(), true);
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
    {
      const f = await fixture(browser, origin);
      const alpha = taskFixture(), beta = taskFixture('task-beta', 3);
      await showTasks(f, [alpha, beta]);
      const writesBefore = f.calls.filter(call => call.method !== 'GET').length;
      await discussButtons(f).nth(0).click();
      assert.equal(await f.page.locator('#task-context').isVisible(), true);
      const details = await f.page.locator('#task-context-details').textContent();
      for (const value of [alpha.id, String(alpha.revision), alpha.summary, alpha.result]) assert(details.includes(value));
      assert.equal(f.calls.filter(call => call.method !== 'GET').length, writesBefore, 'Selecting context is not a mutation');
      await discussButtons(f).nth(1).click();
      assert((await f.page.locator('#task-context-details').textContent()).includes(beta.id));
      await f.send('/agents'); await settled(f);
      await f.send('/delegate bob only this explicit summary'); await settled(f);
      assert.equal(f.posts().length, 0, 'Profile and delegation commands do not send ordinary chat');
      assert.equal(await f.page.locator('#task-context').isVisible(), true);
      await f.send('Explain the selected result'); await settled(f);
      assert.deepEqual(f.posts()[0].body.selected_task, { task_id: beta.id, expected_revision: beta.revision });
      assert.deepEqual(Object.keys(f.posts()[0].body).sort(), ['message', 'request_id', 'selected_task']);
      assert((await f.page.locator('#status').textContent()).includes(`本次任务上下文：${beta.id} · 版本 ${beta.revision}`));
      assert.equal(await f.page.locator('#task-context').isVisible(), false);
      await f.send('Now answer without task context'); await settled(f);
      assert.equal(Object.hasOwn(f.posts()[1].body, 'selected_task'), false);
      assert(!f.calls.some(call => call.pathname.includes('/actions')));
      await f.close(); console.log('PASS Chromium explicit single task selection, command preservation and next-chat-only payload');
    }
    for (const change of ['revision', 'disappearance']) {
      const f = await fixture(browser, origin);
      const alpha = taskFixture(); await showTasks(f, [alpha]);
      await discussButtons(f).click();
      f.state.tasks = change === 'revision' ? [{ ...alpha, revision: 8, summary: 'Updated public summary' }] : [];
      await f.page.locator('#refresh').click();
      if (change === 'revision') await f.page.waitForFunction(() => document.getElementById('conversation').textContent.includes('Updated public summary'));
      else await f.page.waitForFunction(() => ![...document.querySelectorAll('#conversation button')].some(button => button.textContent === '讨论此任务'));
      await f.page.locator('#text').fill('Do not silently drop my selected context');
      await f.page.locator('#form').evaluate(form => form.requestSubmit());
      assert.equal(f.posts().length, 0, 'Invalid selection must block even direct form submission');
      assert.equal(await f.page.locator('#task-context').isVisible(), true);
      assert((await f.page.locator('#task-context-details').textContent()).includes('失效'));
      if (change === 'revision') {
        await discussButtons(f).click();
        await f.send('Discuss the new revision'); await settled(f);
        assert.deepEqual(f.posts()[0].body.selected_task, { task_id: alpha.id, expected_revision: 8 });
      } else {
        await f.page.locator('#clear-task-context').click();
        await f.send('Explicitly continue without context'); await settled(f);
        assert.equal(Object.hasOwn(f.posts()[0].body, 'selected_task'), false);
      }
      await f.close(); console.log(`PASS Chromium ${change} invalidates unsent task selection without silent context loss`);
    }
    {
      const f = await fixture(browser, origin);
      await showTasks(f, [taskFixture()]); await discussButtons(f).click();
      await f.page.locator('#logout').click(); await f.login('bob');
      assert.equal(await f.page.locator('#task-context').isVisible(), false);
      await f.send('New identity question'); await settled(f);
      assert.equal(Object.hasOwn(f.posts()[0].body, 'selected_task'), false);
      await f.close(); console.log('PASS Chromium logout and identity change clear task selection');
    }
    for (const outcome of ['recover', 'new-attempt', 'new-attempt-before-reload']) {
      const f = await fixture(browser, origin);
      const alpha = taskFixture(); await showTasks(f, [alpha]); await discussButtons(f).click();
      await f.uncertain('Discuss this task privately');
      const original = structuredClone(f.posts()[0].body);
      assert.deepEqual(original.selected_task, { task_id: alpha.id, expected_revision: 7 });
      assert.equal(await discussButtons(f).isDisabled(), true);
      assert.equal(await f.page.locator('#clear-task-context').isDisabled(), true);
      f.state.tasks = [{ ...alpha, revision: 8, summary: 'Changed while previous turn unresolved' }];
      await f.page.locator('#refresh').click();
      await f.page.waitForFunction(() => document.getElementById('conversation').textContent.includes('Changed while previous turn unresolved'));
      assert.equal(await discussButtons(f).isDisabled(), true);
      assert.equal(await f.page.locator('#clear-task-context').isDisabled(), true);
      assert.deepEqual(f.posts()[0].body, original, 'Refresh must not retarget the in-flight task context');
      if (outcome !== 'new-attempt-before-reload') { await f.page.reload(); await f.login(); }
      await discussButtons(f).waitFor();
      assert.equal(await discussButtons(f).isDisabled(), true);
      assert.equal(await f.page.locator('#send').isDisabled(), true);
      assert.equal(f.posts().length, 1, 'Reload must not replay selected-context chat');
      if (outcome === 'new-attempt-before-reload') assert.equal(await f.page.locator('#task-context').isVisible(), true);
      if (outcome === 'recover') {
        await f.page.locator('#check-turn').click();
        await f.page.waitForSelector('#turn-recovery.hide', { state: 'attached' });
        await settled(f);
      } else {
        f.page.once('dialog', dialog => dialog.dismiss()); await f.page.locator('#new-turn').click();
        assert.equal(await f.page.locator('#send').isDisabled(), true);
        f.page.once('dialog', dialog => dialog.accept()); await f.page.locator('#new-turn').click();
      }
      assert.equal(f.posts().length, 1, 'Recovery/new attempt is not a resend');
      assert.equal(await f.page.locator('#task-context').isVisible(), false);
      f.state.post = 'complete'; await f.send('An explicitly new ordinary question'); await settled(f);
      assert.notEqual(f.posts()[1].body.request_id, original.request_id);
      assert.equal(Object.hasOwn(f.posts()[1].body, 'selected_task'), false);
      await f.close(); console.log(`PASS Chromium selected-context uncertain reload and ${outcome} isolation`);
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
