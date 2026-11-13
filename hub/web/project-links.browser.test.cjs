/* Three isolated browsers against a real, temporary Hub. All model calls use Mock.
 * Build first: cargo build --locked --manifest-path hub/Cargo.toml
 * Run: node hub/web/project-links.browser.test.cjs
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
const screenshotDir = process.env.AGENCT_TEST_SCREENSHOT_DIR || fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-project-links-screens-'));

async function freePort() {
  const socket = net.createServer();
  await new Promise(resolve => socket.listen(0, '127.0.0.1', resolve));
  const port = socket.address().port;
  await new Promise(resolve => socket.close(resolve));
  return port;
}

(async () => {
  assert(fs.existsSync(binary), 'Build the Hub before running this test');
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-project-'));
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
  async function commit(page, label, repeated = false) {
    const response = page.waitForResponse(r => r.request().method() === 'POST' && r.url().includes('/api/projects'));
    if (repeated) await button(page, label).evaluate(b => { b.click(); b.click(); });
    else await button(page, label).click();
    const result = await response;
    assert.equal(result.status(), 200, label + ' must succeed');
    await page.waitForFunction(() => !projectBusy && !projectDraft && document.getElementById('project-notice')?.textContent.startsWith('操作已保存。'));
    return result;
  }
  const panel = page => page.locator('#project-panel');
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
  async function reloadProject(page, name) {
    // These explicit reloads intentionally discard local previews/history.
    // Playwright otherwise dismisses beforeunload and can leave navigation pending.
    const acceptReload = dialog => { void dialog.accept(); };
    page.on('dialog', acceptReload);
    try { await page.reload(); } finally { page.off('dialog', acceptReload); }
    await page.locator('#room:not(.hide)').waitFor();
    await openProject(page, name);
  }
  try {
    hub = spawn(binary, [], { cwd: root, stdio: ['ignore', 'ignore', 'pipe'], env: {
      PATH: process.env.PATH, HOME: process.env.HOME, LANG: process.env.LANG || 'C.UTF-8',
      AGENCT_BIND: `127.0.0.1:${port}`, AGENCT_DATABASE: path.join(temporary, 'hub.sqlite3'),
      AGENCT_RUNTIME: 'mock', AGENCT_USERS_JSON: JSON.stringify(users), AGENCT_MEMBER_ADMINS_JSON: JSON.stringify(['eve'])
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
    const contexts = {};
    const userPages = {};
    for (const who of ['alice', 'bob', 'carol']) {
      contexts[who] = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
      userPages[who] = await contexts[who].newPage();
      pages.push(userPages[who]);
      userPages[who].on('pageerror', error => pageErrors.push(error.message));
      await login(userPages[who], who);
    }
    const { alice, bob, carol } = userPages;

    const name = '三人任务关联验收';
    const secret = 'PRIVATE-TASK-' + randomUUID();
    const proposal = await ok('alice', 'proposals', 'POST', { owner: 'bob', summary: secret, request_id: randomUUID() });
    const { task_id: taskId } = await ok('alice', 'proposals/' + proposal.id + '/actions', 'POST', { action: 'confirm', expected_revision: 0, details: secret + '-DETAILS' });
    const project = await ok('alice', 'projects', 'POST', { name, goal: '只公开双方同意的协调标签', kind: 'temporary', request_id: randomUUID() });
    const route = 'projects/' + project.id;
    const revision = async () => (await ok('alice', route)).project.revision;
    const join = async who => {
      await ok('alice', route + '/members', 'POST', { owner: who, expected_revision: await revision() });
      await ok(who, route + '/membership', 'POST', { action: 'accept', expected_revision: await revision() });
    };
    await join('bob'); await join('carol');
    const link = page => panel(page).locator('.project-task-card[data-task="' + taskId + '"]');
    async function propose(label) {
      await reloadProject(alice, name);
      await button(alice, '关联我的任务').click();
      await panel(alice).getByLabel('我的任务', { exact: true }).selectOption(taskId);
      assert.equal(await panel(alice).getByLabel('共享协调标签', { exact: true }).inputValue(), '', 'Private summary must not seed shared label');
      await panel(alice).getByLabel('共享协调标签', { exact: true }).fill(label);
      await button(alice, '预览关联').click();
      assert(!(await panel(alice).locator('.project-preview').innerText()).includes(secret));
    }
    async function approve() {
      await reloadProject(bob, name);
      await link(bob).getByRole('button', { name: '同意公开关联', exact: true }).click();
      await commit(bob, '确认公开关联', true);
    }
    await propose('已核对的共享进度');
    assert.deepEqual((await ok('carol', route)).tasks, []);
    assert.deepEqual((await ok('alice', route)).tasks, [], 'Preview must not propose');
    await button(alice, '回到私聊').click();
    await alice.locator('#open-projects').click();
    await button(alice, '取消编辑').click();
    assert.deepEqual((await ok('alice', route)).tasks, [], 'Close and cancel must not publish');
    await propose('已核对的共享进度');
    await commit(alice, '确认提议关联', true);
    assert.equal((await ok('alice', route)).tasks[0].status, 'pending');
    await reloadProject(carol, name);
    assert.equal(await link(carol).count(), 0);
    assert.deepEqual((await ok('carol', route)).tasks, [], 'Third member cannot see pending link');
    await reloadProject(alice, name);
    assert.equal(await link(alice).getByRole('button', { name: '同意公开关联', exact: true }).count(), 0, 'Proposer cannot approve for the other party');
    await approve();
    await reloadProject(carol, name);
    await link(carol).getByText('已核对的共享进度', { exact: true }).waitFor();
    assert.equal(await link(carol).getByRole('button', { name: '打开我的任务', exact: true }).count(), 0);
    const projection = (await ok('carol', route)).tasks[0];
    assert.equal(projection.can_open_task, false);
    assert.equal(projection.status, 'active');
    assert(!JSON.stringify(projection).includes(secret));
    assert(!(await panel(carol).innerText()).includes(secret));
    for (const suffix of ['', '/drafts']) assert.equal((await api('carol', 'tasks/' + taskId + suffix)).status, 404);
    assert.equal((await api('carol', 'tasks/' + taskId + '/actions', 'POST', { action: 'accept', expected_revision: 0 })).status, 404);
    await carol.setViewportSize({ width: 390, height: 844 });
    assert(await carol.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
    await carol.screenshot({ path: path.join(screenshotDir, 'project-links-mobile.png') });
    console.log('PASS explicit bilateral consent, private task ACL and mobile shared projection');

    // A stale revoke preview must fail closed, never silently replay its write.
    await reloadProject(bob, name);
    await bob.evaluate(() => { refreshProjects = async () => {}; });
    await bob.waitForFunction(() => !projectRead);
    await link(bob).getByRole('button', { name: '撤回任务关联', exact: true }).click();
    await ok('alice', route + '/members', 'POST', { owner: 'eve', expected_revision: await revision() });
    const conflict = bob.waitForResponse(r => r.status() === 409 && r.url().includes('/tasks/'));
    await button(bob, '确认撤回关联').click(); await conflict;
    assert(await button(bob, '确认撤回关联').isDisabled());
    assert.equal((await ok('carol', route)).tasks.length, 1);
    await button(bob, '取消编辑').click();
    await link(bob).getByRole('button', { name: '撤回任务关联', exact: true }).click();
    await commit(bob, '确认撤回关联');
    assert.deepEqual((await ok('carol', route)).tasks, []);
    console.log('PASS stale preview rejected and participant withdrawal removes shared projection');

    // Response loss after commit must require read reconciliation, not repeat POST.
    await propose('响应丢失的关联');
    const postUrl = origin + '/api/' + route + '/tasks';
    let posts = 0;
    await alice.route(postUrl, async intercepted => {
      if (intercepted.request().method() !== 'POST') return intercepted.continue();
      posts++; await intercepted.fetch(); await intercepted.abort('failed');
    });
    await button(alice, '确认提议关联').click();
    await panel(alice).getByText(/提交结果未知/).waitFor();
    assert(await button(alice, '确认提议关联').isDisabled());
    await pause(500);
    assert.equal(posts, 1);
    assert.equal((await ok('alice', route)).tasks[0].status, 'pending');
    alice.once('dialog', dialog => dialog.accept());
    await button(alice, '取消编辑').click();
    await alice.unroute(postUrl);
    await link(alice).waitFor();
    await approve();
    console.log('PASS uncertain non-idempotent write is reconciled by reads without replay');

    // Leaving revokes consent. Rejoining cannot resurrect it.
    await reloadProject(bob, name);
    await button(bob, '退出项目').click();
    assert.equal((await ok('bob', route + '/membership')).status, 'active');
    await commit(bob, '确认退出项目');
    assert.equal((await api('bob', route)).status, 404);
    assert.deepEqual((await ok('carol', route)).tasks, []);
    await join('bob');
    assert.deepEqual((await ok('bob', route)).tasks, []);
    await propose('重新明确同意的关联'); await commit(alice, '确认提议关联'); await approve();
    await reloadProject(alice, name);
    await button(alice, '移除成员').click();
    await panel(alice).getByLabel('移除成员', { exact: true }).selectOption('bob');
    await button(alice, '预览移除').click();
    assert.equal((await ok('bob', route + '/membership')).status, 'active');
    await commit(alice, '确认移除成员');
    assert.equal((await api('bob', route)).status, 404);
    assert.deepEqual((await ok('carol', route)).tasks, []);
    assert.equal((await api('bob', 'tasks/' + taskId)).status, 200, 'Removing project access must not cancel private task access');
    console.log('PASS leave, rejoin and owner removal revoke links but preserve private tasks');

    // A held result from an old identity must not repopulate a new session.
    await reloadProject(carol, name);
    await carol.evaluate(() => { refreshProjects = async () => {}; });
    await carol.waitForFunction(() => !projectRead);
    let release, entered;
    const held = new Promise(resolve => { entered = resolve; });
    const gate = new Promise(resolve => { release = resolve; });
    const detailUrl = origin + '/api/' + route;
    await carol.route(detailUrl, async intercepted => {
      const response = await intercepted.fetch(); entered(); await gate;
      await intercepted.fulfill({ response }).catch(() => {});
    });
    const bounded = async promise => {
      let timer;
      try { return await Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(Error('Held response timed out')), 10000); })]); }
      finally { clearTimeout(timer); }
    };
    try {
    await button(carol, '刷新共同状态').click(); await bounded(held);
    await carol.locator('#logout').click();
    await carol.locator('#token').fill(tokens.eve); await carol.locator('#enter').click();
    await carol.locator('#room:not(.hide)').waitFor();
    release(); await carol.unroute(detailUrl); await pause(200);
    assert.equal(await panel(carol).locator('.project-task-card').count(), 0);
    assert.equal(await panel(carol).locator('.project-title').count(), 0);
    console.log('PASS account change discards held project response');
    } finally { release(); await carol.unroute(detailUrl); }

    await join('bob');
    await propose('归档后仍可撤回'); await commit(alice, '确认提议关联'); await approve();
    await reloadProject(alice, name);
    await button(alice, '归档项目').click();
    assert.equal((await ok('alice', route)).project.status, 'active');
    // Delay the loadProject *continuation*, not its network request: logout
    // aborts fetch, so holding a GET alone cannot reliably test late notices.
    await alice.evaluate(() => { refreshProjects = async () => {}; });
    await alice.waitForFunction(() => !projectRead);
    await alice.evaluate(() => {
      const originalLoad = loadProject, originalCommit = commitProject;
      let holdNext = true;
      const gate = new Promise(resolve => { globalThis.releaseSavedLoad = resolve; });
      globalThis.testSavedLoadHeld = false; globalThis.testOldCommitSettled = false;
      loadProject = async (...args) => {
        const hold = holdNext; holdNext = false;
        const result = await originalLoad(...args);
        if (hold) { globalThis.testSavedLoadHeld = true; await gate; }
        return result;
      };
      commitProject = async (...args) => { try { return await originalCommit(...args); } finally { globalThis.testOldCommitSettled = true; } };
    });
    try {
      const saved = alice.waitForResponse(r => r.url().endsWith('/archive') && r.request().method() === 'POST');
      await button(alice, '确认归档项目').click();
      assert.equal((await saved).status(), 200);
      await alice.waitForFunction(() => globalThis.testSavedLoadHeld);
      await alice.locator('#logout').click();
      await alice.locator('#token').fill(tokens.eve); await alice.locator('#enter').click();
      await alice.locator('#room:not(.hide)').waitFor();
      await openProjects(alice);
      await button(alice, '创建项目').waitFor();
      const newSessionNotice = await alice.locator('#project-notice').innerText();
      assert.equal(await alice.evaluate(() => globalThis.testOldCommitSettled), false, 'Old continuation must still be held after new session opens');
      await alice.evaluate(() => globalThis.releaseSavedLoad());
      await alice.waitForFunction(() => globalThis.testOldCommitSettled);
      assert.equal(await alice.locator('#project-notice').innerText(), newSessionNotice, 'Old commit must not overwrite new-session notice');
      assert.equal(await panel(alice).locator('.project-title').count(), 0);
    } finally { await alice.evaluate(() => globalThis.releaseSavedLoad()); }
    await alice.locator('#logout').click();
    await login(alice, 'alice'); await openProject(alice, name);
    console.log('PASS post-success read and notice cannot cross account switch');
    assert.equal((await ok('alice', route)).project.status, 'archived');
    assert.equal(await button(alice, '关联我的任务').count(), 0);
    assert.equal(await button(alice, '邀请成员').count(), 0);
    assert.equal((await api('bob', 'tasks/' + taskId)).status, 200);
    await alice.screenshot({ path: path.join(screenshotDir, 'project-links-desktop.png') });
    await reloadProject(bob, name);
    await link(bob).getByRole('button', { name: '撤回任务关联', exact: true }).click();
    await commit(bob, '确认撤回关联');
    assert.deepEqual((await ok('alice', route)).tasks, []);
    await reloadProject(alice, name);
    await button(alice, '移除成员').click();
    await panel(alice).getByLabel('移除成员', { exact: true }).selectOption('carol');
    await button(alice, '预览移除').click(); await commit(alice, '确认移除成员');
    assert.equal((await api('carol', route)).status, 404);
    console.log('PASS archived projects still permit link withdrawal and owner removal');

    // Real Hub account disable hides the project, but the member-only endpoint
    // still permits withdrawing membership without loading private project data.
    await reloadProject(bob, name);
    await bob.evaluate(() => { refreshProjects = async () => {}; });
    await bob.waitForFunction(() => !projectRead);
    const members = await ok('eve', 'members');
    const account = members.find(member => member.owner === 'alice' || member.id === 'alice');
    assert(account, 'Admin must resolve creator account');
    const disabled = await ok('eve', 'members/alice', 'PUT', { display_name: account.display_name, color: account.color, status: 'disabled', expected_revision: account.revision });
    assert.equal((await api('bob', route)).status, 404);
    await button(bob, '刷新共同状态').click();
    await button(bob, '退出不可访问的项目').click();
    assert.equal(await panel(bob).locator('.project-task-card').count(), 0);
    await commit(bob, '确认退出项目');
    await ok('eve', 'members/alice', 'PUT', { display_name: disabled.display_name, color: disabled.color, status: 'active', expected_revision: disabled.revision });
    assert.equal((await api('bob', route)).status, 404, 'Restoring creator must not restore withdrawn membership');
    console.log('PASS creator unavailable recovery permits membership withdrawal without project read');
    assert.deepEqual(pageErrors, []);
    console.log('PASS explicit archive preserves task lifecycle; no browser exceptions');
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
