/* Conversational personal configuration against a real isolated Mock Hub.
 * Build first: cargo build --locked --manifest-path hub/Cargo.toml
 * Run: node hub/web/profile-chat.browser.test.cjs
 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const { createHash, randomBytes } = require('node:crypto');
const { spawn } = require('node:child_process');
const { chromium } = require('playwright');

const root = path.resolve(__dirname, '../..');
const binary = process.env.AGENCT_TEST_HUB_BINARY || path.join(root, 'hub/target/debug/agenct-hub');
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));

async function freePort() {
  const socket = net.createServer();
  await new Promise(resolve => socket.listen(0, '127.0.0.1', resolve));
  const port = socket.address().port;
  await new Promise(resolve => socket.close(resolve));
  return port;
}

(async () => {
  assert(fs.existsSync(binary), 'Build the Hub before running this test');
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-profile-chat-'));
  const screenshots = process.env.AGENCT_TEST_SCREENSHOT_DIR || fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-profile-chat-screens-'));
  fs.mkdirSync(screenshots, { recursive: true });
  const port = await freePort(), origin = `http://127.0.0.1:${port}`;
  const tokens = Object.fromEntries(['alice', 'bob', 'eve'].map(id => [id, randomBytes(32).toString('hex')]));
  const users = Object.entries(tokens).map(([id, token]) => ({ id, token_sha256: createHash('sha256').update(token).digest('hex') }));
  const pageErrors = [], pages = [], messagePosts = [];
  let hub, browser;
  async function stop() {
    await browser?.close().catch(() => {}); browser = null;
    if (hub && hub.exitCode === null) await new Promise(resolve => {
      const child = hub, timer = setTimeout(() => child.kill('SIGKILL'), 5000);
      child.once('exit', () => { clearTimeout(timer); resolve(); }); child.kill('SIGINT');
    });
    hub = null;
  }
  const interrupted = () => { void stop(); };
  process.once('SIGINT', interrupted); process.once('SIGTERM', interrupted);
  async function api(who, route, method = 'GET', body) {
    const response = await fetch(origin + '/api/' + route, {
      method, headers: { authorization: 'Bearer ' + tokens[who], 'content-type': 'application/json' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) })
    });
    const text = await response.text();
    let value = null; try { value = text ? JSON.parse(text) : null; } catch {}
    return { status: response.status, value };
  }
  async function ok(who, route, method = 'GET', body) {
    const response = await api(who, route, method, body);
    assert.equal(response.status, 200, `${method} ${route}: ${JSON.stringify(response.value)}`);
    return response.value;
  }
  async function login(page, who) {
    await page.goto(origin);
    await page.locator('#token').fill(tokens[who]);
    await page.locator('#enter').click();
    await page.locator('#room:not(.hide)').waitFor();
  }
  const card = (page, id) => page.locator(`article.profile-suggestion[data-suggestion="${id}"]`);
  async function propose(page, text, who = 'alice') {
    const before = new Set((await ok(who, 'profile-suggestions')).map(item => item.id));
    await page.locator('#text').fill(text);
    await page.locator('#send').click();
    let suggestion;
    for (let attempt = 0; attempt < 200; attempt++) {
      suggestion = (await ok(who, 'profile-suggestions')).find(item => !before.has(item.id));
      if (suggestion) break;
      await pause(50);
    }
    assert(suggestion, 'Natural language input must create a durable profile suggestion');
    await card(page, suggestion.id).getByRole('button', { name: '确认记住', exact: true }).waitFor();
    return suggestion;
  }
  async function confirm(page, id) {
    await card(page, id).getByRole('button', { name: '确认记住', exact: true }).click();
    await card(page, id).getByText('已记住', { exact: true }).waitFor();
  }
  try {
    hub = spawn(binary, [], { cwd: root, stdio: ['ignore', 'ignore', 'pipe'], env: {
      PATH: process.env.PATH, HOME: process.env.HOME, LANG: process.env.LANG || 'C.UTF-8',
      AGENCT_BIND: `127.0.0.1:${port}`, AGENCT_DATABASE: path.join(temporary, 'hub.sqlite3'),
      AGENCT_RUNTIME: 'mock', AGENCT_USERS_JSON: JSON.stringify(users)
    } });
    let startupError, ready = false;
    hub.once('error', error => { startupError = error; });
    hub.stderr.on('data', () => {});
    for (let attempt = 0; attempt < 200; attempt++) {
      if (startupError) throw startupError;
      assert.equal(hub.exitCode, null, 'Temporary Hub must stay running');
      try { ready = (await fetch(origin)).ok; } catch {}
      if (ready) break;
      await pause(50);
    }
    assert(ready, 'Temporary Hub did not start');
    browser = await chromium.launch({ headless: true,
      ...(process.env.CHROMIUM_EXECUTABLE_PATH ? { executablePath: process.env.CHROMIUM_EXECUTABLE_PATH } : {}) });
    const aliceContext = await browser.newContext({ viewport: { width: 1440, height: 960 } });
    const bobContext = await browser.newContext({ viewport: { width: 1440, height: 960 } });
    const alice = await aliceContext.newPage(), bob = await bobContext.newPage();
    pages.push(alice, bob);
    for (const page of pages) page.on('pageerror', error => pageErrors.push(error.message));
    alice.on('request', request => {
      if (request.method() === 'POST' && request.url() === origin + '/api/messages') messagePosts.push(request.postDataJSON());
    });
    await login(alice, 'alice'); await login(bob, 'bob');

    await alice.locator('#open-collaboration').click();
    await alice.locator('#collaboration-profile-view').getByRole('button', { name: '通过聊天修改', exact: true }).waitFor();
    assert.equal(await alice.locator('#collaboration-profile-view input, #collaboration-profile-view textarea, #collaboration-profile-view select').count(), 0);
    await alice.locator('#collaboration-profile-view').getByRole('button', { name: '通过聊天修改', exact: true }).click();
    assert.equal(await alice.locator('#text').inputValue(), '记住：');
    assert.equal(messagePosts.length, 0, 'Opening conversational editing must not dispatch a message');
    const initial = await ok('alice', 'collaboration-profile');
    const privateSuggestion = await propose(alice, '记住：我负责支付服务，回复先给结论');
    assert.deepEqual(await ok('alice', 'collaboration-profile'), initial, 'Draft suggestions must not save automatically');
    assert.deepEqual(await ok('bob', 'profile-suggestions'), []);
    assert.deepEqual(await ok('eve', 'profile-suggestions'), []);
    const outsider = await api('bob', 'profile-suggestions/' + privateSuggestion.id + '/actions', 'POST', { action: 'confirm' });
    assert([403, 404].includes(outsider.status), 'Other users must not confirm private suggestions');
    await alice.reload();
    await alice.locator('#room:not(.hide)').waitFor();
    await card(alice, privateSuggestion.id).getByRole('button', { name: '确认记住', exact: true }).waitFor();
    assert.equal(messagePosts.length, 1, 'Refreshing a pending suggestion must not repeat the model request');
    await alice.screenshot({ path: path.join(screenshots, 'private-suggestion-desktop.png') });
    await confirm(alice, privateSuggestion.id);
    const remembered = await ok('alice', 'collaboration-profile');
    assert.equal(remembered.revision, initial.revision + 1);
    assert(remembered.private.work_context.includes('支付服务'));
    assert(remembered.private.response_preferences.includes('结论'));
    assert.deepEqual(remembered.public, initial.public);
    const publicRead = await ok('bob', 'contacts/alice/profile');
    assert.equal(publicRead.private, undefined);
    assert(!JSON.stringify(publicRead).includes('支付服务'), 'Remembering private work context must not publish it');
    console.log('PASS read-only memory page enters chat without sending; pending suggestions persist privately and require confirmation');

    const rejected = await propose(alice, '把我的职责公开为支付服务');
    await card(alice, rejected.id).getByRole('button', { name: '暂不保存', exact: true }).click();
    await alice.waitForFunction(id => !document.querySelector(`article.profile-suggestion[data-suggestion="${id}"]`)?.textContent.includes('确认记住'), rejected.id);
    assert.deepEqual(await ok('alice', 'collaboration-profile'), remembered);
    assert.notEqual((await ok('alice', 'profile-suggestions')).find(item => item.id === rejected.id)?.status, 'pending');
    console.log('PASS rejecting a suggestion leaves both private and public settings unchanged');

    const firstVersion = await propose(alice, '把我的职责公开为支付服务');
    const posted = messagePosts.length;
    await alice.locator('#text').fill('另一条还没发送的问题');
    await card(alice, firstVersion.id).getByRole('button', { name: '继续修改', exact: true }).click();
    assert.equal(await alice.locator('#text').inputValue(), '另一条还没发送的问题');
    assert.equal(messagePosts.length, posted);
    await alice.locator('#text').fill('');
    await card(alice, firstVersion.id).getByRole('button', { name: '继续修改', exact: true }).click();
    assert.equal(await alice.locator('#text').inputValue(), '把刚才的个人设置改为：');
    assert.equal(messagePosts.length, posted);
    assert.deepEqual(await ok('alice', 'collaboration-profile'), remembered);
    const edited = await propose(alice, '把我的职责公开为接口设计');
    const firstAfterEdit = (await ok('alice', 'profile-suggestions')).find(item => item.id === firstVersion.id);
    assert.notEqual(firstAfterEdit?.status, 'pending', 'A new suggestion must supersede the previous pending version');
    await confirm(alice, edited.id);
    const afterPublic = await ok('alice', 'collaboration-profile');
    assert.equal(afterPublic.public.responsibilities, '接口设计');
    assert.equal((await ok('bob', 'contacts/alice/profile')).public.responsibilities, '接口设计');
    assert.deepEqual(afterPublic.private, remembered.private);
    console.log('PASS continue editing only prepares input; new suggestion supersedes old and explicit confirmation publishes only selected fields');

    if (await alice.locator('#toggle-tasks').getAttribute('aria-pressed') === 'true') await alice.locator('#toggle-tasks').click();
    await alice.setViewportSize({ width: 390, height: 844 });
    const permission = await propose(alice, '允许助手用公开资料答复');
    assert.equal((await ok('alice', 'collaboration-profile')).permissions.auto_reply_public, false);
    assert(await alice.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
    await card(alice, permission.id).scrollIntoViewIfNeeded();
    await alice.screenshot({ path: path.join(screenshots, 'permission-suggestion-mobile.png') });
    await confirm(alice, permission.id);
    assert.equal((await ok('alice', 'collaboration-profile')).permissions.auto_reply_public, true);
    await alice.locator('#open-collaboration').click();
    await alice.locator('#collaboration-profile-view').getByRole('button', { name: '通过聊天修改', exact: true }).waitFor();
    assert.equal(await alice.locator('#collaboration-profile-view input, #collaboration-profile-view textarea, #collaboration-profile-view select').count(), 0);
    assert(await alice.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
    await alice.screenshot({ path: path.join(screenshots, 'remembered-profile-mobile.png') });
    await alice.locator('#open-chat').click();
    await alice.setViewportSize({ width: 1440, height: 960 });
    console.log('PASS changing assistant authority requires confirmation and works on a narrow viewport');

    const stale = await propose(alice, '关闭自动起草');
    const { revision, ...latest } = await ok('alice', 'collaboration-profile');
    await ok('alice', 'collaboration-profile', 'PUT', {
      ...latest, expected_revision: revision,
      private: { ...latest.private, response_preferences: '从另一页面保存的回复偏好' }
    });
    const conflictResponse = alice.waitForResponse(response => response.url() === origin + '/api/profile-suggestions/' + stale.id + '/actions' && response.request().method() === 'POST');
    await card(alice, stale.id).getByRole('button', { name: '确认记住', exact: true }).click();
    assert.equal((await conflictResponse).status(), 409);
    await card(alice, stale.id).getByText('资料或这条建议已更新，请在聊天中重新说明要修改的内容。原有资料没有被覆盖。', { exact: true }).waitFor();
    const unchanged = await ok('alice', 'collaboration-profile');
    assert.equal(unchanged.permissions.auto_prepare_drafts, true);
    assert.equal(unchanged.private.response_preferences, '从另一页面保存的回复偏好');
    const renewed = await propose(alice, '关闭自动起草');
    await confirm(alice, renewed.id);
    assert.equal((await ok('alice', 'collaboration-profile')).permissions.auto_prepare_drafts, false);
    console.log('PASS stale suggestions cannot overwrite newer settings; newly proposed changes can be confirmed');

    const pending = await propose(alice, '记住：我负责出海迁移，回复先给风险');
    await alice.locator('#logout').click();
    await login(alice, 'bob');
    assert.equal(await alice.locator('article.profile-suggestion').count(), 0);
    assert(!(await alice.locator('#conversation').innerText()).includes('出海迁移'));
    assert.deepEqual(await ok('bob', 'profile-suggestions'), []);
    assert.equal((await ok('bob', 'collaboration-profile')).revision, 0);
    const wrongAccount = await api('bob', 'profile-suggestions/' + pending.id + '/actions', 'POST', { action: 'reject' });
    assert([403, 404].includes(wrongAccount.status));
    await alice.locator('#open-collaboration').click();
    await alice.locator('#collaboration-profile-view').getByRole('button', { name: '通过聊天修改', exact: true }).waitFor();
    assert(!(await alice.locator('#collaboration-profile-view').innerText()).includes('支付服务'));
    assert.deepEqual(pageErrors, []);
    console.log('PASS logout and account switch clear the previous owner\'s private suggestions and remembered context');
    console.log('Screenshots: ' + screenshots);
  } catch (error) {
    if (pageErrors.length) console.error('Browser exceptions:', pageErrors);
    for (let index = 0; index < pages.length; index++) await pages[index].screenshot({ path: path.join(screenshots, `failure-${index}.png`) }).catch(() => {});
    console.error('Failure screenshots: ' + screenshots);
    throw error;
  } finally {
    await stop();
    process.removeListener('SIGINT', interrupted); process.removeListener('SIGTERM', interrupted);
    fs.rmSync(temporary, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
