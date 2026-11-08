/* Two isolated browsers against a real, temporary Hub. All model calls use Mock.
 * Build first: cargo build --locked --manifest-path hub/Cargo.toml
 * Run: node hub/web/collaboration.browser.test.cjs
 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const { createHash, randomBytes, randomUUID } = require('node:crypto');
const { spawn } = require('node:child_process');
const { chromium } = require('playwright');

const root = path.resolve(__dirname, '../..');
const binary = process.env.AGENCT_TEST_HUB_BINARY || path.join(root, 'hub/target/debug/agenct-hub');
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
const screenshotDir = process.env.AGENCT_TEST_SCREENSHOT_DIR || fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-collaboration-screens-'));

async function freePort() {
  const socket = net.createServer();
  await new Promise(resolve => socket.listen(0, '127.0.0.1', resolve));
  const port = socket.address().port;
  await new Promise(resolve => socket.close(resolve));
  return port;
}

(async () => {
  assert(fs.existsSync(binary), 'Build the Hub before running this test');
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-collaboration-'));
  fs.mkdirSync(screenshotDir, { recursive: true });
  const port = await freePort(), origin = `http://127.0.0.1:${port}`;
  const tokens = Object.fromEntries(['alice', 'bob', 'eve'].map(id => [id, randomBytes(32).toString('hex')]));
  const users = Object.entries(tokens).map(([id, token]) => ({ id, token_sha256: createHash('sha256').update(token).digest('hex') }));
  let hub, browser;
  const pageErrors = [], pages = [];
  const stop = async () => {
    await browser?.close().catch(() => {}); browser = null;
    if (hub && hub.exitCode === null) await new Promise(resolve => {
      const child = hub, timer = setTimeout(() => child.kill('SIGKILL'), 5000);
      child.once('exit', () => { clearTimeout(timer); resolve(); }); child.kill('SIGINT');
    });
    hub = null;
  };
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
  async function refresh(page) { await page.locator('#refresh').evaluate(button => button.click()); }
  async function showTasks(page) {
    if (await page.locator('#toggle-tasks').getAttribute('aria-pressed') !== 'true') await page.locator('#toggle-tasks').click();
    await page.locator('#tab-tasks').click();
  }
  async function openPeer(page, id) {
    await page.locator('#open-contacts').click();
    await page.locator('.contact-row').filter({ hasText: id }).first().click();
    await page.locator('#peer-message-input').waitFor();
  }
  async function deliver(page, text) {
    await page.getByRole('button', { name: '提交人工交付', exact: true }).click();
    await page.getByLabel('人工交付内容', { exact: true }).fill(text);
    await page.locator('.action-editor').getByRole('button', { name: '预览', exact: true }).click();
    await page.getByRole('button', { name: '确认交付', exact: true }).click();
    await page.locator('.task-ready_for_acceptance').waitFor();
  }
  try {
    hub = spawn(binary, [], { cwd: root, stdio: ['ignore', 'ignore', 'pipe'], env: {
      PATH: process.env.PATH, HOME: process.env.HOME, LANG: process.env.LANG || 'C.UTF-8',
      AGENCT_BIND: `127.0.0.1:${port}`, AGENCT_DATABASE: path.join(temporary, 'hub.sqlite3'),
      AGENCT_RUNTIME: 'mock', AGENCT_USERS_JSON: JSON.stringify(users)
    } });
    let startupError;
    hub.once('error', error => { startupError = error; });
    hub.stderr.on('data', () => {});
    let ready = false;
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
    const aliceContext = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
    const bobContext = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
    const alice = await aliceContext.newPage(), bob = await bobContext.newPage();
    pages.push(alice, bob);
    for (const page of [alice, bob]) page.on('pageerror', error => pageErrors.push(error.message));
    await login(alice, 'alice'); await login(bob, 'bob');

    // Public and private values are intentionally distinct, so any accidental
    // inclusion of private fields in contact responses is detectable.
    const originalProfile = await ok('bob', 'collaboration-profile');
    const profile = {
      expected_revision: originalProfile.revision,
      public: { team: '支付团队', responsibilities: '支付服务与接口审核', help_with: '审核接口设计', request_requirements: '请提供接口文档和变更原因' },
      private: { work_context: '仅自己知道的客户迁移计划', response_preferences: 'PRIVATE-PREFERENCE：先写结论' },
      permissions: { auto_reply_public: true, auto_prepare_drafts: true }
    };
    // This suite exercises colleague handoff; conversational profile editing
    // has its own real-Hub coverage in profile-chat.browser.test.cjs.
    await ok('bob', 'collaboration-profile', 'PUT', profile);
    const savedProfile = await ok('bob', 'collaboration-profile');
    assert.equal(savedProfile.revision, originalProfile.revision + 1);
    assert.deepEqual(savedProfile.public, profile.public);
    assert.deepEqual(savedProfile.private, profile.private);
    assert.deepEqual(savedProfile.permissions, profile.permissions);
    const contact = await ok('alice', 'contacts/bob/profile');
    assert.equal(contact.public.responsibilities, profile.public.responsibilities);
    assert.equal(contact.private, undefined);
    assert(!JSON.stringify(contact).includes('PRIVATE-PREFERENCE'));
    assert.equal((await ok('alice', 'collaboration-profile')).private.work_context, '');
    console.log('PASS persisted collaboration profile distinguishes shared information from private context');

    await openPeer(alice, 'bob');
    await alice.locator('#peer-message-input').fill('你负责什么？');
    await alice.locator('#peer-send').click();
    await alice.waitForFunction(() => document.getElementById('peer-message-input').value === '');
    await alice.locator('#peer-messages').getByText(/支付服务与接口审核/).waitFor();
    const transcript = await ok('alice', 'contacts/bob/messages');
    assert.equal(transcript.length, 2);
    assert.equal(transcript[0].author_kind, 'human');
    assert.equal(transcript[0].sender, 'alice');
    assert.equal(transcript[1].author_kind, 'assistant');
    assert.equal(transcript[1].sender, 'bob');
    assert(!JSON.stringify(transcript).includes('PRIVATE-PREFERENCE'));
    assert((await alice.locator('#peer-messages').innerText()).includes('助手'));
    assert.deepEqual(await ok('eve', 'contacts/bob/messages'), []);
    const forged = await api('alice', 'contacts/bob/messages', 'POST', { body: '不能冒充本人或助手', request_id: randomUUID(), author_kind: 'assistant', sender: 'bob' });
    assert.equal(forged.status, 422);
    console.log('PASS real two-party conversation, explicitly attributed assistant reply, no private context or author impersonation');

    const retryBody = { body: '请查看共享的接口文档。', request_id: randomUUID() };
    const first = await ok('alice', 'contacts/bob/messages', 'POST', retryBody);
    const retry = await ok('alice', 'contacts/bob/messages', 'POST', retryBody);
    assert.deepEqual(retry, first);
    assert.equal((await api('alice', 'contacts/bob/messages', 'POST', { ...retryBody, body: '同一个请求不能改正文' })).status, 409);
    assert.equal((await ok('alice', 'contacts/bob/messages')).filter(message => message.body === retryBody.body).length, 1);
    await openPeer(bob, 'alice');
    await bob.locator('#peer-messages').getByText(retryBody.body, { exact: true }).waitFor();
    await bob.locator('#peer-message-input').fill('我看到了，稍后确认审核安排。');
    await bob.locator('#peer-send').click();
    await alice.locator('#peer-messages').getByText('我看到了，稍后确认审核安排。', { exact: true }).waitFor({ timeout: 12000 });
    console.log('PASS server idempotency and live messages between isolated browsers');
    await alice.screenshot({ path: path.join(screenshotDir, 'conversation-desktop.png') });

    // Return to a small viewport and verify the shared conversation retains its
    // unsent input while navigating to the colleague's details and back.
    await alice.setViewportSize({ width: 390, height: 844 });
    if (await alice.locator('#contacts-view').isVisible()) await alice.keyboard.press('Escape');
    await alice.locator('#peer-message-input').fill('未发送的协作问题');
    await alice.locator('#peer-details').click();
    assert(await alice.locator('#contacts-view').isVisible());
    await alice.locator('#contacts-view').getByText('支付服务与接口审核', { exact: true }).waitFor();
    assert(await alice.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
    await alice.screenshot({ path: path.join(screenshotDir, 'contact-mobile.png') });
    await alice.keyboard.press('Escape');
    assert.equal(await alice.locator('#peer-message-input').inputValue(), '未发送的协作问题');
    await alice.screenshot({ path: path.join(screenshotDir, 'conversation-mobile.png') });
    await alice.locator('#peer-message-input').fill('');
    await alice.setViewportSize({ width: 1440, height: 1000 });

    // Creating a proposal must not publish it. Sending the task still requires
    // the explicit existing review/confirmation step.
    await alice.locator('#peer-details').click();
    await alice.getByRole('button', { name: '请他帮忙', exact: true }).click();
    await alice.getByLabel('委派摘要', { exact: true }).fill('审核接口文档并给出权限设计修改意见');
    await alice.getByRole('button', { name: '生成委派提案', exact: true }).click();
    const proposal = alice.locator('.proposal:not(.settled)');
    await proposal.waitFor();
    assert.deepEqual(await ok('bob', 'tasks'), []);
    await proposal.getByRole('button', { name: '确认共享并委派', exact: true }).click();
    await refresh(bob); await showTasks(bob);
    await bob.getByRole('button', { name: '接手', exact: true }).click();
    await bob.getByRole('button', { name: '审阅 Agent 草稿', exact: true }).waitFor({ timeout: 15000 });
    assert.equal((await ok('alice', 'tasks'))[0].my_draft, undefined);
    assert.equal((await ok('alice', 'tasks'))[0].result, null);
    await bob.getByRole('button', { name: '审阅 Agent 草稿', exact: true }).click();
    await bob.getByLabel('私密交付草稿编辑', { exact: true }).fill('审核意见：补充接口权限校验和异常返回。');
    await bob.getByRole('button', { name: '使用此草稿填入人工交付', exact: true }).click();
    await deliver(bob, '审核意见：补充接口权限校验和异常返回。');
    await refresh(alice); await showTasks(alice);
    await alice.getByRole('button', { name: '退回', exact: true }).click();
    await alice.locator('.task-awaiting_owner').waitFor();
    await refresh(bob);
    await bob.getByRole('button', { name: '接手', exact: true }).click();
    await bob.locator('.task-working').waitFor();
    await deliver(bob, '已补充权限校验示例、异常返回和验收清单。');
    await refresh(alice);
    await alice.getByRole('button', { name: '验收', exact: true }).click();
    await alice.locator('#task-filter').selectOption('completed');
    await alice.locator('.task-completed').waitFor();
    const completed = (await ok('alice', 'tasks'))[0];
    assert.equal(completed.status, 'completed');
    const detail = await ok('alice', 'tasks/' + completed.id);
    assert.equal(detail.deliveries.length, 2);
    assert.deepEqual(await ok('eve', 'tasks'), []);
    assert(!JSON.stringify(detail).includes('PRIVATE-PREFERENCE'));
    console.log('PASS contact conversation → private proposal → explicit delegation → private drafting → delivery → return → acceptance');
    await alice.screenshot({ path: path.join(screenshotDir, 'completed-desktop.png') });
    await alice.reload();
    await alice.locator('#room:not(.hide)').waitFor();
    await openPeer(alice, 'bob');
    await alice.locator('#peer-messages').getByText('我看到了，稍后确认审核安排。', { exact: true }).waitFor();
    assert.deepEqual(pageErrors, []);
    console.log('PASS reload restores login and persisted conversation; no browser exceptions');
    console.log('Screenshots: ' + screenshotDir);
  } catch (error) {
    if (pageErrors.length) console.error('Browser exceptions:', pageErrors);
    for (let index = 0; index < pages.length; index++) {
      await pages[index].screenshot({ path: path.join(screenshotDir, `failure-${index}.png`) }).catch(() => {});
    }
    console.error('Failure screenshots: ' + screenshotDir);
    throw error;
  } finally {
    await stop();
    process.removeListener('SIGINT', interrupted); process.removeListener('SIGTERM', interrupted);
    fs.rmSync(temporary, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
