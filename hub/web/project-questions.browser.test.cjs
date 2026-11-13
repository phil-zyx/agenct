/* Explicit shared questions against a real temporary Hub; Mock only. */
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
async function freePort() {
  const server = net.createServer();
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  return port;
}
(async () => {
  assert(fs.existsSync(binary), 'Build the current Hub before running this acceptance');
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-project-questions-'));
  const port = await freePort(), origin = `http://127.0.0.1:${port}`;
  const tokens = Object.fromEntries(['alice', 'bob', 'carol', 'eve'].map(id => [id, randomBytes(32).toString('hex')]));
  const users = Object.entries(tokens).map(([id, token]) => ({ id, token_sha256: createHash('sha256').update(token).digest('hex') }));
  let browser, hub, startupError;
  const errors = [];
  const button = (page, name) => page.getByRole('button', { name, exact: true });
  async function api(who, route, method = 'GET', body) {
    const response = await fetch(origin + '/api/' + route, { method,
      headers: { authorization: 'Bearer ' + tokens[who], 'content-type': 'application/json' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
    const text = await response.text();
    let value; try { value = JSON.parse(text); } catch { value = text; }
    return { status: response.status, value };
  }
  async function ok(who, route, method = 'GET', body) {
    const result = await api(who, route, method, body);
    assert.equal(result.status, 200, `${method} ${route}: ${JSON.stringify(result.value)}`);
    return result.value;
  }
  async function login(page, who) {
    await page.goto(origin); await page.locator('#token').fill(tokens[who]);
    await page.locator('#enter').click(); await page.locator('#room:not(.hide)').waitFor();
  }
  async function openProject(page, name) {
    await page.locator('#open-projects').click();
    const panel = page.locator('#project-panel');
    await panel.waitFor();
    await panel.getByRole('button', { name: /^(项目列表|创建项目)$/ }).first().waitFor();
    if (await button(page, '项目列表').isVisible()) await button(page, '项目列表').click();
    await panel.locator('.project-list-item').filter({ hasText: name }).click();
    await panel.locator('.project-title').filter({ hasText: name }).waitFor();
  }
  try {
    hub = spawn(binary, [], { cwd: root, stdio: ['ignore', 'ignore', 'pipe'], env: {
      PATH: process.env.PATH, HOME: process.env.HOME, LANG: process.env.LANG || 'C.UTF-8',
      AGENCT_BIND: `127.0.0.1:${port}`, AGENCT_DATABASE: path.join(temp, 'hub.sqlite3'),
      AGENCT_RUNTIME: 'mock', AGENCT_USERS_JSON: JSON.stringify(users)
    } });
    hub.once('error', error => { startupError = error; }); hub.stderr.on('data', () => {});
    let ready = false;
    for (let n = 0; n < 200; n++) {
      if (startupError) throw startupError;
      assert.equal(hub.exitCode, null, 'Hub exited during startup');
      try { ready = (await fetch(origin)).ok; } catch {}
      if (ready) break; await pause(50);
    }
    assert(ready, 'Hub startup timed out');
    browser = await chromium.launch({ headless: true,
      ...(process.env.CHROMIUM_EXECUTABLE_PATH ? { executablePath: process.env.CHROMIUM_EXECUTABLE_PATH } : {}) });
    const pages = {};
    for (const who of ['alice', 'bob', 'carol']) {
      const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
      pages[who] = await context.newPage(); pages[who].on('pageerror', e => errors.push(e.message));
      await login(pages[who], who);
    }
    const { alice, bob, carol } = pages;
    const project = await ok('alice', 'projects', 'POST', { name: '三人的共享问题',
      goal: '项目说明不等于团队确认', kind: 'temporary', request_id: randomUUID() });
    const route = 'projects/' + project.id;
    const revision = async () => (await ok('alice', route)).project.revision;
    for (const who of ['bob', 'carol']) {
      await ok('alice', route + '/members', 'POST', { owner: who, expected_revision: await revision() });
      await ok(who, route + '/membership', 'POST', { action: 'accept', expected_revision: await revision() });
    }

    const source = { kind: 'goal', content: '三人分别核对网站交付范围', assignee: null,
      source_kind: 'human_statement', source_note: 'Alice 主动发布的待确认目标' };
    const created = await ok('alice', route + '/state', 'POST', { request_id: randomUUID(), expected_revision: await revision(), content: source });
    const item = created.item, questionRoute = route + '/state/' + item.id + '/questions';
    const secret = 'PRIVATE-CAROL-' + randomUUID();
    await ok('carol', 'messages', 'POST', { message: secret, request_id: randomUUID() });
    const privateBefore = {};
    for (const who of ['alice', 'bob', 'carol']) privateBefore[who] = await ok(who, 'messages');
    const stateBefore = (await ok('alice', route + '/state')).items;
    const mutations = [];
    for (const page of Object.values(pages)) page.on('request', request => {
      if (request.method() !== 'GET' && request.url().startsWith(origin + '/api/')) mutations.push(request.url());
    });
    const panel = page => page.locator('#project-panel');
    const editor = page => panel(page).locator('.project-editor');
    const ack = page => page.getByLabel('我已核对项目、问题版本和共享文字', { exact: true });
    async function questions(page) {
      await openProject(page, project.name);
      await panel(page).locator('.project-state-card[data-item="' + item.id + '"]').getByRole('button', { name: '共享问答', exact: true }).click();
      await button(page, '提出共享问题').waitFor();
    }
    async function prepare(page, text, replyId = null) {
      await questions(page);
      if (replyId) await page.locator('.project-question-message[data-question="' + replyId + '"]').getByRole('button', { name: '回复此问题', exact: true }).click();
      else await button(page, '提出共享问题').click();
      await page.getByLabel(replyId ? '共享回复内容' : '共享问题内容', { exact: true }).fill(text);
      await button(page, '预览共享反馈').click();
      assert(await button(page, '确认共享反馈').isDisabled(), 'Explicit acknowledgement required');
      assert((await editor(page).innerText()).includes('不代表确认'));
      assert(!(await editor(page).innerText()).includes(secret));
    }
    async function publish(page, label = '确认共享反馈') {
      await page.getByLabel(label === '确认撤回我的反馈' ? '我已核对撤回范围与历史保留说明' : '我已核对项目、问题版本和共享文字', { exact: true }).check();
      const response = page.waitForResponse(r => r.url().includes('/questions') && r.request().method() === 'POST');
      await button(page, label).evaluate(b => { b.click(); b.click(); });
      const result = await response;
      assert.equal(result.status(), 200);
      await editor(page).waitFor({ state: 'detached' });
      return { value: await result.json(), body: result.request().postDataJSON() };
    }
    await prepare(alice, '取消的本地问题');
    assert.equal(mutations.length, 0, 'Preview does not publish or call a model');
    await button(alice, '取消编辑').click();
    await editor(alice).waitFor({ state: 'detached' });
    assert.deepEqual((await ok('alice', questionRoute)).messages, []);

    const questionText = '<img src=x onerror="globalThis.questionXss=true"> 第一版是否包括移动端？';
    await prepare(alice, questionText);
    // Closing the panel clears acknowledgement, but preserves local text for review.
    await ack(alice).check(); await button(alice, '回到私聊').click();
    await alice.locator('#open-projects').click();
    assert.equal(await ack(alice).isChecked(), false);
    assert(await button(alice, '确认共享反馈').isDisabled());
    const question = await publish(alice), rootMessage = question.value.message;
    assert.equal(rootMessage.author, 'alice'); assert.equal(rootMessage.item_version, 1);
    assert.equal((await ok('alice', questionRoute)).messages.length, 1, 'Repeated click creates once');
    assert.equal((await ok('alice', questionRoute, 'POST', question.body)).message.id, rootMessage.id);
    assert.equal((await api('alice', questionRoute, 'POST', { ...question.body, content: 'Different intent' })).status, 409);

    await prepare(bob, '我先核对移动布局，这不是接受分工。', rootMessage.id);
    const bobReply = await publish(bob);
    await prepare(carol, '可以，先讨论验收条件。', rootMessage.id);
    // Execute the genuine write, then drop its response. Retry must use the same request ID.
    let interceptedBody, completedBody;
    const dropOnce = async intercepted => {
      if (intercepted.request().method() !== 'POST') return intercepted.continue();
      interceptedBody = intercepted.request().postDataJSON();
      const response = await intercepted.fetch(); assert.equal(response.status(), 200);
      completedBody = await response.json(); await intercepted.abort('failed');
    };
    await carol.route(origin + '/api/' + questionRoute, dropOnce);
    await ack(carol).check(); await button(carol, '确认共享反馈').click();
    await button(carol, '核对并重试原提交').waitFor();
    await carol.unroute(origin + '/api/' + questionRoute, dropOnce);
    const recovered = await publish(carol, '核对并重试原提交');
    assert.deepEqual(recovered.body, interceptedBody, 'Unknown-result retry preserves original intent');
    assert.equal(recovered.value.message.id, completedBody.message.id);
    const messages = (await ok('alice', questionRoute)).messages;
    assert.equal(messages.length, 3); assert.deepEqual(messages.map(m => m.author).sort(), ['alice', 'bob', 'carol']);
    assert.deepEqual((await ok('alice', route + '/state')).items, stateBefore, 'Feedback never confirms or changes shared state');
    for (const who of ['alice', 'bob', 'carol']) assert.deepEqual(await ok(who, 'messages'), privateBefore[who], 'No automatic private messages or model dispatch');
    assert(mutations.every(url => url === origin + '/api/' + questionRoute), 'UI only publishes explicit question/replies');
    await questions(alice);
    assert((await editor(alice).innerText()).includes(questionText));
    assert.equal(await editor(alice).locator('img').count(), 0, 'Shared text is not interpreted as HTML');
    assert.equal(await alice.evaluate(() => globalThis.questionXss), undefined);
    assert(!(await editor(alice).innerText()).includes(secret));
    if (process.env.AGENCT_TEST_SCREENSHOT_DIR) {
      fs.mkdirSync(process.env.AGENCT_TEST_SCREENSHOT_DIR, { recursive: true });
      await alice.screenshot({ path: path.join(process.env.AGENCT_TEST_SCREENSHOT_DIR, 'project-questions-desktop.png'), fullPage: true });
      await alice.setViewportSize({ width: 390, height: 844 });
      assert(await alice.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), 'Shared discussion fits mobile');
      await alice.screenshot({ path: path.join(process.env.AGENCT_TEST_SCREENSHOT_DIR, 'project-questions-mobile.png'), fullPage: true });
      await alice.setViewportSize({ width: 1440, height: 1000 });
    }
    await button(alice, '取消编辑').click(); await editor(alice).waitFor({ state: 'detached' });
    console.log('PASS real three-person explicit questions/replies, acknowledgement, response-loss recovery, privacy and no confirmation side effects');

    await prepare(bob, '尚未发布的旧版本回复', rootMessage.id);
    await ok('alice', route + '/state/' + item.id + '/actions', 'POST', { action: 'revise', expected_revision: await revision(), expected_version: 1, content: { ...source, content: '第二版范围必须重新提问' } });
    const staleBody = { request_id: randomUUID(), expected_revision: await revision(), expected_version: 1, content: 'stale', reply_to: rootMessage.id };
    assert.equal((await api('bob', questionRoute, 'POST', staleBody)).status, 409);
    assert.equal((await api('bob', questionRoute, 'POST', { ...staleBody, expected_version: 2 })).status, 409, 'Cannot attach old question to a new version');
    // Browser may notice through polling before submit, or receive the server conflict.
    if (!(await ack(bob).isDisabled())) await ack(bob).check();
    if (!(await button(bob, '确认共享反馈').isDisabled())) {
      await button(bob, '确认共享反馈').click();
      await bob.waitForFunction(() => projectDraft?.invalid === true);
    }
    assert.equal((await ok('alice', questionRoute)).messages.length, 3);
    await button(bob, '取消编辑').click(); await editor(bob).waitFor({ state: 'detached' });
    await questions(bob);
    assert((await editor(bob).innerText()).includes('旧版本反馈'));
    assert.equal(await editor(bob).getByRole('button', { name: '回复此问题', exact: true }).count(), 0);
    await button(bob, '取消编辑').click(); await editor(bob).waitFor({ state: 'detached' });

    assert.equal((await api('bob', questionRoute + '/' + rootMessage.id + '/withdraw', 'POST', { expected_revision: await revision() })).status, 404);
    await questions(alice);
    await alice.locator('.project-question-message[data-question="' + rootMessage.id + '"]').getByRole('button', { name: '撤回我的反馈', exact: true }).click();
    await publish(alice, '确认撤回我的反馈');
    const withdrawn = (await ok('alice', questionRoute)).messages;
    assert.equal(withdrawn.find(m => m.id === rootMessage.id).content, '');
    assert.equal(withdrawn.find(m => m.id === rootMessage.id).withdrawn, true);
    assert.equal(withdrawn.find(m => m.id === bobReply.value.message.id).content, bobReply.value.message.content);
    assert.equal((await ok('alice', questionRoute, 'POST', question.body)).message.content, '', 'Receipt also redacts withdrawn content');
    await ok('bob', route + '/membership', 'POST', { action: 'leave', expected_revision: await revision() });
    assert.equal((await api('bob', questionRoute)).status, 404);
    assert.equal((await api('bob', questionRoute, 'POST', bobReply.body)).status, 404, 'Revocation blocks even old receipt replay');
    assert.equal((await api('eve', questionRoute)).status, 404);
    console.log('PASS stale source/read-only history, author-only withdrawal, redacted replay and revoked/outsider access');

    // Hold a genuine authorized response across logout and make sure it cannot
    // restore project content in a newly logged-in outsider account.
    await openProject(alice, project.name);
    let release, started, settled, routeError, held = false, loggedOut = false;
    const gate = new Promise(resolve => { release = resolve; });
    const readyRead = new Promise(resolve => { started = resolve; });
    const done = new Promise(resolve => { settled = resolve; });
    const bounded = async (promise, label) => {
      let timer;
      try { return await Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(Error(label)), 15000); })]); }
      finally { clearTimeout(timer); }
    };
    const holdRead = async intercepted => {
      if (held) return intercepted.continue(); held = true;
      try { const response = await intercepted.fetch(); started(); await gate; await intercepted.fulfill({ response }); }
      catch (error) { if (!(loggedOut && /Route is already handled|closed|cancel|abort|disposed/i.test(error.message))) routeError = error; }
      finally { settled(); }
    };
    await alice.route(origin + '/api/' + questionRoute, holdRead);
    try {
      await panel(alice).locator('.project-state-card[data-item="' + item.id + '"]').getByRole('button', { name: '共享问答', exact: true }).click();
      await bounded(readyRead, 'Question read did not start');
      await alice.locator('#logout').click(); await alice.locator('#token').waitFor(); loggedOut = true;
    } finally {
      release(); if (held) await bounded(done, 'Held read did not settle');
      await alice.unroute(origin + '/api/' + questionRoute, holdRead);
    }
    if (routeError) throw routeError;
    await login(alice, 'eve');
    assert(!(await panel(alice).innerText()).includes(project.name));
    assert.equal(await alice.locator('.project-question-message').count(), 0);
    assert.deepEqual(errors, [], 'No browser exceptions');
    console.log('PASS late question response fenced after logout; shared question browser acceptance complete');

  } finally {
    await browser?.close().catch(() => {});
    if (hub && hub.exitCode === null) await new Promise(resolve => {
      const timer = setTimeout(() => hub.kill('SIGKILL'), 5000);
      hub.once('exit', () => { clearTimeout(timer); resolve(); }); hub.kill('SIGINT');
    });
    fs.rmSync(temp, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
