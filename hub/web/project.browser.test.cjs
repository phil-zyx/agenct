/* Three isolated browsers against a real, temporary Hub. All model calls use Mock.
 * Build first: cargo build --locked --manifest-path hub/Cargo.toml
 * Run: node hub/web/project.browser.test.cjs
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
const screenshotDir = process.env.AGENCT_TEST_SCREENSHOT_DIR || fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-project-screens-'));

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
    return result;
  }
  const panel = page => page.locator('#project-panel');
  const card = (page, id) => panel(page).locator('.project-state-card[data-item="' + id + '"]');
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
  async function editor(page, kind, content, source = 'human_statement') {
    await button(page, '发布共同状态').click();
    await panel(page).getByLabel('类型', { exact: true }).selectOption(kind);
    await panel(page).getByLabel('内容', { exact: true }).fill(content);
    await panel(page).getByLabel('来源说明', { exact: true }).fill('本测试用户明确发布的说明');
    await panel(page).getByLabel('来源类型', { exact: true }).selectOption(source);
    if (kind === 'assignment') await panel(page).getByLabel('负责人', { exact: true }).selectOption('bob');
    await button(page, '预览发布').click();
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
    const name = '发布验证项目';
    const secret = 'PRIVATE-CHAT-' + randomUUID();
    await ok('alice', 'messages', 'POST', { message: secret, request_id: randomUUID() });
    await openProjects(alice);
    await button(alice, '创建项目').click();
    await panel(alice).getByLabel('项目名称', { exact: true }).fill(name);
    await panel(alice).getByLabel('项目说明', { exact: true }).fill('只共享主动发布的共同状态');
    await panel(alice).getByLabel('项目类型', { exact: true }).selectOption('temporary');
    await button(alice, '预览创建').click();
    assert.deepEqual(await ok('alice', 'projects'), [], 'Preview must not create a project');
    await commit(alice, '确认创建项目', true);
    await button(alice, '发布共同状态').waitFor();
    const projects = await ok('alice', 'projects');
    assert.equal(projects.length, 1, 'Repeated creation click must create exactly one project');
    const project = projects[0].project || projects[0];
    const route = 'projects/' + project.id;
    const state = async () => (await ok('alice', route + '/state')).items;
    const revision = async () => (await ok('alice', route)).project.revision;
    assert.deepEqual(await state(), []);
    assert.deepEqual(await ok('bob', 'projects'), []);
    console.log('PASS private project creation preview and repeated confirmation');

    for (const who of ['bob', 'carol']) {
      await reloadProject(alice, name);
      await button(alice, '邀请成员').click();
      await panel(alice).getByLabel('邀请成员', { exact: true }).selectOption(who);
      await button(alice, '预览邀请').click();
      assert.deepEqual(await ok(who, 'project-invitations'), [], 'Invitation preview must not send');
      await commit(alice, '确认邀请');
      await openProjects(userPages[who]);
      await button(userPages[who], '加入项目').click();
      assert.equal(await panel(userPages[who]).locator('.project-editor').getByRole('button', { name: '返回修改', exact: true }).count(), 0);
      assert.deepEqual(await ok(who, 'projects'), [], 'Joining requires its explicit confirmation');
      await commit(userPages[who], '确认加入项目');
      await panel(userPages[who]).locator('.project-list-item').filter({ hasText: name }).click();
      await button(userPages[who], '发布共同状态').waitFor();
    }
    await reloadProject(alice, name);
    // Closing the panel retains only a local draft; cancel discards it.
    await editor(alice, 'goal', '取消的草稿不得共享');
    await button(alice, '回到私聊').click();
    assert.deepEqual(await state(), []);
    await alice.locator('#open-projects').click();
    await button(alice, '确认发布给项目成员').waitFor();
    await button(alice, '取消编辑').click();
    await button(alice, '发布共同状态').waitFor();
    assert.deepEqual(await state(), []);
    const items = {};
    for (const kind of ['goal', 'assignment', 'blocker', 'decision']) {
      const before = await state();
      await editor(alice, kind, '共同状态：' + kind, kind === 'decision' ? 'model_suggestion' : 'human_statement');
      assert.deepEqual(await state(), before, 'Editing and preview must not publish');
      assert(!(await panel(alice).innerText()).includes(secret));
      await commit(alice, '确认发布给项目成员', true);
      await panel(alice).getByText('共同状态：' + kind, { exact: true }).waitFor();
      const after = await state();
      assert.equal(after.length, before.length + 1);
      items[kind] = after.find(item => item.kind === kind);
      assert.equal(items[kind].status, 'proposed');
      assert.equal(items[kind].version, 1);
      assert.equal(items[kind].confirmed_by, null);
    }
    for (const who of ['alice', 'bob', 'carol']) {
      await reloadProject(userPages[who], name);
      const shared = await ok(who, route + '/state');
      assert.equal(shared.items.length, 4);
      assert(!JSON.stringify(shared).includes(secret));
      assert(!(await panel(userPages[who]).innerText()).includes(secret));
      for (const item of shared.items) {
        assert((await card(userPages[who], item.id).innerText()).includes(item.source_note));
      }
    }
    assert((await card(carol, items.decision.id).innerText()).includes('模型建议（不是本人承诺）'));
    assert.equal((await api('eve', route + '/state')).status, 404);
    assert(!(JSON.stringify(await ok('bob', 'messages'))).includes(secret));
    assert(!(JSON.stringify(await ok('carol', 'messages'))).includes(secret));
    assert.equal(await card(alice, items.assignment.id).getByRole('button', { name: '确认这项分工', exact: true }).count(), 0);
    for (const label of ['确认这项目标', '确认这项分工', '确认这个阻塞', '确认这项决定']) {
      assert.equal(await button(carol, label).count(), 0, 'Non-author non-assignee has no confirmation action');
    }
    assert.equal(await button(carol, '修订').count(), 0);
    assert.equal(await button(carol, '撤回').count(), 0);
    console.log('PASS three members see only explicitly published state, provenance, and authorized actions');

    await card(bob, items.assignment.id).getByRole('button', { name: '确认这项分工', exact: true }).click();
    assert.equal(await panel(bob).locator('.project-editor').getByRole('button', { name: '返回修改', exact: true }).count(), 0);
    assert.equal((await state()).find(item => item.id === items.assignment.id).version, 1);
    assert((await panel(bob).innerText()).includes('共同状态：assignment'));
    const confirmed = await commit(bob, '确认当前版本');
    assert.equal(confirmed.request().postDataJSON().expected_version, 1, 'Confirm the exact previewed version');
    await card(bob, items.assignment.id).getByText(/bob/).first().waitFor();
    let assignment = (await state()).find(item => item.id === items.assignment.id);
    assert.equal(assignment.version, 2);
    assert.equal(assignment.confirmed_by, 'bob');
    assert.equal(assignment.confirmation_valid, true);
    await reloadProject(alice, name);
    await card(alice, assignment.id).getByRole('button', { name: '修订', exact: true }).click();
    await panel(alice).getByLabel('内容', { exact: true }).fill('修订后的分工必须重新确认');
    await button(alice, '预览发布').click();
    assert.equal((await state()).find(item => item.id === assignment.id).version, 2);
    await commit(alice, '确认发布给项目成员');
    await panel(alice).getByText('修订后的分工必须重新确认', { exact: true }).waitFor();
    assignment = (await state()).find(item => item.id === assignment.id);
    assert.equal(assignment.version, 3);
    assert.equal(assignment.status, 'proposed');
    assert.equal(assignment.confirmed_by, null);
    assert.equal(assignment.confirmation_valid, false);
    await card(alice, assignment.id).getByRole('button', { name: '版本记录', exact: true }).click();
    await panel(alice).getByText('共同状态：assignment', { exact: true }).first().waitFor();
    assert.equal((await ok('alice', route + '/state/' + assignment.id)).versions.length, 3);
    console.log('PASS exact version confirmation, explicit revision, reset confirmation and history');

    // Isolate the server conflict from the separately tested polling guard.
    // Drain outstanding project reads before deliberately holding an old preview.
    await reloadProject(bob, name);
    await bob.evaluate(() => { refreshProjects = async () => {}; });
    await bob.waitForFunction(() => !projectRead);
    await card(bob, assignment.id).getByRole('button', { name: '确认这项分工', exact: true }).click();
    const revised = await ok('alice', route + '/state/' + assignment.id + '/actions', 'POST', {
      action: 'revise', expected_revision: await revision(), expected_version: assignment.version,
      content: { kind: 'assignment', content: '另一窗口已更新', assignee: 'bob', source_kind: 'human_statement', source_note: '新版本来源' }
    });
    const conflict = bob.waitForResponse(response => response.url().endsWith('/state/' + assignment.id + '/actions') && response.status() === 409);
    await button(bob, '确认当前版本').click();
    await conflict;
    await panel(bob).getByText('项目已更新，请取消编辑后重新核对。', { exact: false }).waitFor();
    assert(await button(bob, '确认当前版本').isDisabled(), 'Stale preview cannot be resubmitted');
    assignment = (await state()).find(item => item.id === assignment.id);
    assert.equal(assignment.version, revised.item.version);
    assert.equal(assignment.status, 'proposed');
    console.log('PASS stale confirmation refuses to endorse a newer version');

    // Commit at the real Hub but drop the browser response, then retry only via
    // the UI. Its retained request ID must reconcile instead of duplicating.
    await reloadProject(alice, name);
    await editor(alice, 'goal', '丢失响应也只能创建一次');
    let intercepted = 0;
    let dropped;
    const droppedResponse = new Promise(resolve => { dropped = resolve; });
    const lost = origin + '/api/' + route + '/state';
    await alice.route(lost, async route => {
      if (route.request().method() !== 'POST') return route.continue();
      intercepted++;
      if (intercepted === 1) { await route.fetch(); await route.abort('failed'); dropped(); }
      else await route.continue();
    });
    await button(alice, '确认发布给项目成员').click();
    await droppedResponse;
    await button(alice, '核对并重试原提交').waitFor();
    assert.equal((await state()).filter(item => item.content === '丢失响应也只能创建一次').length, 1);
    await button(alice, '核对并重试原提交').waitFor();
    // Let background polling observe the server-side commit before retrying.
    await pause(3500);
    assert(await button(alice, '核对并重试原提交').isEnabled(), 'Polling must preserve idempotent recovery');
    await commit(alice, '核对并重试原提交');
    await panel(alice).getByText('丢失响应也只能创建一次', { exact: true }).waitFor();
    assert.equal((await state()).filter(item => item.content === '丢失响应也只能创建一次').length, 1);
    await alice.unroute(lost);
    console.log('PASS committed response loss and repeated publish do not duplicate state');

    await reloadProject(alice, name);
    await card(alice, items.blocker.id).getByRole('button', { name: '撤回', exact: true }).click();
    assert.equal(await panel(alice).locator('.project-editor').getByRole('button', { name: '返回修改', exact: true }).count(), 0);
    assert.equal((await state()).find(item => item.id === items.blocker.id).status, 'proposed');
    await commit(alice, '确认撤回');
    assert.equal((await state()).find(item => item.id === items.blocker.id).status, 'withdrawn');
    assert.equal((await ok('alice', route + '/state/' + items.blocker.id)).versions.length, 2);
    console.log('PASS withdrawing requires explicit confirmation and preserves history');

    await reloadProject(carol, name);
    assert.equal(await panel(carol).locator('.project-state-card').count(), 5);
    await carol.setViewportSize({ width: 390, height: 844 });
    await carol.waitForFunction(() => document.querySelector('.chat-pane').inert);
    assert(await carol.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'Project layout must fit mobile viewport');
    await carol.screenshot({ path: path.join(screenshotDir, 'project-mobile.png') });
    await carol.locator('#open-chat').click();
    await carol.locator('#text').waitFor();
    await carol.waitForFunction(() => !document.querySelector('.chat-pane').inert);
    const unsent = '尚未发送的私人想法，不得被项目讨论覆盖';
    const privateMessages = await ok('carol', 'messages');
    await carol.locator('#text').fill(unsent);
    await openProject(carol, name);
    await card(carol, items.goal.id).waitFor();
    await carol.waitForFunction(() => document.querySelector('.chat-pane').inert);
    assert.equal(await carol.locator('#text').inputValue(), unsent);
    await button(carol, '和助手讨论').click();
    await panel(carol).getByText('请先处理当前输入或已选择的聊天上下文，再讨论项目。', { exact: true }).waitFor();
    assert.equal(await carol.locator('#text').inputValue(), unsent);
    assert(await panel(carol).isVisible());
    assert.deepEqual(await ok('carol', 'messages'), privateMessages, 'Discuss must not send or overwrite an existing private draft');
    await button(carol, '回到私聊').click();
    await carol.waitForFunction(() => !document.querySelector('.chat-pane').inert);
    assert.equal(await carol.locator('#text').inputValue(), unsent);
    await carol.locator('#text').fill('');
    await openProject(carol, name);
    await panel(carol).evaluate(element => { element.scrollTop = 0; });
    await panel(alice).evaluate(element => { element.scrollTop = 0; });
    await carol.emulateMedia({ colorScheme: 'dark' });
    await carol.screenshot({ path: path.join(screenshotDir, 'project-mobile-dark.png') });
    await alice.emulateMedia({ colorScheme: 'dark' });
    await alice.screenshot({ path: path.join(screenshotDir, 'project-desktop-dark.png') });
    await alice.emulateMedia({ colorScheme: 'light' });
    await carol.emulateMedia({ colorScheme: 'light' });
    console.log('PASS mobile panel isolates focus, restores private composer, and preserves unsent text');
    await carol.locator('#logout').click();
    await carol.locator('#token').waitFor();
    await login(carol, 'eve');
    await openProjects(carol);
    await button(carol, '创建项目').waitFor();
    assert(!(await panel(carol).innerText()).includes(name));
    assert.equal(await panel(carol).locator('.project-state-card').count(), 0);
    assert.deepEqual(await ok('eve', 'projects'), []);
    await alice.screenshot({ path: path.join(screenshotDir, 'project-desktop.png') });
    // A poll already in flight before a commit must not poison the retained
    // request when it observes the newer server revision before POST fails.
    await reloadProject(alice, name);
    await alice.evaluate(() => { globalThis.projectTestRead = refreshProjects; refreshProjects = async () => {}; });
    await alice.waitForFunction(() => !projectRead);
    await editor(alice, 'goal', '旧轮询不能阻止原编号重试');
    const gate = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
    const bounded = async promise => {
      let timer;
      try { return await Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(Error('Held-response regression timed out')), 15000); })]); }
      finally { clearTimeout(timer); }
    };
    const pollHeld = gate(), releasePoll = gate(), postCommitted = gate(), releaseFailure = gate();
    let holdNextGet = true, holdNextPost = true;
    const submittedBodies = [];
    await alice.route(lost, async interceptedRoute => {
      const request = interceptedRoute.request();
      if (request.method() === 'GET' && holdNextGet) {
        holdNextGet = false; pollHeld.resolve(); await releasePoll.promise;
        // Read after the POST committed, so fetchProject must re-read the
        // earlier detail revision and eventually return the newer snapshot.
        await interceptedRoute.fulfill({ response: await interceptedRoute.fetch() });
      } else if (request.method() === 'POST') {
        submittedBodies.push(request.postDataJSON());
        if (holdNextPost) {
          holdNextPost = false;
          await interceptedRoute.fetch(); postCommitted.resolve();
          await releaseFailure.promise; await interceptedRoute.abort('failed');
        } else await interceptedRoute.continue();
      } else await interceptedRoute.continue();
    });
    let poll;
    try {
      // Background project reads were disabled and drained before interception.
      poll = alice.evaluate(() => globalThis.projectTestRead());
      await bounded(pollHeld.promise);
      await button(alice, '确认发布给项目成员').click();
      await bounded(postCommitted.promise);
      releasePoll.resolve();
      await bounded(poll);
      releaseFailure.resolve();
      await button(alice, '核对并重试原提交').waitFor();
      assert(await button(alice, '核对并重试原提交').isEnabled());
      assert.equal(await alice.evaluate(() => projectDraft.invalid), false, 'Pre-commit read must not invalidate the draft');
      await commit(alice, '核对并重试原提交');
      await panel(alice).getByText('旧轮询不能阻止原编号重试', { exact: true }).waitFor();
      assert.equal(submittedBodies.length, 2);
      assert.deepEqual(submittedBodies[1], submittedBodies[0], 'Retry must preserve exact body and request ID');
      assert.equal((await state()).filter(item => item.content === '旧轮询不能阻止原编号重试').length, 1);
    } finally {
      releasePoll.resolve(); releaseFailure.resolve();
      if (poll) await poll.catch(() => {});
      await alice.unroute(lost);
    }
    console.log('PASS pre-existing poll cannot invalidate uncertain idempotent publication');

    // Historical membership remains in the server record, but must not be
    // presented as the current roster or an eligible assignment target.
    await ok('alice', route + '/members', 'POST', { owner: 'eve', expected_revision: await revision() });
    await ok('eve', route + '/membership', 'POST', { action: 'decline', expected_revision: await revision() });
    await ok('carol', route + '/membership', 'POST', { action: 'leave', expected_revision: await revision() });
    const inactive = (await ok('alice', route)).members;
    assert(inactive.some(member => member.owner === 'eve' && member.status === 'declined'));
    assert(inactive.some(member => member.owner === 'carol' && member.status === 'left'));
    await reloadProject(alice, name);
    const roster = await panel(alice).locator('.project-members').innerText();
    assert(!roster.includes('eve') && !roster.includes('carol'));
    assert(roster.includes('alice') && roster.includes('bob'));
    await button(alice, '发布共同状态').click();
    await panel(alice).getByLabel('类型', { exact: true }).selectOption('assignment');
    const options = await panel(alice).getByLabel('负责人', { exact: true }).locator('option').allTextContents();
    assert(!options.includes('eve') && !options.includes('carol'));
    await button(alice, '取消编辑').click();
    console.log('PASS declined and departed members are absent from current roster and assignee choices');
    assert.deepEqual(pageErrors, []);
    console.log('PASS refresh persistence, mobile navigation, account-switch isolation, no browser exceptions');
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
