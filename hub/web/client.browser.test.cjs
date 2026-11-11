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
  if (!/^(?:brand\/)?[a-z][a-z0-9-]*\.(?:html|css|js|png|svg)$/.test(name) || !fs.existsSync(path.join(__dirname, name))) {
    response.writeHead(404).end(); return;
  }
  response.setHeader('Content-Type', name.endsWith('.svg') ? 'image/svg+xml' : name.endsWith('.png') ? 'image/png' : name.endsWith('.js') ? 'text/javascript' : name.endsWith('.css') ? 'text/css' : 'text/html');
  response.end(fs.readFileSync(path.join(__dirname, name)));
});

async function fixture(browser, origin, browserMode = false, useClock = false) {
  const context = await browser.newContext();
  const page = await context.newPage();
  if (useClock) await page.clock.install();
  const calls = [], errors = [];
  const state = { user: 'alice', authenticated: true, post: 'complete', receipt: 'complete', receiptCode: 200, tasks: [], reply: 'Saved reply', proposals: [{ id: 1, owner: 'bob', summary: 'Share only this exact summary', status: 'pending', revision: 0 }] };
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
        if (state.post === 'complete') messages.push({ role: 'assistant', status: 'complete', content: state.reply });
        // Stand-in for the model's reading of a plain-language task reply.
        const context = taskContexts.get(body.request_id);
        const action = context && state.taskAction ? state.taskAction(body.message, context) : null;
        data = { turn: { request_id: body.request_id, status: state.post }, reply: state.post === 'complete' ? state.reply : null, task_context: context,
          task_action: action ? { task_id: context.id, expected_revision: context.revision, instruction: null, ...action } : null };
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
    if (pathname === '/api/proposals' && method === 'GET') data = state.proposals;
    if (/^\/api\/proposals\/[^/]+\/actions$/.test(pathname) && method === 'POST') {
      const proposal = state.proposals.find(p => String(p.id) === pathname.split('/')[3]);
      if (proposal) { proposal.status = body.action === 'reject' ? 'rejected' : 'confirmed'; proposal.revision++; }
    }
    if (/^\/api\/tasks\/[^/]+\/drafts(\/turns\/[^/]+)?$/.test(pathname)) {
      // Like the Hub: a completed draft becomes the owner's my_draft for this version.
      const task = state.tasks.find(t => t.id === pathname.split('/')[3]);
      const receipt = id => ({ kind: 'draft_delivery', task_id: task.id, request_id: id, status: 'complete', current_stale: false,
        response: { draft: { id: 'draft-' + id, seq: 1, content: 'Private generated draft\nSecond paragraph', task_revision: task.revision }, task_context: { id: task.id }, runtime_profile: { revision: 0 } } });
      if (pathname.includes('/turns/')) data = receipt(pathname.split('/').at(-1));
      else if (method === 'GET') data = { kind: 'draft_delivery', task_id: task.id, items: [] };
      else { data = receipt(body.request_id); task.my_draft = { status: 'complete', automatic: false, request_id: body.request_id }; }
    }
    if (/^\/api\/tasks\/[^/]+\/actions$/.test(pathname) && method === 'POST') {
      const task = state.tasks.find(t => t.id === pathname.split('/')[3]);
      if (task && body.action === 'deliver') { task.status = 'ready_for_acceptance'; task.result = body.result; task.revision++; }
    }
    await route.fulfill({ json: data });
  });
  await page.goto(origin);
  async function login(user = 'alice') {
    await page.waitForFunction(() => !document.getElementById('room').classList.contains('hide') || !document.getElementById('dev-login').classList.contains('hide'));
    if (await page.locator('#room').isVisible()) {
      if ((await page.locator('#identity').textContent()).startsWith(user + ' ')) return;
      await page.locator('#logout').click();
    }
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
  await f.page.locator('#open-chat').click();
  await f.page.locator('#refresh').evaluate(button => button.click());
  await f.page.waitForFunction(count => [...document.querySelectorAll('#tasks button')]
    .filter(button => button.textContent === '讨论此任务').length === count, tasks.length);
  await f.page.locator('#toggle-tasks').click();
  await f.page.locator('#tab-tasks').click();
}

function discussButtons(f) { return f.page.getByRole('button', { name: '讨论此任务', exact: true }); }

async function settled(f) {
  await f.page.waitForFunction(() => !document.getElementById('send').disabled);
}

if (require.main === module) (async () => {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const browser = await chromium.launch({ headless: true,
    ...(process.env.CHROMIUM_EXECUTABLE_PATH ? { executablePath: process.env.CHROMIUM_EXECUTABLE_PATH } : {}) });
  try {
    {
      const f = await fixture(browser, origin);
      const own = { id: 'alice', display_name: 'Alice', color: 'green', status: 'active', revision: 0, admin: true };
      const other = { id: 'bob', display_name: 'Bob', color: 'blue', status: 'active', revision: 0, admin: false };
      await f.page.route('**/api/me', route => route.fulfill({ json: { user: 'alice', runtime: 'mock', member: own } }));
      await f.page.route('**/api/members', route => route.fulfill({ json: [own, other] }));
      const writes = [];
      await f.page.route('**/api/members/*', route => {
        const data = route.request().postDataJSON(); writes.push(data);
        Object.assign(own, data, { revision: own.revision + 1 }); return route.fulfill({ json: own });
      });
      await f.page.locator('#open-account').click();
      await f.page.getByRole('textbox', { name: 'alice 显示姓名', exact: true }).fill('Alice Chen');
      await f.page.getByRole('button', { name: '保存资料', exact: true }).first().click();
      await f.page.waitForFunction(() => document.querySelector('input[aria-label="alice 显示姓名"]')?.value === 'Alice Chen');
      assert.equal(writes[0].expected_revision, 0); assert.equal(writes[0].display_name, 'Alice Chen');
      assert.equal(await f.page.getByRole('button', { name: '停用账号', exact: true }).count(), 1);
      await f.page.keyboard.press('Escape');
      assert.equal(await f.page.locator('#contacts-view').isVisible(), false);
      await f.close(); console.log('PASS Chromium account profile editing and admin member controls');
    }
    {
      const f = await fixture(browser, origin);
      await f.page.route('**/api/contacts', route => route.fulfill({ status: 404, body: '' }));
      await f.page.locator('#open-contacts').click();
      await f.page.getByText('无法读取联系人：服务端未提供此接口，请更新并重启本地服务', { exact: true }).waitFor();
      await f.page.route('**/api/contacts', route => route.fulfill({ json: ['bob', 'carol'] }));
      await f.page.route('**/api/contacts/*/profile', route => route.fulfill({ json: { public: {} } }));
      await f.page.getByRole('button', { name: '重新读取', exact: true }).click();
      await f.page.locator('#text').fill('My unsent draft');
      await f.page.locator('#open-contacts').click();
      await f.page.locator('.contact-row').first().waitFor();
      await f.page.getByRole('searchbox', { name: '搜索联系人' }).fill('bob');
      assert.equal(await f.page.locator('.contact-row').count(), 1);
      await f.page.locator('.contact-row').click();
      assert.equal(f.calls.filter(c => c.method === 'POST').length, 0);
      await f.page.getByRole('button', { name: '请他帮忙', exact: true }).click();
      await f.page.getByRole('textbox', { name: '委派摘要', exact: true }).fill('Review the document');
      await f.page.getByRole('button', { name: '生成委派提案', exact: true }).click();
      await f.page.waitForSelector('#contacts-view.hide', { state: 'attached' });
      const proposal = f.calls.find(c => c.pathname === '/api/proposals' && c.method === 'POST');
      assert.equal(proposal.body.owner, 'bob'); assert.equal(proposal.body.summary, 'Review the document');
      assert.equal(await f.page.locator('#text').inputValue(), 'My unsent draft');
      assert(!f.calls.some(c => c.pathname.endsWith('/actions')));
      await f.page.locator('#open-contacts').click();
      await f.page.locator('#logout').click();
      assert.equal(await f.page.locator('#contacts-view').textContent(), '');
      await f.close(); console.log('PASS Chromium contacts search, explicit proposal, preserved draft and logout cleanup');
    }
    {
      const f = await fixture(browser, origin);
      await f.page.reload();
      await f.page.waitForSelector('#room:not(.hide)');
      assert((await f.page.locator('#identity').textContent()).startsWith('alice'));
      assert.equal(await f.page.locator('#token').inputValue(), '');
      assert.equal(f.posts().length, 0);
      await f.page.locator('#logout').click();
      await f.page.reload();
      await f.page.waitForSelector('#dev-login:not(.hide)');
      assert.equal(await f.page.locator('#room').isVisible(), false);
      await f.login();
      await f.page.route('**/api/me', route => route.fulfill({ status: 401, json: { error: 'expired token' } }));
      await f.page.reload();
      await f.page.waitForSelector('#dev-login:not(.hide)');
      assert.equal(await f.page.evaluate(() => sessionStorage.getItem('agenct.dev-auth.v1')), null);
      assert.equal(await f.page.locator('#room').isVisible(), false);
      await f.close(); console.log('PASS Chromium development login refresh, logout and invalid-token cleanup');
    }
    {
      const f = await fixture(browser, origin);
      await f.page.setViewportSize({ width: 400, height: 760 });
      await showTasks(f, [taskFixture()]);
      await f.page.locator('#toggle-tasks').evaluate(b => { if (b.getAttribute('aria-pressed') !== 'true') b.click(); });
      // A real status generated by a rejected task action must be above the drawer.
      await f.page.route('**/api/tasks/task-alpha/actions', route => route.fulfill({ status: 409, json: { error: 'Review latest task before retrying' } }));
      // Actions live in the conversation; any status raised while the drawer is open must still show above it.
      await f.page.evaluate(() => status('Review latest task before retrying'));
      await f.page.waitForFunction(() => document.getElementById('status').textContent.includes('Review latest'));
      assert(await f.page.locator('#status').evaluate(el => {
        const r = el.getBoundingClientRect(); return el.contains(document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2));
      }), 'Task error toast must not be covered by the mobile drawer');
      assert.equal(await f.page.locator('#status').evaluate(el => getComputedStyle(el).animationName), 'none', 'critical error must not expire');
      await f.page.locator('#status').focus(); await f.page.keyboard.press('Escape');
      assert.equal(await f.page.locator('#status').textContent(), '');
      await discussButtons(f).click();
      assert.equal(await f.page.locator('.task-pane').isVisible(), false);
      assert.equal(await f.page.locator('#text').evaluate(el => el === document.activeElement), true);
      await f.page.locator('#toggle-tasks').click(); await f.page.locator('#open-chat').click();
      assert.equal(await f.page.locator('.task-pane').isVisible(), false);
      await f.page.locator('#composer-menu-toggle').click();
      await f.page.locator('#help > summary').click();
      assert.equal(await f.page.locator('#help').getAttribute('open'), '');
      await f.page.locator('#composer-menu-toggle').click();
      await f.page.locator('#text').fill('Keep my unsent private text');
      await f.page.locator('#toggle-tasks').click(); await f.page.locator('#open-agents').click();
      await f.page.waitForFunction(() => !document.getElementById('open-agents').disabled);
      assert.equal(await f.page.locator('#profile-view').isVisible(), true);
      assert.equal(await f.page.locator('#conversation .profile-settings').count(), 0);
      assert.equal(await f.page.locator('.chat-pane').isVisible(), true);
      await f.page.setViewportSize({ width: 1280, height: 900 });
      const chatBounds = await f.page.locator('.chat-pane').boundingBox();
      const settingsBounds = await f.page.locator('#profile-view').boundingBox();
      assert(settingsBounds.x >= chatBounds.x + chatBounds.width - 1, 'Settings occupy the right column alongside chat');
      assert.equal(settingsBounds.width, 420);
      await f.page.setViewportSize({ width: 400, height: 800 });
      await f.page.keyboard.press('Escape');
      assert.equal(await f.page.locator('#profile-view').isVisible(), false);
      assert.equal(await f.page.locator('.chat-pane').isVisible(), true);
      assert.equal(await f.page.locator('#text').inputValue(), 'Keep my unsent private text');
      assert.equal(await f.page.locator('.task-pane').isVisible(), false);
      assert.equal(f.posts().length, 0);
      await f.close(); console.log('PASS Chromium narrow navigation, visible task errors and preserved composer draft');
    }
    {
      const f = await fixture(browser, origin, false, true);
      f.state.post = 'stall'; await f.send('Wait for a response');
      await f.page.waitForSelector('#turn-recovery.quiet');
      assert.equal(await f.page.locator('#text').inputValue(), '');
      assert.equal(await f.page.locator('#outgoing-message .msg-body').textContent(), 'Wait for a response');
      assert.equal(await f.page.locator('#outgoing-message').isVisible(), true);
      const snapshot = await f.page.locator('#turn-status').ariaSnapshot();
      assert.match(snapshot, /正在发送/);
      assert.equal(await f.page.locator('#turn-recovery').evaluate(el => getComputedStyle(el).display === 'none'), false);
      await f.page.locator('#logout').click(); await f.close();
      console.log('PASS Chromium accessible in-flight sending status');
    }
    {
      const f = await fixture(browser, origin);
      await f.uncertain();
      const id = f.posts()[0].body.request_id;
      assert.equal(await f.page.locator('#send').isDisabled(), true);
      assert.match(await f.page.locator('#turn-status').textContent(), new RegExp(id));
      assert.deepEqual(JSON.parse(await f.stored()), { id, user: 'alice', browserMode: false });
      await f.page.locator('#text').fill('changed unsent draft');
      await f.page.locator('#text').evaluate(element => element.form.requestSubmit());
      await f.page.locator('#refresh').evaluate(button => button.click());
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
      await f.page.waitForSelector('#turn-recovery.hide', { state: 'attached' });
      await settled(f);
      assert.equal(f.posts().length, 1, 'Reauthentication must only read the receipt');
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
      // In progress with bob: nothing waits on alice, so no automatic binding applies.
      const alpha = { ...taskFixture(), status: 'working' }, beta = { ...taskFixture('task-beta', 3), status: 'working' };
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
      await f.page.getByRole('button', { name: '关闭设置', exact: true }).click();
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
      await f.page.locator('#refresh').evaluate(button => button.click());
      if (change === 'revision') await f.page.waitForFunction(() => document.getElementById('room').textContent.includes('Updated public summary'));
      else await f.page.waitForFunction(() => ![...document.querySelectorAll('#tasks button')].some(button => button.textContent === '讨论此任务'));
      await f.page.locator('#text').fill('Do not silently drop my selected context');
      await f.page.locator('#form').evaluate(form => form.requestSubmit());
      assert.equal(f.posts().length, 0, 'Invalid selection must block even direct form submission');
      assert.equal(await f.page.locator('#task-context').isVisible(), true);
      assert((await f.page.locator('#task-context-details').textContent()).includes('失效'));
      assert.match(await f.page.locator('#task-context-status').ariaSnapshot(), /已失效/);
      assert.equal(await f.page.locator('.chip-more').getAttribute('open'), null, 'announcement does not depend on expanded details');
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
      await f.page.locator('#refresh').evaluate(button => button.click());
      await f.page.waitForFunction(() => document.getElementById('room').textContent.includes('Changed while previous turn unresolved'));
      assert.equal(await discussButtons(f).isDisabled(), true);
      assert.equal(await f.page.locator('#clear-task-context').isDisabled(), true);
      assert.deepEqual(f.posts()[0].body, original, 'Refresh must not retarget the in-flight task context');
      f.state.receipt = 'pending';
      if (outcome !== 'new-attempt-before-reload') { await f.page.reload(); await f.login(); }
      await f.page.locator('#tab-tasks').click();
      await discussButtons(f).waitFor();
      assert.equal(await discussButtons(f).isDisabled(), true);
      assert.equal(await f.page.locator('#send').isDisabled(), true);
      assert.equal(f.posts().length, 1, 'Reload must not replay selected-context chat');
      if (outcome === 'new-attempt-before-reload') assert.equal(await f.page.locator('#task-context').isVisible(), true);
      if (outcome === 'recover') {
        f.state.receipt = 'complete';
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
      f.state.receipt = 'pending';
      const receipt = await f.stored(); await f.page.reload();
      await f.page.waitForSelector('#room:not(.hide)');
      assert.equal(await f.page.locator('#token').inputValue(), '');
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
    await nextStepsInteractions(browser, origin);
  } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; }).finally(() => server.close());

module.exports = { fixture, server, taskFixture, showTasks, settled, nextStepsInteractions };

async function nextStepsInteractions(browser, origin) {
  for (const browserMode of [false, true]) {
    const f = await fixture(browser, origin, browserMode);
    await f.uncertain();
    await f.page.reload();
    if (!browserMode) await f.login();
    await f.page.waitForFunction(key => sessionStorage.getItem(key) === null, STORAGE_KEY);
    await settled(f);
    assert.equal(f.posts().length, 1);
    // Clearing the receipt unlocks sending before its follow-up refresh renders.
    await f.page.waitForFunction(() => document.getElementById('conversation').textContent.includes('Recovered saved reply'));
    assert((await f.page.locator('#conversation').textContent()).includes('Recovered saved reply'));
    assert(f.calls.some(c => c.pathname.startsWith('/api/turns/') && c.method === 'GET'));
    await f.close();
  }
  console.log('PASS Chromium reload automatically reads complete receipts without resending');
  {
    const f = await fixture(browser, origin);
    await f.page.getByRole('button', { name: '修改后确认', exact: true }).click();
    await f.page.getByLabel('共享摘要', { exact: true }).fill('My manually rewritten summary');
    await f.page.getByLabel('接收人', { exact: true }).fill('carol');
    const confirm = f.page.getByRole('button', { name: '确认修改并委派', exact: true });
    assert(await confirm.isDisabled());
    await f.page.getByRole('button', { name: '预览', exact: true }).click();
    assert(!f.calls.some(c => c.pathname.includes('/actions')));
    await confirm.click();
    await f.page.waitForFunction(() => document.getElementById('status').textContent.includes('已'));
    const action = f.calls.find(c => c.pathname === '/api/proposals/1/actions');
    assert.equal(action.body.action, 'confirm');
    assert.equal(action.body.owner, 'carol'); assert.equal(action.body.summary, 'My manually rewritten summary');
    f.state.proposals = [2, 3].map(id => ({ id, owner: 'bob', summary: 'Old pending ' + id, status: 'pending', revision: 0, created_at: '2020-01-01 00:00:00' }));
    await f.page.locator('#refresh').evaluate(button => button.click());
    const group = f.page.locator('details').filter({ has: f.page.locator('summary', { hasText: '2 个未处理的提案' }) });
    await group.waitFor(); assert.equal(await group.getAttribute('open'), null);
    await group.locator('summary').click();
    await group.getByRole('button', { name: /全部拒绝/ }).click();
    await f.page.waitForFunction(() => !document.querySelector('.old-proposals'));
    assert.equal(f.calls.filter(c => c.pathname.match(/\/proposals\/[23]\/actions/) && c.body.action === 'reject').length, 2);
    await f.close(); console.log('PASS Chromium edited proposal preview boundary and old-proposal bulk rejection');
  }
  {
    const f = await fixture(browser, origin);
    await f.page.setViewportSize({ width: 400, height: 900 });
    const task = { id: 'draft-task', revision: 2, requester: 'bob', owner: 'alice', status: 'working', summary: 'Draft a delivery' };
    await showTasks(f, [task]);
    // The pane only points into the conversation, where the work happens by talking.
    f.state.taskAction = message => /交付/.test(message) ? { action: 'deliver' } : { action: 'revise', instruction: message };
    await f.page.getByRole('button', { name: '在对话中处理', exact: true }).click();
    assert.equal(await f.page.locator('.task-pane').isVisible(), false);
    const card = f.page.locator('#task-chat-draft-task');
    assert.equal(await card.getByRole('button').count(), 0, 'no action buttons on the card');
    assert((await f.page.locator('#task-context-label').textContent()).includes('回复给任务'));
    await f.page.locator('#text').fill('帮我写一版，语气正式'); await f.page.locator('#text').press('Enter');
    const retry = card.getByRole('button', { name: '用保留的要求修改草稿', exact: true });
    await retry.waitFor(); assert(await retry.isDisabled());
    assert((await card.textContent()).includes('帮我写一版，语气正式'));
    assert.equal(f.calls.filter(c => c.pathname === '/api/tasks/draft-task/drafts' && c.method === 'POST').length, 0,
      'missing base must not discard the instruction and dispatch an uninstructed draft');
    await card.getByRole('button', { name: '先生成基础草稿', exact: true }).click();
    await f.page.waitForFunction(() => [...document.querySelectorAll('.deferred-revision button')].some(b => !b.disabled));
    const initial = f.calls.filter(c => c.pathname === '/api/tasks/draft-task/drafts' && c.method === 'POST');
    assert.equal(initial.length, 1, 'creating the base does not silently submit the retained instruction');
    assert.equal(initial[0].body.revision, undefined);
    assert((await card.textContent()).includes('帮我写一版，语气正式'));
    await retry.click();
    await f.page.waitForFunction(() => !document.querySelector('.deferred-revision'));
    await card.locator('.agent-draft').waitFor();
    assert((await card.textContent()).includes('Private generated draft'));
    const asked = f.calls.filter(c => c.pathname === '/api/tasks/draft-task/drafts' && c.method === 'POST').at(-1).body;
    assert.deepEqual(asked.revision, { base_draft_id: 'draft-' + initial[0].body.request_id, instruction: '帮我写一版，语气正式' });
    assert(!f.calls.some(c => c.pathname.endsWith('/actions')), 'drafting never delivers');
    await f.page.locator('#text').fill('更正式一些'); await f.page.locator('#text').press('Enter');
    await f.page.waitForFunction(() => document.querySelectorAll('.task-chat .agent-draft').length >= 1);
    const supplement = await (async () => { for (let i = 0; i < 50; i++) {
      const last = f.calls.filter(c => c.pathname === '/api/tasks/draft-task/drafts' && c.method === 'POST').at(-1).body;
      if (last.revision?.instruction === '更正式一些') return last; await f.page.waitForTimeout(100); } })();
    assert.equal(supplement.revision.instruction, '更正式一些');
    await f.page.locator('#text').fill('可以，交付'); await f.page.locator('#text').press('Enter');
    await card.getByRole('button', { name: '先改一下再交付', exact: true }).click();
    const box = card.getByLabel('交付内容', { exact: true });
    await box.fill('Human reviewed delivery\nSecond paragraph');
    assert(!f.calls.some(c => c.pathname.endsWith('/actions')));
    await card.getByRole('button', { name: '确认交付', exact: true }).click();
    await f.page.waitForFunction(() => document.getElementById('conversation').textContent.includes('等待验收'));
    const delivered = f.calls.find(c => c.pathname === '/api/tasks/draft-task/actions').body;
    assert.equal(delivered.result, 'Human reviewed delivery\nSecond paragraph');
    assert(delivered.source_draft_id.startsWith('draft-'));
    await f.close(); console.log('PASS Chromium plain-language drafting, supplement, one-step delivery confirmation and human editing');
  }
  {
    const f = await fixture(browser, origin);
    const tasks = ['draft-alpha', 'draft-beta'].map((id, index) => ({
      id, revision: 2, requester: index ? 'carol' : 'bob', owner: 'alice', status: 'working', summary: id,
      my_draft: { status: 'complete', request_id: 'private-' + id }
    }));
    f.state.taskAction = message => message === '交付' ? { action: 'deliver' } : null;
    const actionPosts = () => f.calls.filter(c => c.method === 'POST' && c.pathname.endsWith('/actions'));
    const card = id => f.page.locator('#task-chat-' + id);
    const select = async index => {
      await showTasks(f, tasks); await discussButtons(f).nth(index).click();
      await f.page.locator('#open-chat').click();
    };
    await select(0);
    await card('draft-alpha').locator('.agent-draft').waitFor();
    await f.send('交付'); await settled(f);
    await card('draft-alpha').getByRole('button', { name: '确认交付', exact: true }).waitFor();
    await f.send('明天天气怎么样'); await settled(f);
    await f.send('嗯'); await settled(f);
    assert.equal(actionPosts().length, 0, 'unrelated chat followed by agreement must not deliver');
    assert.equal(f.posts().at(-1).body.message, '嗯');
    await f.page.reload(); await f.login();
    await card('draft-alpha').locator('.agent-draft').waitFor();
    await card('draft-alpha').getByRole('button', { name: '确认交付', exact: true }).waitFor();
    for (const reply of ['嗯', '好', '可以', '确认', 'OK']) {
      await f.send(reply); await settled(f);
      assert.equal(actionPosts().length, 0, 'reload must not turn ' + reply + ' into authorization');
      assert.equal(f.posts().at(-1).body.message, reply);
    }
    await select(1);
    await card('draft-beta').locator('.agent-draft').waitFor();
    await f.send('确认交付'); await settled(f);
    assert.equal(actionPosts().length, 0, 'explicit confirmation cannot execute another task’s waiting card');
    assert.equal(f.posts().at(-1).body.selected_task.task_id, 'draft-beta');
    // Explicit task selection belongs to one ordinary chat turn, so select B
    // again before asking it to prepare its own waiting confirmation.
    await select(1);
    await f.send('交付'); await settled(f);
    await card('draft-beta').getByRole('button', { name: '确认交付', exact: true }).waitFor();
    await f.send('好的'); await settled(f);
    assert.equal(actionPosts().length, 0, 'agreement cannot choose between multiple waiting tasks');
    await select(0);
    await f.send('确认退回'); await settled(f);
    assert.equal(await f.page.locator('#text').inputValue(), '确认退回');
    assert.equal(actionPosts().length, 0, 'the named action must match');
    await f.send('确认交付'); await settled(f);
    await f.page.waitForFunction(() => document.getElementById('task-chat-draft-alpha') === null);
    assert.equal(actionPosts().length, 1);
    assert.equal(actionPosts()[0].pathname, '/api/tasks/draft-alpha/actions');
    assert.deepEqual(actionPosts()[0].body, {
      action: 'deliver', expected_revision: 2, result: 'Private generated draft\nSecond paragraph', source_draft_id: 'draft-private-draft-alpha'
    });
    await card('draft-beta').getByRole('button', { name: '确认交付', exact: true }).click();
    await f.page.waitForFunction(() => document.getElementById('task-chat-draft-beta') === null);
    assert.equal(actionPosts().length, 2, 'explicit card confirmation is still supported');
    assert.equal(actionPosts()[1].pathname, '/api/tasks/draft-beta/actions');
    await f.close(); console.log('PASS Chromium ambiguous replies never confirm stale tasks after chat, reload or task switching');
  }
  {
    const f = await fixture(browser, origin);
    await f.page.setViewportSize({ width: 400, height: 900 });
    await f.page.locator('#open-chat').click();
    f.state.proposals = [];
    const long = 'LongUnbrokenValue'.repeat(100);
    f.state.reply = '# Structured reply\n\n- first\n- second\n\n| Name | Details |\n| --- | --- |\n| example | ' + long + ' |\n\n```js\n' + long + '\n```\n\n' + long + '\n<img src=x onerror=alert(1)>';
    await f.send('Show a long structured reply'); await settled(f);
    assert.equal(await f.page.locator('#conversation table').count(), 1);
    assert.equal(await f.page.locator('#conversation li').count(), 2);
    assert.equal(await f.page.locator('#conversation img').count(), 0);
    assert.equal(await f.page.locator('#conversation pre code').textContent(), long);
    const bounds = await f.page.evaluate(() => ({
      page: document.documentElement.scrollWidth <= innerWidth,
      bubbles: [...document.querySelectorAll('#conversation .markdown')].every(el => el.getBoundingClientRect().right <= innerWidth),
      composer: document.getElementById('send').getBoundingClientRect().bottom <= innerHeight
    }));
    assert.deepEqual(bounds, { page: true, bubbles: true, composer: true });
    await f.close(); console.log('PASS Chromium narrow long Markdown table/code/reply containment and inert HTML');
  }
  {
    const f = await fixture(browser, origin, false, true);
    f.state.tasks = [{ id: 'new-task', revision: 0, requester: 'bob', owner: 'alice', status: 'awaiting_owner', summary: 'New assignment' }];
    await f.page.clock.fastForward(4000);
    await f.page.waitForFunction(() => document.title.startsWith('('));
    assert.equal(await f.page.title(), '(1) Agenct');
    await f.close(); console.log('PASS Chromium incoming task updates tab title within five seconds');
  }
}
