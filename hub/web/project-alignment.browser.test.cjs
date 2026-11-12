/* Four isolated browsers against a real, temporary Hub. All model calls use Mock.
 * Build first: cargo build --locked --manifest-path hub/Cargo.toml
 * Run: node hub/web/project-alignment.browser.test.cjs
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
const screenshotDir = process.env.AGENCT_TEST_SCREENSHOT_DIR || fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-project-alignment-screens-'));

async function freePort() {
  const socket = net.createServer();
  await new Promise(resolve => socket.listen(0, '127.0.0.1', resolve));
  const port = socket.address().port;
  await new Promise(resolve => socket.close(resolve));
  return port;
}

(async () => {
  assert(fs.existsSync(binary), 'Build the Hub before running this test');
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-project-alignment-'));
  fs.mkdirSync(screenshotDir, { recursive: true });
  const port = await freePort(), origin = `http://127.0.0.1:${port}`;
  const tokens = Object.fromEntries(['alice', 'bob', 'carol', 'eve'].map(id => [id, randomBytes(32).toString('hex')]));
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
  const button = (page, name) => page.getByRole('button', { name, exact: true });
  const panel = page => page.locator('#project-panel');
  const alignment = page => panel(page).locator('.project-alignment[aria-label="对齐建议"]');
  const card = (page, id) => panel(page).locator(`.project-state-card[data-item="${id}"]`);
  async function openProjects(page) {
    await page.locator('#open-projects').click();
    await panel(page).waitFor();
    await panel(page).getByRole('button', { name: /^(项目列表|创建项目)$/ }).first().waitFor();
  }
  async function openProject(page, name) {
    await openProjects(page);
    if (await button(page, '项目列表').isVisible()) await button(page, '项目列表').click();
    await panel(page).locator('.project-list-item').filter({ hasText: name }).click();
    await panel(page).locator('.project-title').filter({ hasText: name }).waitFor();
  }
  async function settle(page) {
    await page.waitForFunction(() => !projectRead && !projectBusy);
  }
  async function poll(page) {
    await settle(page);
    await page.evaluate(async () => { await globalThis.alignmentTestRefresh(); });
    await settle(page);
  }
  async function cleared(page) {
    await alignment(page).waitFor({ state: 'detached' });
  }
  async function check(page, id) {
    await settle(page);
    const detail = page.waitForResponse(r => r.url() === `${origin}/api/projects/${id}` && r.request().method() === 'GET');
    const state = page.waitForResponse(r => r.url() === `${origin}/api/projects/${id}/state` && r.request().method() === 'GET');
    await button(page, '检查对齐').click();
    assert.equal((await detail).status(), 200);
    assert.equal((await state).status(), 200);
    await alignment(page).waitFor();
    await settle(page);
    for (const evidence of await alignment(page).locator('details.project-alignment-sources').all()) {
      if (await evidence.getAttribute('open') === null) await evidence.locator('summary').click();
    }
    return alignment(page).innerText();
  }
  // Hold an already-fetched real response, rather than simulating its contents.
  async function holdNextDetail(page, url) {
    let release, held, done, failure, first = true;
    // Observe completion without changing the production loader or its fencing.
    await page.evaluate(() => {
      globalThis.alignmentTestLoad = loadProject;
      loadProject = (...args) => {
        const pending = globalThis.alignmentTestLoad(...args);
        globalThis.alignmentTestPending = pending; return pending;
      };
    });
    const gate = new Promise(resolve => { release = resolve; });
    const ready = new Promise(resolve => { held = resolve; });
    const finished = new Promise(resolve => { done = resolve; });
    const handler = async intercepted => {
      if (!first) return intercepted.continue();
      first = false;
      try {
        const response = await intercepted.fetch(); held(); await gate;
        await intercepted.fulfill({ response });
      } catch (error) {
        // Identity reset is permitted to abort its outstanding request.
        if (!/closed|cancel|abort|disposed/i.test(error.message)) failure = error;
      } finally { done(); }
    };
    await page.route(url, handler);
    return {
      ready: async () => {
        let timer;
        try { await Promise.race([ready, new Promise((_, reject) => { timer = setTimeout(() => reject(Error('Expected detail read was not held')), 15000); })]); }
        finally { clearTimeout(timer); }
      },
      finish: async () => {
        release(); if (!first) await finished;
        await page.evaluate(async () => {
          try { await globalThis.alignmentTestPending; }
          finally { loadProject = globalThis.alignmentTestLoad; delete globalThis.alignmentTestLoad; delete globalThis.alignmentTestPending; }
        });
        await page.unroute(url, handler);
        if (failure) throw failure;
      }
    };
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
    const userPages = {};
    for (const who of ['alice', 'bob', 'carol', 'eve']) {
      const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
      const page = userPages[who] = await context.newPage();
      pages.push(page); page.on('pageerror', error => pageErrors.push(error.message));
      await login(page, who);
      // Invoke the real poll explicitly below, preventing interval races from
      // consuming a held read or clearing a screenshot midway through capture.
      await page.evaluate(() => { globalThis.alignmentTestRefresh = refreshProjects; refreshProjects = async () => {}; });
    }
    const { alice, bob, carol, eve } = userPages;
    const project = await ok('alice', 'projects', 'POST', {
      name: '只读对齐检查项目', goal: '项目说明不是已发布的目标或集体承诺', kind: 'temporary', request_id: randomUUID()
    });
    const other = await ok('alice', 'projects', 'POST', {
      name: '另一个对齐目标', goal: '旧按钮不得转向新项目', kind: 'temporary', request_id: randomUUID()
    });
    const route = 'projects/' + project.id;
    const detailUrl = origin + '/api/' + route, stateUrl = detailUrl + '/state';
    const state = async () => (await ok('alice', route + '/state')).items;
    const revision = async () => (await ok('alice', route)).project.revision;
    for (const who of ['bob', 'carol']) {
      await ok('alice', route + '/members', 'POST', { owner: who, expected_revision: await revision() });
      await ok(who, route + '/membership', 'POST', { action: 'accept', expected_revision: await revision() });
    }
    const secret = 'PRIVATE-ALIGNMENT-' + randomUUID();
    await ok('alice', 'messages', 'POST', { message: secret, request_id: randomUUID() });
    await alice.reload(); await alice.locator('#room:not(.hide)').waitFor();
    await alice.evaluate(() => { globalThis.alignmentTestRefresh = refreshProjects; refreshProjects = async () => {}; });
    await alice.locator('#text').fill('UNSENT-' + secret);
    const writes = [], projectReads = [];
    for (const page of pages) page.on('request', request => {
      if (request.method() === 'GET' && request.url().startsWith(origin + '/api/projects')) projectReads.push(request.url());
      if (request.url().startsWith(origin + '/api/') && request.method() !== 'GET') writes.push({ method: request.method(), url: request.url() });
    });
    const messageBefore = await ok('alice', 'messages');
    for (const page of [alice, bob, carol]) {
      await openProject(page, project.name);
      const text = await check(page, project.id);
      assert(text.includes('规则检查 · 项目版本 ' + await revision()));
      assert(/目标/.test(text) && /分工/.test(text), 'Missing published goal and assignment are both actionable');
      assert(/只读|不会.*发布/.test(text), 'Explain read-only behavior');
      assert(!text.includes(secret));
    }
    assert.deepEqual(await state(), []);
    await openProjects(eve);
    assert(!(await panel(eve).innerText()).includes(project.name));
    assert.equal(await button(eve, '检查对齐').count(), 0);
    assert.equal((await api('eve', route)).status, 404);
    assert.equal((await api('eve', route + '/state')).status, 404);
    console.log('PASS three members check missing shared state; outsider cannot read; private draft stays private');

    async function publish(who, kind, content, source_kind = 'human_statement', assignee = null) {
      await ok(who, route + '/state', 'POST', { request_id: randomUUID(), expected_revision: await revision(), content: {
        kind, content, source_kind, assignee, source_note: who + ' 明确发布的独立来源'
      } });
      return (await state()).find(item => item.content === content);
    }
    const human = await publish('bob', 'goal', 'Bob 的目标候选');
    const model = await publish('carol', 'goal', 'Carol 发布的模型目标候选', 'model_suggestion');
    const assignment = await publish('alice', 'assignment', '待 Bob 本人确认的分工', 'human_statement', 'bob');
    const blocker = await publish('carol', 'blocker', '已发布阻塞记录，不推断它仍未解决');
    const withdrawn = await publish('alice', 'decision', 'WITHDRAWN-' + randomUUID());
    await ok('alice', route + '/state/' + withdrawn.id + '/actions', 'POST', {
      action: 'withdraw', expected_revision: await revision(), expected_version: withdrawn.version
    });
    for (const page of [alice, bob, carol]) {
      await poll(page); await cleared(page);
      assert(/重新|再.*检查/.test(await panel(page).innerText()), 'Changed poll asks for a fresh check');
      const text = await check(page, project.id);
      for (const item of [human, model, assignment, blocker]) {
        assert(text.includes(item.id), 'Suggestion references exact source ID ' + item.id);
        assert(text.includes(item.created_by), 'Keep each source actor');
        const source = alignment(page).locator('.project-alignment-source').filter({ hasText: item.id }).first();
        const evidence = await source.innerText();
        assert(evidence.includes('版本 ' + item.version));
        assert(evidence.includes(item.created_by));
        assert(evidence.includes(item.source_kind === 'model_suggestion' ? '模型建议' : '人工陈述'));
        assert(evidence.includes({ goal: '目标', assignment: '分工', blocker: '阻塞' }[item.kind]));
        assert(/待.*确认/.test(evidence));
        assert(evidence.includes(item.kind === 'assignment' ? 'bob' : item.kind === 'blocker' ? 'carol' : 'alice'), 'Correct named confirmer');
      }
      assert(/版本\s*1|v1/.test(text));
      assert(/人工/.test(text) && /模型/.test(text));
      assert(/待.*确认/.test(text) && text.includes('bob'));
      assert(/不代表.*共识|不.*共识|不代表.*承诺/.test(text), 'Independent proposals must not become consensus');
      assert(!text.includes(withdrawn.id) && !text.includes(withdrawn.content), 'Withdrawn material is ignored');
      assert(!text.includes(secret));
    }
    const snapshot = await state(), revBefore = await revision();
    const firstSource = alignment(alice).getByRole('button', { name: '查看共享条目', exact: true }).first();
    await firstSource.click();
    const focusedID = await alice.evaluate(() => document.activeElement?.closest('.project-state-card')?.dataset.item);
    assert([human.id, model.id, assignment.id, blocker.id].includes(focusedID), 'Source jump focuses the original shared card');
    assert(await card(alice, focusedID).isVisible());
    assert.deepEqual(await state(), snapshot); assert.equal(await revision(), revBefore);
    console.log('PASS provenance, named confirmation, withdrawn exclusion and read-only source navigation');

    for (const [size, viewport] of [['desktop', { width: 1440, height: 1000 }], ['mobile', { width: 390, height: 844 }]]) {
      await alice.setViewportSize(viewport);
      for (const colorScheme of ['light', 'dark']) {
        await alice.emulateMedia({ colorScheme });
        await alignment(alice).scrollIntoViewIfNeeded();
        assert(await alice.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'Alignment fits ' + size);
        await alice.screenshot({ path: path.join(screenshotDir, `project-alignment-${size}-${colorScheme}.png`), animations: 'disabled' });
      }
    }
    await alice.setViewportSize({ width: 1440, height: 1000 }); await alice.emulateMedia({ colorScheme: 'light' });
    await button(alice, '刷新共同状态').click(); await cleared(alice);
    await button(alice, '检查对齐').waitFor(); await check(alice, project.id);
    await button(alice, '回到私聊').click(); await cleared(alice);
    assert.equal(await alice.locator('#text').inputValue(), 'UNSENT-' + secret);
    await openProject(alice, project.name); await cleared(alice);
    await check(alice, project.id);
    await button(alice, '项目列表').click(); await cleared(alice);
    await openProject(alice, project.name);
    const staleCheck = await button(alice, '检查对齐').elementHandle();
    await button(alice, '发布共同状态').click();
    await panel(alice).getByLabel('内容', { exact: true }).fill('LOCAL-DRAFT-' + secret);
    await staleCheck.evaluate(node => { node.click(); node.onclick?.(); }); await staleCheck.dispose();
    await cleared(alice);
    assert.equal(await panel(alice).getByLabel('内容', { exact: true }).inputValue(), 'LOCAL-DRAFT-' + secret);
    await button(alice, '取消编辑').click();
    await button(alice, '检查对齐').waitFor();
    console.log('PASS refresh, close, navigation and outstanding private editor clear or block suggestions');

    const oldProjectCheck = await button(alice, '检查对齐').elementHandle();
    await openProject(alice, other.name);
    const readsBeforeProjectSwitch = projectReads.length;
    await oldProjectCheck.evaluate(node => { node.click(); node.onclick?.(); });
    await oldProjectCheck.dispose();
    await cleared(alice);
    assert.equal(projectReads.length, readsBeforeProjectSwitch, 'Detached action must not retarget the new project');
    assert((await panel(alice).locator('.project-title').innerText()).includes(other.name));
    await openProject(alice, project.name);

    // Confirmation invalidity and same-revision snapshots are read-only fixtures:
    // the real server currently invalidates confirmations as new item versions.
    let invalidConfirmation = true;
    await alice.route(stateUrl, async intercepted => {
      const response = await intercepted.fetch(), value = await response.json();
      if (invalidConfirmation) {
        const item = value.items.find(item => item.id === assignment.id);
        item.status = 'confirmed'; item.confirmed_by = 'bob'; item.confirmation_valid = false;
      }
      await intercepted.fulfill({ response, json: value });
    });
    let text = await check(alice, project.id);
    assert(/确认.*失效|失效.*确认/.test(text), 'Invalid confirmation needs rechecking, never treated as consent');
    assert(text.includes(assignment.id));
    invalidConfirmation = false;
    await poll(alice); await cleared(alice);
    assert(/重新|再.*检查/.test(await panel(alice).innerText()));
    await alice.unroute(stateUrl);
    await check(alice, project.id);
    let changedMember = true;
    await alice.route(detailUrl, async intercepted => {
      const response = await intercepted.fetch(), value = await response.json();
      if (changedMember) value.members = value.members.map(member => member.owner === 'carol' ? { ...member, status: 'left' } : member);
      await intercepted.fulfill({ response, json: value });
    });
    await poll(alice); await cleared(alice);
    assert(/重新|再.*检查/.test(await panel(alice).innerText()));
    changedMember = false; await alice.unroute(detailUrl);
    await check(alice, project.id);
    await publish('bob', 'decision', '新版本的已发布待决定项');
    await poll(alice); await cleared(alice);
    text = await check(alice, project.id);
    assert(text.includes('规则检查 · 项目版本 ' + await revision()));
    console.log('PASS invalid confirmation and revision/member/status changes invalidate cached checks');

    // A direct check permission error, not only background polling, must purge
    // both old suggestions and old material, even if a previous check succeeded.
    await settle(bob);
    await bob.route(detailUrl, intercepted => intercepted.fulfill({ status: 403, contentType: 'application/json', body: JSON.stringify({ error: 'permission revoked' }) }));
    const denied = bob.waitForResponse(r => r.url() === detailUrl && r.status() === 403);
    await button(bob, '检查对齐').click(); await denied;
    await cleared(bob);
    await card(bob, human.id).waitFor({ state: 'detached' });
    assert(!(await panel(bob).innerText()).includes(human.content));
    await bob.unroute(detailUrl);
    await openProject(bob, project.name);
    await check(bob, project.id);
    await ok('bob', route + '/membership', 'POST', { action: 'leave', expected_revision: await revision() });
    await poll(bob); await cleared(bob);
    assert.equal(await panel(bob).locator('.project-state-card').count(), 0);
    assert.equal((await api('bob', route + '/state')).status, 404);
    console.log('PASS direct read permission failure and real membership removal purge shared material');

    // Fence checks whose real response arrives after closing/navigating away.
    for (const label of ['回到私聊', '项目列表']) {
      await openProject(alice, project.name); await settle(alice);
      const held = await holdNextDetail(alice, detailUrl);
      try {
        await button(alice, '检查对齐').click(); await held.ready();
        await button(alice, label).click();
      } finally { await held.finish(); }
      await cleared(alice);
    }
    await openProject(alice, project.name); await settle(alice);
    const oldIdentityCheck = await button(alice, '检查对齐').elementHandle();
    const held = await holdNextDetail(alice, detailUrl);
    try {
      await button(alice, '检查对齐').click(); await held.ready();
      await alice.locator('#logout').click(); await alice.locator('#token').waitFor();
      // No page reload: exercise the stale closure against the new identity.
      await alice.locator('#token').fill(tokens.eve); await alice.locator('#enter').click();
      await alice.locator('#room:not(.hide)').waitFor();
      await openProjects(alice);
    } finally { await held.finish(); }
    await cleared(alice);
    const readsBeforeStaleClick = projectReads.length;
    await oldIdentityCheck.evaluate(node => { node.click(); node.onclick?.(); });
    await oldIdentityCheck.dispose();
    assert.equal(projectReads.length, readsBeforeStaleClick, 'Detached old-identity action must not start a new read');
    assert(!(await panel(alice).innerText()).includes(project.name));
    assert.equal(await panel(alice).locator('.project-state-card').count(), 0);
    console.log('PASS delayed reads cannot restore suggestions after close, navigation or identity switch');

    await check(carol, project.id);
    await ok('alice', route + '/archive', 'POST', { expected_revision: await revision() });
    await poll(carol); await cleared(carol);
    assert.equal(await button(carol, '检查对齐').count(), 0, 'Archived project has no active check');
    assert(/归档/.test(await panel(carol).innerText()));
    assert.deepEqual(writes, [], 'All UI checks and source jumps must be GET-only: no publication, confirmation, chat or model request');
    assert.deepEqual(await ok('alice', 'messages'), messageBefore, 'Checking never starts a private model turn');
    assert.deepEqual(pageErrors, []);
    console.log('PASS archived read-only state and no UI writes/model calls throughout');
    console.log('Screenshots: ' + screenshotDir);
  } catch (error) {
    if (pageErrors.length) console.error('Browser exceptions:', pageErrors);
    for (let index = 0; index < pages.length; index++) {
      await pages[index].screenshot({ path: path.join(screenshotDir, `project-alignment-failure-${index}.png`) }).catch(() => {});
    }
    console.error('Failure screenshots: ' + screenshotDir);
    throw error;
  } finally {
    await stop();
    process.removeListener('SIGINT', interrupted); process.removeListener('SIGTERM', interrupted);
    fs.rmSync(temporary, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
