/* Three isolated browsers against a real, temporary Hub. All model calls use Mock.
 * Build first: cargo build --locked --manifest-path hub/Cargo.toml
 * Run: node hub/web/website-collaboration.browser.test.cjs
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
const screenshotDir = process.env.AGENCT_TEST_SCREENSHOT_DIR || fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-website-screens-'));

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


    const name = '网站协作完整验收（Mock）';
    const project = await ok('alice', 'projects', 'POST', { name, goal: '先澄清，再确认分工，最后验收；不部署', kind: 'temporary', request_id: randomUUID() });
    const route = 'projects/' + project.id;
    const revision = async () => (await ok('alice', route)).project.revision;
    const state = async () => (await ok('alice', route + '/state')).items;
    for (const who of ['bob', 'carol']) {
      await ok('alice', route + '/members', 'POST', { owner: who, expected_revision: await revision() });
      await ok(who, route + '/membership', 'POST', { action: 'accept', expected_revision: await revision() });
    }
    const secret = 'PRIVATE-CAROL-' + randomUUID();
    await ok('carol', 'messages', 'POST', { message: secret, request_id: randomUUID() });
    async function publishState(who, kind, content, assignee = null) {
      const page = userPages[who];
      await reloadProject(page, name);
      await button(page, '发布共同状态').click();
      await panel(page).getByLabel('类型', { exact: true }).selectOption(kind);
      await panel(page).getByLabel('内容', { exact: true }).fill(content);
      await panel(page).getByLabel('来源说明', { exact: true }).fill('本人核对后明确发布；Mock 场景，不是模型能力证明');
      if (assignee) await panel(page).getByLabel('负责人', { exact: true }).selectOption(assignee);
      const before = await state();
      await button(page, '预览发布').click();
      assert.deepEqual(await state(), before, 'Preview cannot publish');
      await commit(page, '确认发布给项目成员', true);
      const after = await state();
      assert.equal(after.length, before.length + 1, 'Repeated publish click creates once');
      return after.find(item => !before.some(old => old.id === item.id));
    }
    async function confirm(who, item, label) {
      const page = userPages[who]; await reloadProject(page, name);
      await panel(page).locator(`[data-item="${item.id}"]`).getByRole('button', { name: label, exact: true }).click();
      await commit(page, '确认当前版本', true);
      return (await state()).find(i => i.id === item.id);
    }
    async function feedback(who, item, text, reply = null) {
      const page = userPages[who]; await reloadProject(page, name);
      await panel(page).locator(`[data-item="${item.id}"]`).getByRole('button', { name: '共享问答', exact: true }).click();
      await button(page, '提出共享问题').waitFor();
      if (reply) await page.locator(`[data-question="${reply}"]`).getByRole('button', { name: '回复此问题', exact: true }).click();
      else await button(page, '提出共享问题').click();
      await page.getByLabel(reply ? '共享回复内容' : '共享问题内容', { exact: true }).fill(text);
      await button(page, '预览共享反馈').click();
      assert(await button(page, '确认共享反馈').isDisabled());
      await page.getByLabel('我已核对项目、问题版本和共享文字', { exact: true }).check();
      const response = page.waitForResponse(r => r.url().includes('/questions') && r.request().method() === 'POST');
      await button(page, '确认共享反馈').evaluate(b => { b.click(); b.click(); });
      const result = await response; assert.equal(result.status(), 200);
      await panel(page).locator('.project-editor').waitFor({ state: 'detached' });
      return (await result.json()).message;
    }
    async function discuss(who, question) {
      const page = userPages[who]; await reloadProject(page, name);
      const before = await state(), others = {};
      const tasksBefore = await ok(who, 'tasks');
      for (const other of ['alice', 'bob', 'carol'].filter(x => x !== who)) others[other] = await ok(other, 'messages');
      await button(page, '与我的 Agent 讨论').click();
      await page.locator('#project-context:not(.hide)').waitFor();
      await page.locator('#text').fill(question);
      const response = page.waitForResponse(r => r.url() === origin + '/api/messages' && r.request().method() === 'POST');
      await page.locator('#send').click();
      const result = await response; assert.equal(result.status(), 200);
      await page.waitForFunction(() => !pendingTurn && !busy.has('send'));
      const value = await result.json();
      assert(value.reply.includes('MOCK'), 'Never represent mock as real Agent reasoning');
      assert(!JSON.stringify(value.project_context).includes(secret));
      assert.deepEqual(await state(), before, 'Agent discussion never publishes or confirms');
      assert.deepEqual(await ok(who, 'tasks'), tasksBefore, 'Discussion cannot create or execute tasks');
      for (const [other, history] of Object.entries(others)) assert.deepEqual(await ok(other, 'messages'), history);
      return value;
    }
    let goal = await publishState('alice', 'goal', '制作三页作品网站：主页、作品、联系；支持手机，不含支付和上线部署');
    const scopeQuestion = await feedback('bob', goal, '移动端和联系表单怎么验收？');
    const scopeReply = await feedback('alice', goal, '390px 无横向滚动；联系入口只展示邮箱，不发送表单。', scopeQuestion.id);
    await feedback('carol', goal, '我可以检查三页文案与移动布局；这还不是认领分工。', scopeQuestion.id);
    for (const who of ['bob', 'carol']) {
      const discussion = await discuss(who, '根据已经共享的范围问答，给我私下建议；不要代我承诺或发送。');
      assert(discussion.project_context.feedback.some(message => message.id === scopeReply.id && message.content === scopeReply.content && message.author === 'alice' && message.current_reference), 'Agent sees explicitly shared versioned clarification');
    }
    goal = await confirm('alice', goal, '确认这项目标');
    const decision = await publishState('alice', 'decision', '首轮只交付静态页面和验收清单，不配置支付、模型或部署');
    await confirm('alice', decision, '确认这项决定');
    const build = await publishState('bob', 'assignment', 'Bob 准备网站实现交付清单', 'bob');
    const review = await publishState('carol', 'assignment', 'Carol 核对内容与 390px 布局验收要求', 'carol');
    assert.equal((await api('alice', route + '/state/' + build.id + '/actions', 'POST', { action: 'confirm', expected_revision: await revision(), expected_version: build.version })).status, 404);
    await confirm('bob', build, '确认这项分工'); await confirm('carol', review, '确认这项分工');
    let blocker = await publishState('bob', 'blocker', '缺少最终联系邮箱，先暂停该页定稿');
    blocker = await confirm('bob', blocker, '确认这个阻塞');
    const query = await feedback('bob', blocker, '请 Alice 确认占位邮箱是否可用于本轮 Mock 验收');
    await feedback('alice', blocker, '本轮使用 hello@example.test 占位，不发送邮件。', query.id);
    const resolved = await discuss('bob', '根据共享回复说明如何收尾；不要把回复当作分工确认。');
    assert(JSON.stringify(resolved.project_context).includes('hello@example.test'));
    const completion = () => ok('alice', route + '/completion');
    assert.equal((await completion()).ready, false, 'Confirming blocker is not resolving it');
    assert.equal((await api('alice', route + '/state/' + blocker.id + '/resolution', 'POST', { request_id: randomUUID(), expected_revision: await revision(), expected_version: blocker.version, expected_snapshot: (await completion()).snapshot_token, action: 'resolve', note: 'Cannot resolve for Bob' })).status, 404);
    async function declaration(who, action, field, note, confirmLabel, cancel = false) {
      const page = userPages[who]; await reloadProject(page, name);
      await button(page, action).click();
      if (field) { await page.getByLabel(field, { exact: true }).fill(note); await button(page, '预览声明').click(); }
      assert(await button(page, confirmLabel).isDisabled(), 'Explicit acknowledgement required');
      const before = await completion();
      if (cancel) { await button(page, '取消编辑').click(); assert.deepEqual(await completion(), before); return; }
      await page.getByLabel('我已核对版本、共享范围和本人声明', { exact: true }).check();
      const response = page.waitForResponse(r => r.request().method() === 'POST' && r.url().includes('/api/projects'));
      await button(page, confirmLabel).evaluate(b => { b.click(); b.click(); });
      const result = await response; assert.equal(result.status(), 200, confirmLabel);
      await page.waitForFunction(() => !projectBusy && !projectDraft);
      return { value: await result.json(), body: result.request().postDataJSON(), url: result.url() };
    }
    await declaration('bob', '解除阻塞', '阻塞解除说明', '占位邮箱由 Alice 明确提供，仅用于 Mock 清单。', '确认解除阻塞', true);
    assert.equal((await completion()).blockers[0].resolved, false, 'Cancel leaves blocker unresolved');
    const resolution = await declaration('bob', '解除阻塞', '阻塞解除说明', '占位邮箱由 Alice 明确提供，仅用于 Mock 清单。', '确认解除阻塞');
    assert.equal((await completion()).blockers[0].resolved, true);
    const resolutionRevision = await revision();
    await ok('bob', route + '/state/' + blocker.id + '/resolution', 'POST', resolution.body);
    assert.equal(await revision(), resolutionRevision, 'Resolution replay is idempotent');
    console.log('PASS website clarification → private Mock suggestions → named assignments → explicit blocker resolution');

    const bobProfile = await ok('bob', 'collaboration-profile');
    await ok('bob', 'collaboration-profile', 'PUT', { expected_revision: bobProfile.revision, public: bobProfile.public, private: bobProfile.private,
      permissions: { auto_reply_public: false, auto_prepare_drafts: true } });
    const privateTask = 'PRIVATE-WEBSITE-DELIVERY-' + randomUUID();
    const proposal = await ok('alice', 'proposals', 'POST', { owner: 'bob', summary: privateTask, request_id: randomUUID() });
    assert.deepEqual(await ok('bob', 'tasks'), [], 'Private proposal is not delegation');
    const { task_id: taskId } = await ok('alice', 'proposals/' + proposal.id + '/actions', 'POST', { action: 'confirm', expected_revision: 0, details: '只准备本轮网站验收清单，不运行代码和部署' });
    await ok('alice', route + '/tasks', 'POST', { task_id: taskId, label: '网站交付验收清单', expected_revision: await revision() });
    assert.deepEqual((await ok('carol', route)).tasks, []);
    await reloadProject(bob, name);
    await panel(bob).locator(`[data-task="${taskId}"]`).getByRole('button', { name: '同意公开关联', exact: true }).click();
    await commit(bob, '确认公开关联', true);
    async function refresh(page) { await page.locator('#refresh').evaluate(b => b.click()); }
    async function say(page, text) {
      await page.locator('#open-chat').click();
      await page.waitForFunction(() => !document.getElementById('task-context').classList.contains('hide'));
      await page.locator('#text').fill(text); await page.locator('#text').press('Enter');
    }
    await button(bob, '回到私聊').click(); await refresh(bob);
    await say(bob, '我接手');
    await bob.locator('.task-chat').last().getByRole('button', { name: '确认接手', exact: true }).click();
    await bob.locator('.task-chat').last().locator('.agent-draft').waitFor({ timeout: 15000 });
    assert.equal((await ok('alice', 'tasks'))[0].result, null);
    assert.equal((await api('carol', 'tasks/' + taskId)).status, 404);
    assert.equal((await api('carol', 'tasks/' + taskId + '/drafts')).status, 404);
    await say(bob, '可以了，交付');
    const taskCard = bob.locator('.task-chat').last();
    await taskCard.getByRole('button', { name: '先改一下再交付', exact: true }).click();
    await taskCard.getByLabel('交付内容', { exact: true }).fill('Mock 网站验收清单：三页；390px 无横向滚动；hello@example.test；无支付、表单发送和部署。');
    await taskCard.getByRole('button', { name: '确认交付', exact: true }).click();
    await bob.locator('.task-line', { hasText: '等待验收' }).waitFor();
    await button(alice, '回到私聊').click(); await refresh(alice);
    await say(alice, '验收通过');
    await alice.locator('.task-chat', { hasText: '交付了结果' }).getByRole('button', { name: '确认验收', exact: true }).click();
    await alice.locator('.task-line', { hasText: '已验收完成' }).waitFor();
    assert.equal((await ok('alice', 'tasks/' + taskId)).task.status, 'completed');
    await reloadProject(carol, name);
    const publicProject = await ok('carol', route);
    assert(!JSON.stringify(publicProject).includes(privateTask));
    assert(!(await panel(carol).innerText()).includes(privateTask));
    assert.equal(publicProject.tasks[0].can_open_task, false);
    assert.equal((await completion()).acceptance, null, 'Task acceptance is not project final acceptance');
    assert.equal((await completion()).ready, true);
    assert.equal(await button(carol, '验收项目').count(), 0, 'Only creator can finally accept');
    await declaration('alice', '验收项目', '项目验收说明', '核对本轮 Mock 网站清单，不是实际代码或上线交付。', '确认项目最终验收', true);
    assert.equal((await completion()).acceptance, null, 'Preview and cancel do not complete project');
    const accepted = await declaration('alice', '验收项目', '项目验收说明', '核对本轮 Mock 网站清单，不是实际代码或上线交付。', '确认项目最终验收');
    let acceptedState = await completion();
    assert.equal(acceptedState.acceptance.valid, true);
    assert.deepEqual(acceptedState.acceptance.goals, [{ item_id: goal.id, item_version: goal.version }]);
    assert.equal((await ok('alice', route)).project.status, 'active', 'Acceptance does not archive');
    assert.equal((await api('carol', route + '/completion', 'POST', { ...accepted.body, request_id: randomUUID() })).status, 404);
    const acceptedRevision = await revision();
    assert.equal((await ok('alice', route + '/completion', 'POST', accepted.body)).acceptance.valid, true);
    assert.equal(await revision(), acceptedRevision, 'Acceptance replay creates no duplicate');
    assert(!JSON.stringify(await ok('carol', route + '/completion')).includes(privateTask));
    await reloadProject(carol, name);
    await panel(carol).getByText('当前项目版本已由 alice 最终验收', { exact: true }).waitFor();
    await carol.setViewportSize({ width: 390, height: 844 });
    assert(await carol.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1));
    await carol.screenshot({ path: path.join(screenshotDir, 'website-collaboration-mobile.png') });
    await alice.screenshot({ path: path.join(screenshotDir, 'website-delivery-completed.png') });
    await declaration('bob', '重新打开阻塞', '重新打开说明', '占位邮箱仍需下一轮最终核对。', '确认重新打开阻塞');
    assert.equal((await completion()).acceptance.valid, false, 'Reopened blocker invalidates acceptance');
    assert.equal((await ok('alice', route + '/completion', 'POST', accepted.body)).acceptance.valid, false, 'Old replay cannot revive completion');
    await declaration('bob', '解除阻塞', '阻塞解除说明', '再次核对本轮仅使用占位信息。', '确认解除阻塞');
    assert.equal((await completion()).acceptance.valid, false, 'Re-resolving is not implicit re-acceptance');
    await declaration('alice', '验收项目', '项目验收说明', '针对当前目标版本重新核对 Mock 清单。', '确认项目最终验收');
    await declaration('alice', '撤回项目验收', null, null, '确认撤回项目验收');
    assert.equal((await completion()).acceptance.valid, false);
    assert.equal((await completion()).acceptance.withdrawn, true);
    const finalAccepted = await declaration('alice', '验收项目', '项目验收说明', '再次明确接受本轮当前版本，仅是本人声明。', '确认项目最终验收');
    const oldRevision = await revision();
    await ok('carol', route + '/membership', 'POST', { action: 'leave', expected_revision: oldRevision });
    assert.equal((await api('carol', 'messages', 'POST', { message: '旧项目讨论', request_id: randomUUID(), selected_project: { project_id: project.id, expected_revision: oldRevision } })).status, 404);
    assert.equal((await api('alice', 'messages', 'POST', { message: '旧修订讨论', request_id: randomUUID(), selected_project: { project_id: project.id, expected_revision: oldRevision } })).status, 409);
    assert.equal((await api('carol', route + '/completion')).status, 404);
    assert.equal((await completion()).acceptance.valid, false, 'Membership change invalidates acceptance');
    assert.equal((await ok('alice', route + '/completion', 'POST', finalAccepted.body)).acceptance.valid, false);
    assert.deepEqual(pageErrors, []);
    console.log('PASS human delivery/acceptance, reload persistence, private task ACL, revoked/stale contexts; mock only, no site built or deployed');
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
