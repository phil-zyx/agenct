/* Explicit project discussion against a real temporary Hub; Mock only. */
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
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-project-discussion-'));
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
  async function select(page, name) {
    await openProject(page, name);
    await button(page, '与我的 Agent 讨论').click();
    await page.locator('#project-context:not(.hide)').waitFor();
  }
  async function send(page, message) {
    await page.locator('#text').fill(message);
    const result = page.waitForResponse(r => r.url() === origin + '/api/messages' && r.request().method() === 'POST');
    await page.locator('#send').click();
    const response = await result;
    await page.waitForFunction(() => !pendingTurn && !busy.has('send'));
    return { status: response.status(), body: response.request().postDataJSON(), result: await response.json() };
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
    const { alice, bob } = pages;
    const project = await ok('alice', 'projects', 'POST', { name: '共同项目的私下讨论',
      goal: '项目说明不等于团队确认', kind: 'temporary', request_id: randomUUID() });
    const route = 'projects/' + project.id;
    const revision = async () => (await ok('alice', route)).project.revision;
    for (const who of ['bob', 'carol']) {
      await ok('alice', route + '/members', 'POST', { owner: who, expected_revision: await revision() });
      await ok(who, route + '/membership', 'POST', { action: 'accept', expected_revision: await revision() });
    }
    const sourceNote = 'Carol 明确发布的模型来源，不是团队承诺';
    await ok('carol', route + '/state', 'POST', { request_id: randomUUID(), expected_revision: await revision(),
      content: { kind: 'goal', content: '先核对交付范围，再分别认领', assignee: null,
        source_kind: 'model_suggestion', source_note: sourceNote } });
    const secret = 'CAROL-PRIVATE-' + randomUUID();
    await ok('carol', 'messages', 'POST', { message: secret, request_id: randomUUID() });
    const before = await ok('alice', route + '/state');
    const bobHistory = await ok('bob', 'messages');
    const writes = [];
    alice.on('request', request => {
      if (request.url().startsWith(origin + '/api/') && request.method() !== 'GET') writes.push(request.url());
    });
    await alice.locator('#text').fill('尚未发送的私下问题');
    await select(alice, project.name);
    assert.equal(await alice.locator('#text').inputValue(), '尚未发送的私下问题');
    assert.equal(writes.length, 0, 'Selecting discussion must not publish or send');
    assert((await alice.locator('#project-context-title').innerText()).includes(project.name));
    const details = alice.locator('#project-context details');
    if (await details.count()) await details.locator('summary').click();
    const preview = await alice.locator('#project-context').innerText();
    assert(/版本/.test(preview));
    assert(/私|自己/.test(preview));
    assert(/不会|不.*授权/.test(preview));
    assert(!preview.includes(secret));
    if (process.env.AGENCT_TEST_SCREENSHOT_DIR) {
      fs.mkdirSync(process.env.AGENCT_TEST_SCREENSHOT_DIR, { recursive: true });
      await alice.screenshot({ path: path.join(process.env.AGENCT_TEST_SCREENSHOT_DIR, 'project-discussion-desktop.png'), fullPage: true });
      await alice.setViewportSize({ width: 390, height: 844 });
      assert(await alice.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), 'Project chip must not overflow a narrow viewport');
      await alice.screenshot({ path: path.join(process.env.AGENCT_TEST_SCREENSHOT_DIR, 'project-discussion-mobile.png'), fullPage: true });
      await alice.setViewportSize({ width: 1440, height: 1000 });
    }
    await alice.locator('#text').fill('/delegate bob 不应该执行的命令');
    await alice.locator('#send').click();
    await alice.waitForFunction(() => !busy.has('send'));
    assert.equal(writes.length, 0, 'Selected project forbids command side effects');
    assert.equal(await alice.locator('#text').inputValue(), '/delegate bob 不应该执行的命令');
    const selectedRevision = await revision();
    const sent = await send(alice, '根据共同项目状态，我接下来应该先和谁核对什么？不要发布或代替任何人确认。');
    assert.equal(sent.status, 200);
    assert.equal((await api('alice', 'messages', 'POST', { message: 'conflicting context', request_id: randomUUID(),
      selected_project: { project_id: project.id, expected_revision: selectedRevision },
      selected_task: { task_id: 'not-needed', expected_revision: 0 } })).status, 400, 'Server enforces mutually exclusive context');
    assert.deepEqual(sent.body.selected_project, { project_id: project.id, expected_revision: selectedRevision });
    assert(!sent.body.selected_task, 'Project and task context cannot be sent together');
    assert.deepEqual(await ok('alice', route + '/state'), before, 'Private discussion must not mutate project state');
    assert.deepEqual(await ok('bob', 'messages'), bobHistory, 'Discussion stays in the requester private chat');
    assert.equal(sent.result.project_context.project_id, project.id);
    assert.equal(sent.result.project_context.revision, selectedRevision);
    const source = sent.result.project_context.items.find(entry => entry.source_note === sourceNote);
    assert(source, 'Snapshot preserves published source provenance');
    assert.equal(source.source_kind, 'model_suggestion');
    assert.equal(source.status, 'proposed');
    assert.equal(source.version, 1);
    assert.equal(source.created_by, 'carol');
    assert(!JSON.stringify(sent.result).includes(secret), 'Other member private chat must never enter project context');
    assert.equal(writes.filter(url => url !== origin + '/api/messages').length, 0, 'Only private chat is written');
    await alice.locator('#project-context.hide').waitFor({ state: 'attached' });
    const ordinary = await send(alice, '接下来普通聊天');
    assert(!ordinary.body.selected_project, 'Project selection is one turn only');
    assert.deepEqual((await api('alice', 'messages', 'POST', sent.body)).value, sent.result, 'Same request replays without model redispatch');
    assert.equal((await api('alice', 'messages', 'POST', { ...sent.body,
      selected_project: { project_id: project.id, expected_revision: selectedRevision + 1 } })).status, 409);
    console.log('PASS explicit one-turn project selection, private-only writes, no private leakage, durable replay');

    await select(bob, project.name);
    const oldRevision = await revision();
    await ok('carol', route + '/state', 'POST', { request_id: randomUUID(), expected_revision: oldRevision,
      content: { kind: 'blocker', content: '新发布的阻塞，旧选择不能静默替换', source_kind: 'human_statement', source_note: 'Carol 当前报告' } });
    const stale = await api('bob', 'messages', 'POST', { message: 'old selected version', request_id: randomUUID(),
      selected_project: { project_id: project.id, expected_revision: oldRevision } });
    assert.equal(stale.status, 409, 'Server rejects stale selection rather than recapturing a newer version');
    const staleWrites = [];
    const observeStale = request => { if (request.method() === 'POST') staleWrites.push(request.url()); };
    bob.on('request', observeStale);
    await bob.locator('#text').fill('保留我针对旧版本的问题');
    await bob.locator('#send').click();
    await bob.waitForFunction(() => !busy.has('send'));
    assert.deepEqual(staleWrites, [], 'Stale preflight prevents the browser from sending a turn');
    assert.equal(await bob.locator('#text').inputValue(), '保留我针对旧版本的问题');
    bob.off('request', observeStale);
    await bob.locator('#clear-project-context').click();
    await bob.locator('#project-context.hide').waitFor({ state: 'attached' });
    await select(bob, project.name);
    const removedRevision = await revision();
    const bobTurn = { message: '保留在本人历史的项目讨论', request_id: randomUUID(),
      selected_project: { project_id: project.id, expected_revision: removedRevision } };
    await ok('bob', 'messages', 'POST', bobTurn);
    await ok('bob', route + '/membership', 'POST', { action: 'leave', expected_revision: removedRevision });
    const denied = await api('bob', 'messages', 'POST', { message: 'not authorized now', request_id: randomUUID(),
      selected_project: { project_id: project.id, expected_revision: removedRevision } });
    assert.equal(denied.status, 404, 'Removed member cannot discuss the former snapshot');
    assert.equal((await api('bob', 'messages', 'POST', bobTurn)).status, 404, 'Revoked member cannot replay the snapshot receipt');
    assert((await ok('bob', 'messages')).some(message => message.content === bobTurn.message), 'Previously received private history is retained, not remotely erased');
    assert.equal((await api('eve', 'messages', 'POST', { message: 'outside', request_id: randomUUID(),
      selected_project: { project_id: project.id, expected_revision: await revision() } })).status, 404);
    console.log('PASS real project revision changes, revoked membership, outsider denial');

    // A real authorized response delayed across logout must not restore project
    // material to the newly logged-in account. No mock response content is used.
    await openProject(alice, project.name);
    let releaseRead, readStarted;
    const readGate = new Promise(resolve => { releaseRead = resolve; });
    const readReady = new Promise(resolve => { readStarted = resolve; });
    const detailUrl = origin + '/api/' + route;
    let held = false;
    const delayDetail = async intercepted => {
      if (held) return intercepted.continue();
      held = true;
      try {
        const response = await intercepted.fetch(); readStarted(); await readGate;
        await intercepted.fulfill({ response });
      } catch (error) {
        if (!/closed|cancel|abort|disposed/i.test(error.message)) throw error;
      }
    };
    await alice.route(detailUrl, delayDetail);
    await button(alice, '与我的 Agent 讨论').click();
    let readTimeout;
    try { await Promise.race([readReady, new Promise((_, reject) => { readTimeout = setTimeout(() => reject(Error('Discussion did not freshly read project')), 15000); })]); }
    finally { clearTimeout(readTimeout); }
    await alice.locator('#logout').click(); await alice.locator('#token').waitFor();
    releaseRead(); await alice.unroute(detailUrl, delayDetail);
    await login(alice, 'eve');
    await alice.locator('#project-context.hide').waitFor({ state: 'attached' });
    assert(!(await alice.locator('#project-context').textContent()).includes(project.name), 'Account change clears context text');
    const afterSwitch = await send(alice, '新账号普通聊天');
    assert(!afterSwitch.body.selected_project);
    assert.deepEqual(errors, [], 'No browser exceptions');
    console.log('PASS identity reset clears selection; all project discussion browser checks passed');
  } finally {
    await browser?.close().catch(() => {});
    if (hub && hub.exitCode === null) await new Promise(resolve => {
      const timer = setTimeout(() => hub.kill('SIGKILL'), 5000);
      hub.once('exit', () => { clearTimeout(timer); resolve(); }); hub.kill('SIGINT');
    });
    fs.rmSync(temp, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
