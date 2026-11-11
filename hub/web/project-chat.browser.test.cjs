/* Three isolated browsers against a real, temporary Hub. All model calls use Mock.
 * Build first: cargo build --locked --manifest-path hub/Cargo.toml
 * Run: node hub/web/project-chat.browser.test.cjs
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
const screenshotDir = process.env.AGENCT_TEST_SCREENSHOT_DIR || fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-project-chat-screens-'));

async function freePort() {
  const socket = net.createServer();
  await new Promise(resolve => socket.listen(0, '127.0.0.1', resolve));
  const port = socket.address().port;
  await new Promise(resolve => socket.close(resolve));
  return port;
}

(async () => {
  assert(fs.existsSync(binary), 'Build the Hub before running this test');
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-project-chat-'));
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
    const review = panel(page).getByLabel('我已核对项目、共享文字和来源', { exact: true });
    if (await review.count() && !(await review.isChecked())) {
      assert(await button(page, label).isDisabled(), 'Candidate publication requires explicit review');
      await review.check();
    }
    const response = page.waitForResponse(r => r.request().method() === 'POST' && r.url().includes('/api/projects'));
    if (repeated) await button(page, label).evaluate(b => { b.click(); b.click(); });
    else await button(page, label).click();
    const result = await response;
    assert.equal(result.status(), 200, label + ' must succeed');
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
    // Seed only via public APIs. All candidate/publish operations below use the UI.
    const project = await ok('alice', 'projects', 'POST', {
      name: '聊天候选共享项目', goal: '仅明确发布的材料可见', kind: 'temporary', request_id: randomUUID()
    });
    const other = await ok('alice', 'projects', 'POST', {
      name: '另一目标项目', goal: '不得误选之前查看的项目', kind: 'temporary', request_id: randomUUID()
    });
    const route = 'projects/' + project.id;
    const state = async () => (await ok('alice', route + '/state')).items;
    const revision = async () => (await ok('alice', route)).project.revision;
    for (const who of ['bob', 'carol']) {
      await ok('alice', route + '/members', 'POST', { owner: who, expected_revision: await revision() });
      await ok(who, route + '/membership', 'POST', { action: 'accept', expected_revision: await revision() });
    }
    const secret = 'UNRELATED-PRIVATE-' + randomUUID();
    const source = '单条消息 <img src=x onerror="globalThis.projectXss=true"> **只分享整理后的内容**';
    await ok('alice', 'messages', 'POST', { message: secret, request_id: randomUUID() });
    await ok('alice', 'messages', 'POST', { message: source, request_id: randomUUID() });
    await alice.reload();
    await alice.locator('#room:not(.hide)').waitFor();
    const sourceBubble = page => page.locator('.msg-user').filter({ has: page.locator('.msg-body', { hasText: source }) });
    const promote = page => sourceBubble(page).getByRole('button', { name: '提为项目候选', exact: true });
    const selection = page => panel(page).getByLabel('发布到项目', { exact: true });
    const field = (page, name) => panel(page).getByLabel(name, { exact: true });
    const projectPosts = [];
    alice.on('request', request => {
      if (request.method() === 'POST' && /\/api\/projects(?:\/|$)/.test(request.url())) {
        projectPosts.push({ url: request.url(), body: request.postDataJSON() });
      }
    });
    async function prepare(page, text = '经过本人整理的候选', id = project.id) {
      await promote(page).click();
      await selection(page).waitFor();
      assert.equal(await selection(page).inputValue(), '', 'Each candidate must explicitly choose a project');
      await selection(page).selectOption(id);
      await button(page, '整理候选').click();
      await field(page, '内容').waitFor();
      assert.equal(await field(page, '内容').inputValue(), source);
      assert.equal(await field(page, '来源类型').inputValue(), 'human_statement');
      assert(await field(page, '来源类型').isDisabled(), 'Message actor provenance cannot be relabeled');
      const note = await field(page, '来源说明').inputValue();
      assert(note.includes('alice'));
      assert(!note.includes(secret) && !note.includes(source), 'Source note carries metadata only');
      await field(page, '内容').fill(text);
      await button(page, '预览发布').click();
      await button(page, '确认发布给项目成员').waitFor();
      return note;
    }
    // A previously viewed project is never an implicit publication destination.
    await openProject(alice, other.name);
    await button(alice, '回到私聊').click();
    await promote(alice).evaluate(b => { b.click(); b.click(); });
    await selection(alice).waitFor();
    assert.equal(await selection(alice).inputValue(), '');
    assert.equal(await selection(alice).count(), 1, 'Repeated entry creates only one local candidate');
    assert.equal(projectPosts.length, 0);
    await button(alice, '回到私聊').click();
    await alice.locator('#open-projects').click();
    await selection(alice).waitFor();
    assert.equal(await selection(alice).inputValue(), '');
    await button(alice, '取消编辑').click();
    assert.deepEqual(await state(), []);
    console.log('PASS empty explicit destination, repeated entry, close/reopen and cancel remain private');

    // Cancel while project reads are outstanding. Their late response must not
    // replace a newer candidate, nor carry the old destination into it.
    let releaseRead, markHeld;
    const held = new Promise(resolve => { markHeld = resolve; });
    const release = new Promise(resolve => { releaseRead = resolve; });
    const detailUrl = origin + '/api/' + route;
    await alice.route(detailUrl, async intercepted => {
      const response = await intercepted.fetch(); markHeld(); await release;
      await intercepted.fulfill({ response });
    });
    await promote(alice).click(); await selection(alice).selectOption(project.id);
    await button(alice, '整理候选').click();
    await Promise.race([held, pause(15000).then(() => { throw Error('Project detail read was never held'); })]);
    assert(await selection(alice).isDisabled());
    await button(alice, '取消编辑').click();
    await promote(alice).click(); await selection(alice).waitFor();
    releaseRead(); await alice.unroute(detailUrl);
    await pause(200);
    assert.equal(await selection(alice).inputValue(), '');
    assert.equal(await field(alice, '内容').count(), 0);
    await button(alice, '取消编辑').click();
    assert.equal(projectPosts.length, 0);
    console.log('PASS canceled asynchronous project read cannot replace newer candidate');

    const stateUrl = origin + '/api/' + route + '/state';
    await alice.route(stateUrl, intercepted => intercepted.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ error: 'test read unavailable' }) }));
    await promote(alice).click(); await selection(alice).selectOption(project.id);
    await button(alice, '整理候选').click();
    await panel(alice).getByText(/无法准备候选/).waitFor();
    assert.equal(await field(alice, '内容').count(), 0);
    assert.equal(projectPosts.length, 0);
    await button(alice, '取消编辑').click(); await alice.unroute(stateUrl);
    console.log('PASS read failure never creates an editable stale-state publication');

    const note = await prepare(alice, '公开的分工候选');
    const review = page => panel(page).getByLabel('我已核对项目、共享文字和来源', { exact: true });
    async function invokeStale(handle) {
      const before = projectPosts.length;
      await handle.evaluate(node => { node.click(); node.onclick?.(); });
      await pause(100);
      assert.equal(projectPosts.length, before, 'Detached/hidden/stale confirmation must not POST');
    }
    await review(alice).check();
    const oldPreview = await button(alice, '确认发布给项目成员').elementHandle();
    await button(alice, '返回修改').click();
    await invokeStale(oldPreview);
    await field(alice, '内容').fill('😀'.repeat(4001));
    await button(alice, '预览发布').click();
    await panel(alice).getByText(/4000 个字符/).waitFor();
    assert.equal(await button(alice, '确认发布给项目成员').count(), 0);
    assert.equal(projectPosts.length, 0);
    await field(alice, '内容').fill('😀'.repeat(4000));
    await button(alice, '预览发布').click();
    await button(alice, '确认发布给项目成员').waitFor();
    assert((await panel(alice).locator('.project-preview').innerText()).includes('😀'.repeat(4000)), '4000 Unicode scalars pass without truncation');
    await invokeStale(oldPreview);
    await button(alice, '返回修改').click();
    await field(alice, '内容').fill('公开的分工候选');
    await field(alice, '类型').selectOption('assignment');
    await field(alice, '负责人').selectOption('bob');
    await button(alice, '预览发布').click();
    await invokeStale(oldPreview);
    await oldPreview.dispose();
    await review(alice).check();
    const hiddenPreview = await button(alice, '确认发布给项目成员').elementHandle();
    await button(alice, '回到私聊').click();
    await invokeStale(hiddenPreview);
    await alice.locator('#open-projects').click();
    assert(!(await review(alice).isChecked()), 'Close/reopen clears publication acknowledgement');
    assert(await button(alice, '确认发布给项目成员').isDisabled());
    await invokeStale(hiddenPreview);
    await hiddenPreview.dispose();
    const preview = await panel(alice).locator('.project-preview').innerText();
    for (const exact of [project.name, project.id, String(await revision()), '公开的分工候选', note, 'bob', '成员']) assert(preview.includes(exact), 'Preview includes ' + exact);
    assert(/(?:版本\s*1|v1)/.test(preview), 'New item version is explicit');
    assert(!preview.includes(secret) && !preview.includes(source));
    assert.equal(projectPosts.length, 0, 'Opening, editing, selecting, and previewing never POST');
    assert.deepEqual(await state(), []);
    assert.equal(await alice.evaluate(() => globalThis.projectXss), undefined);
    // Persist the actual candidate review UI for the existing CI artifact upload.
    for (const [size, viewport] of [['desktop', { width: 1440, height: 1000 }], ['mobile', { width: 390, height: 844 }]]) {
      await alice.setViewportSize(viewport);
      for (const colorScheme of ['light', 'dark']) {
        await alice.emulateMedia({ colorScheme });
        await panel(alice).locator('.project-preview').scrollIntoViewIfNeeded();
        assert(await panel(alice).locator('.project-preview').isVisible());
        assert(await alice.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'Candidate preview fits ' + size);
        await alice.screenshot({ path: path.join(screenshotDir, `project-chat-candidate-${size}-${colorScheme}.png`), animations: 'disabled' });
      }
    }
    await alice.setViewportSize({ width: 1440, height: 1000 });
    await alice.emulateMedia({ colorScheme: 'light' });
    const response = await commit(alice, '确认发布给项目成员', true);
    const body = response.request().postDataJSON();
    assert.equal(projectPosts.length, 1);
    assert(body.request_id);
    assert.equal(body.content.source_kind, 'human_statement');
    assert.equal(body.content.assignee, 'bob');
    assert.equal(body.content.content, '公开的分工候选');
    assert.equal(body.content.source_note, note);
    let items = await state();
    assert.equal(items.length, 1);
    assert.equal(items[0].version, 1);
    assert.equal(items[0].status, 'proposed');
    assert.equal(items[0].confirmed_by, null);
    for (const page of [bob, carol]) {
      await openProject(page, project.name);
      await panel(page).getByText('公开的分工候选', { exact: true }).waitFor();
      assert(!(await panel(page).innerText()).includes(secret));
      assert(!(await panel(page).innerText()).includes(source));
    }
    for (const who of ['bob', 'carol']) {
      assert(!JSON.stringify(await ok(who, 'messages')).includes(secret));
      assert(!JSON.stringify(await ok(who, 'messages')).includes(source));
    }
    console.log('PASS three real clients see only explicit edited publication, exact preview and proposed v1');

    // A completed assistant message always remains a model suggestion.
    await button(alice, '回到私聊').click();
    // Mock replies are notices, not eligible assistant messages. Replace one
    // read-only response text to exercise real assistant presentation while all
    // project writes, membership and assertions still use the real Hub.
    const messagesUrl = origin + '/api/messages';
    await alice.route(messagesUrl, async intercepted => {
      if (intercepted.request().method() !== 'GET') return intercepted.continue();
      const response = await intercepted.fetch(), messages = await response.json();
      const message = messages.findLast(entry => entry.role === 'assistant' && entry.status === 'complete');
      assert(message, 'Real Mock Hub must have completed the conversation');
      message.content = '助手测试建议：先整理候选，再由本人明确发布。';
      await intercepted.fulfill({ response, json: messages });
    });
    await alice.reload(); await alice.locator('#room:not(.hide)').waitFor();
    const assistantAction = alice.locator('.msg-assistant, .msg-notice').getByRole('button', { name: '提为项目候选', exact: true }).last();
    await assistantAction.click();
    assert.equal(await selection(alice).inputValue(), '');
    await selection(alice).selectOption(project.id);
    await button(alice, '整理候选').click();
    await field(alice, '内容').waitFor();
    assert.equal(await field(alice, '来源类型').inputValue(), 'model_suggestion');
    assert(await field(alice, '来源类型').isDisabled());
    await field(alice, '内容').fill('整理后的模型建议，不代表承诺');
    await button(alice, '预览发布').click();
    await commit(alice, '确认发布给项目成员');
    items = await state();
    const modelItem = items.find(item => item.content === '整理后的模型建议，不代表承诺');
    assert.equal(modelItem.source_kind, 'model_suggestion');
    assert.equal(modelItem.status, 'proposed');
    await alice.unroute(messagesUrl);
    console.log('PASS assistant provenance remains a suggestion');

    await button(alice, '回到私聊').click();
    await prepare(alice, '丢失响应仍是同一次候选发布');
    const lost = origin + '/api/' + route + '/state';
    const bodies = [];
    await alice.route(lost, async intercepted => {
      if (intercepted.request().method() !== 'POST') return intercepted.continue();
      bodies.push(intercepted.request().postDataJSON());
      if (bodies.length === 1) { await intercepted.fetch(); await intercepted.abort('failed'); }
      else await intercepted.continue();
    });
    await panel(alice).getByLabel('我已核对项目、共享文字和来源', { exact: true }).check();
    await button(alice, '确认发布给项目成员').click();
    await button(alice, '核对并重试原提交').waitFor();
    await pause(3500); // Polling sees the committed revision before the UI retry.
    await button(alice, '回到私聊').click();
    await alice.locator('#open-projects').click();
    await button(alice, '核对并重试原提交').waitFor();
    await commit(alice, '核对并重试原提交', true);
    assert.equal(bodies.length, 2);
    assert.deepEqual(bodies[1], bodies[0]);
    assert.equal((await state()).filter(item => item.content === '丢失响应仍是同一次候选发布').length, 1);
    await alice.unroute(lost);
    console.log('PASS lost POST response, polling, close/reopen and repeated exact retry do not duplicate');

    await button(alice, '回到私聊').click();
    await prepare(alice, '结果未知后取消不会撤回发布');
    let uncertainPosts = 0;
    await alice.route(lost, async intercepted => {
      if (intercepted.request().method() !== 'POST') return intercepted.continue();
      uncertainPosts++; await intercepted.fetch(); await intercepted.abort('failed');
    });
    await panel(alice).getByLabel('我已核对项目、共享文字和来源', { exact: true }).check();
    await button(alice, '确认发布给项目成员').click();
    await button(alice, '核对并重试原提交').waitFor();
    const declineCancel = alice.waitForEvent('dialog').then(async dialog => {
      assert(dialog.message().includes('不会撤回')); await dialog.dismiss();
    });
    await button(alice, '取消编辑').click(); await declineCancel;
    assert(await button(alice, '核对并重试原提交').isVisible());
    const acceptCancel = alice.waitForEvent('dialog').then(dialog => dialog.accept());
    await button(alice, '取消编辑').click(); await acceptCancel;
    await panel(alice).getByText('结果未知后取消不会撤回发布', { exact: true }).waitFor();
    assert.equal(uncertainPosts, 1);
    assert.equal(await button(alice, '核对并重试原提交').count(), 0);
    assert.equal((await state()).filter(item => item.content === '结果未知后取消不会撤回发布').length, 1);
    await alice.unroute(lost);
    console.log('PASS uncertain cancellation warns, can be declined, and never claims to undo publication');

    await button(alice, '回到私聊').click();
    await prepare(alice, '旧版本候选不能直接发布');
    await alice.evaluate(() => { refreshProjects = async () => {}; });
    await alice.waitForFunction(() => !projectRead);
    await ok('bob', route + '/state', 'POST', { request_id: randomUUID(), expected_revision: await revision(), content: {
      kind: 'goal', content: '其他成员发布新版本', source_kind: 'human_statement', source_note: '明确发布', assignee: null
    } });
    const conflict = alice.waitForResponse(r => r.url() === lost && r.status() === 409);
    await panel(alice).getByLabel('我已核对项目、共享文字和来源', { exact: true }).check();
    await button(alice, '确认发布给项目成员').click();
    await conflict;
    await panel(alice).getByText('项目已更新，请取消编辑后重新核对。', { exact: false }).waitFor();
    assert(await button(alice, '确认发布给项目成员').isDisabled());
    assert(!(await state()).some(item => item.content === '旧版本候选不能直接发布'));
    await button(alice, '取消编辑').click();
    console.log('PASS stale project revision cannot silently publish');

    // Membership is checked on the server even if a stale preview remains open.
    await ok('bob', 'messages', 'POST', { message: source, request_id: randomUUID() });
    await bob.reload(); await bob.locator('#room:not(.hide)').waitFor();
    await promote(bob).click(); await selection(bob).selectOption(project.id);
    await button(bob, '整理候选').click(); await field(bob, '内容').waitFor();
    await field(bob, '内容').fill('退出后不能发布'); await button(bob, '预览发布').click();
    await bob.evaluate(() => { refreshProjects = async () => {}; }); await bob.waitForFunction(() => !projectRead);
    await ok('bob', route + '/membership', 'POST', { action: 'leave', expected_revision: await revision() });
    const denied = bob.waitForResponse(r => r.url() === lost && [403, 404].includes(r.status()));
    await panel(bob).getByLabel('我已核对项目、共享文字和来源', { exact: true }).check();
    await button(bob, '确认发布给项目成员').click(); await denied;
    assert(!(await state()).some(item => item.content === '退出后不能发布'));
    console.log('PASS revoked membership prevents publication');

    // Account switching must destroy private candidates and their source text.
    await button(alice, '回到私聊').click();
    await prepare(alice, '切换账号必须丢弃此候选');
    await review(alice).check();
    const priorIdentityPreview = await button(alice, '确认发布给项目成员').elementHandle();
    await alice.locator('#logout').click(); await alice.locator('#token').waitFor();
    await invokeStale(priorIdentityPreview);
    // Sign in without navigation to retain and exercise the old DOM closure.
    await alice.locator('#token').fill(tokens.eve); await alice.locator('#enter').click();
    await alice.locator('#room:not(.hide)').waitFor();
    await invokeStale(priorIdentityPreview); await priorIdentityPreview.dispose();
    await openProjects(alice);
    assert.equal(await button(alice, '确认发布给项目成员').count(), 0);
    assert.equal(await selection(alice).count(), 0);
    assert(!(await panel(alice).innerText()).includes(source));
    assert.deepEqual(await ok('eve', 'projects'), []);
    assert.equal(await alice.evaluate(() => globalThis.projectXss), undefined);
    assert.deepEqual(pageErrors, []);
    console.log('PASS account switch clears private candidate; HTML source never executes');
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
