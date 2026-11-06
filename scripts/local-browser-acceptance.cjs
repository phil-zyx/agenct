/* Real local Hub + two isolated Chromium contexts. Mock model only; no remote
 * provider, shared-host, physical-device or screen-reader acceptance is claimed.
 * Build first: cargo build --locked --manifest-path hub/Cargo.toml
 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const { spawn, execFileSync } = require('node:child_process');
const { chromium } = require('playwright');
const root = path.resolve(__dirname, '..');
const binary = process.env.AGENCT_TEST_HUB_BINARY || path.join(root, 'hub/target/debug/agenct-hub');
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
async function freePort() {
  const socket = net.createServer(); await new Promise(resolve => socket.listen(0, '127.0.0.1', resolve));
  const port = socket.address().port; await new Promise(resolve => socket.close(resolve)); return port;
}
(async () => {
  try { execFileSync('pdftotext', ['-v'], { stdio: 'ignore' }); } catch { throw Error('Real PDF acceptance requires poppler-utils (pdftotext).'); }
  assert(fs.existsSync(binary), 'Build the Hub first: cargo build --locked --manifest-path hub/Cargo.toml');
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-browser-acceptance-'));
  const state = path.join(temporary, 'state'), backup = path.join(temporary, 'backup.sqlite3');
  const repositoryGate = process.env.AGENCT_TEST_REPOSITORY_GATE;
  let repositoryFixture = null, repositoryConfig = null;
  if (repositoryGate) {
    assert(fs.existsSync(repositoryGate), 'Configured real repository boundary gate does not exist');
    repositoryFixture = JSON.parse(fs.readFileSync(path.join(root, 'hub/tests/fixtures/readonly-objects.json'), 'utf8'));
    const objects = path.join(temporary, 'objects'); fs.mkdirSync(objects, { mode: 0o700 });
    for (const [id, bytes] of Object.entries(repositoryFixture.objects)) {
      const directory = path.join(objects, id.slice(0, 2)); fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
      fs.writeFileSync(path.join(directory, id.slice(2)), Buffer.from(bytes, 'base64'), { mode: 0o600 });
    }
    repositoryConfig = path.join(temporary, 'repositories.json');
    fs.writeFileSync(repositoryConfig, JSON.stringify([{id:'local-fixture',label:'Local verified repository',owners:['alice'],objects_dir:objects}]), {mode:0o600});
  }
  let processHub = null, browser = null;
  const interrupt = () => { processHub?.kill('SIGINT'); void browser?.close().catch(() => {}); };
  process.once('SIGINT', interrupt); process.once('SIGTERM', interrupt);
  const port = await freePort(), origin = `http://127.0.0.1:${port}`;
  const staging = (...args) => execFileSync('python3', [path.join(root, 'scripts/private-staging.py'), ...args, '--state-dir', state], { cwd: root, stdio: ['ignore', 'pipe', 'pipe'] });
  const credentials = () => JSON.parse(fs.readFileSync(path.join(state, 'credentials.json'), 'utf8'));
  async function start() {
    const configPath = path.join(state, 'config.json');
    const config = JSON.parse(fs.readFileSync(configPath, 'utf8'));
    // Deliberately omit inherited provider credentials, adapters and OIDC config.
    processHub = spawn(binary, [], { cwd: root, stdio: ['ignore', 'ignore', 'pipe'], env: {
      PATH: process.env.PATH, HOME: process.env.HOME, LANG: process.env.LANG || 'C.UTF-8',
      AGENCT_BIND: `127.0.0.1:${port}`, AGENCT_DATABASE: path.join(state, 'hub.sqlite3'),
      AGENCT_RUNTIME: 'mock', AGENCT_USERS_JSON: JSON.stringify(config.users), AGENCT_PRIVATE_CONFIG: configPath,
      ...(repositoryConfig ? { AGENCT_REPOSITORIES_FILE: repositoryConfig, AGENCT_REPOSITORY_GATE_FILE: path.resolve(repositoryGate) } : {})
    } });
    // Drain logs without publishing request data or credentials.
    processHub.stderr.on('data', () => {});
    let startupError; processHub.once('error', error => { startupError = error; });
    for (let i = 0; i < 200; i++) {
      if (startupError) throw startupError;
      if (processHub.exitCode !== null) throw Error('Temporary Hub exited during startup');
      try { if ((await fetch(origin)).ok) return; } catch {}
      await wait(50);
    }
    throw Error('Temporary Hub did not become ready');
  }
  async function stop() {
    if (!processHub || processHub.exitCode !== null) { processHub = null; return; }
    const child = processHub;
    await new Promise(resolve => {
      const timeout = setTimeout(() => child.kill('SIGKILL'), 5000);
      child.once('exit', () => { clearTimeout(timeout); resolve(); }); child.kill('SIGINT');
    }); processHub = null;
  }
  async function login(page, identity) {
    await page.bringToFront();
    await page.goto(origin); await page.locator('#token').fill(credentials()[identity]); await page.locator('#enter').click();
    await page.locator('#room:not(.hide)').waitFor();
  }
  async function activate(page, name) {
    const control = page.getByRole('button', { name, exact: true });
    if (process.env.AGENCT_TEST_HEADED === '1') {
      await control.focus(); await page.waitForTimeout(700); await page.keyboard.press('Enter');
    } else await control.click();
  }
  async function tasks(page) {
    if (await page.locator('#toggle-tasks').getAttribute('aria-pressed') !== 'true') await page.locator('#toggle-tasks').click();
    await page.locator('#tab-tasks').click();
  }
  async function api(identity, route, overrideToken) {
    return fetch(origin + '/api/' + route, { headers: { Authorization: 'Bearer ' + (overrideToken || credentials()[identity]) } });
  }
  try {
    staging('init', '--users', 'alice', 'bob', '--port', String(port));
    await start();
    browser = await chromium.launch({ headless: process.env.AGENCT_TEST_HEADED !== '1',
      ...(process.env.AGENCT_TEST_HEADED === '1' ? {args:['--force-renderer-accessibility'],slowMo:150} : {}),
      ...(process.env.CHROMIUM_EXECUTABLE_PATH ? { executablePath: process.env.CHROMIUM_EXECUTABLE_PATH } : {}) });
    const aliceContext = await browser.newContext(), bobContext = await browser.newContext();
    const alice = await aliceContext.newPage(), bob = await bobContext.newPage();
    const errors = []; for (const page of [alice, bob]) page.on('pageerror', error => errors.push(error.message));
    await login(alice, 'alice');
    for (let step = 0; step < 3; step++) await alice.locator('#onboarding-next').click();
    await login(bob, 'bob'); await bob.locator('#onboarding-skip').click();
    await alice.bringToFront();
    const sentMessages = []; alice.on('request', request => { if (request.url() === origin + '/api/messages' && request.method() === 'POST') sentMessages.push(request.postDataJSON()); });
    if (repositoryFixture) {
      await alice.locator('#composer-menu-toggle').click(); await alice.locator('#choose-repository').click();
      await alice.getByLabel('仓库提交 ID', {exact:true}).fill(repositoryFixture.commit);
      await alice.getByLabel('仓库文件路径或搜索文字', {exact:true}).fill('notes.txt');
      await alice.getByRole('button', {name:'预览仓库内容',exact:true}).click();
      await alice.locator('.repository-preview:not(.hide)').waitFor();
      if (process.env.AGENCT_TEST_SCREENSHOT_DIR) {
        fs.mkdirSync(process.env.AGENCT_TEST_SCREENSHOT_DIR, {recursive:true});
        await alice.screenshot({path:path.join(process.env.AGENCT_TEST_SCREENSHOT_DIR,'repository-preview.png')});
        await alice.setViewportSize({width:400,height:760}); await alice.locator('#open-chat').click();
        await alice.getByRole('button',{name:'确认用于下一条聊天',exact:true}).scrollIntoViewIfNeeded();
        assert(await alice.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),'Repository picker must fit a phone viewport');
        await alice.screenshot({path:path.join(process.env.AGENCT_TEST_SCREENSHOT_DIR,'repository-preview-mobile.png')});
        await alice.setViewportSize({width:1280,height:720});
      }
      assert.equal(sentMessages.length,0,'Repository preview never dispatches a model message');
      await alice.getByRole('button',{name:'确认用于下一条聊天',exact:true}).click();
      assert.equal((await (await api('bob','repositories')).json()).length,0,'Repository listing remains owner scoped');
    }
    await alice.locator('#text').fill('你好，请概括协作流程'); await alice.locator('#send').click();
    await alice.locator('.msg-assistant, .msg-notice').waitFor();
    if (repositoryFixture) {
      assert(sentMessages[0].message.startsWith('/repo local-fixture ' + repositoryFixture.commit + ' notes.txt\n'));
      await alice.locator('#repository-context.hide').waitFor({state:'attached'});
      console.log('PASS real gated repository preview, explicit single-chat attachment, matching snapshot receipt and owner isolation');
    }
    await alice.locator('#text').fill('请让 bob 帮我 检查本地验收文档'); await alice.locator('#send').click();
    const proposal = alice.locator('.proposal:not(.settled)'); await proposal.waitFor();
    await proposal.getByRole('button', { name: '＋ 附带共享材料（可选）', exact: true }).click();
    let material = '# 本地验收材料\n只共享这份正文，不共享完整私聊。';
    await proposal.locator('input[type=file]').setInputFiles({ name: 'first.txt', mimeType: 'text/plain', buffer: Buffer.from('预览文本导入，不共享。') });
    await alice.waitForFunction(() => document.querySelector('.proposal:not(.settled) textarea')?.value === '预览文本导入，不共享。');
    alice.once('dialog', dialog => dialog.accept());
    await proposal.locator('input[type=file]').setInputFiles({ name: 'acceptance.md', mimeType: 'text/markdown', buffer: Buffer.from(material) });
    await alice.waitForFunction(value => document.querySelector('.proposal:not(.settled) textarea')?.value === value, material);
    assert.equal(await proposal.locator('textarea').inputValue(), material);
    const pdfBytes = Buffer.from(execFileSync('python3', ['-c', 'import sys,base64;sys.path.insert(0,"scripts");from test_extract_pdf import document;print(base64.b64encode(document("Local PDF acceptance material")).decode())'], {cwd:root}).toString().trim(), 'base64');
    const approvePdf = dialog => dialog.accept(); alice.on('dialog', approvePdf);
    await proposal.locator('input[type=file]').setInputFiles({ name: 'acceptance.pdf', mimeType: 'application/pdf', buffer: pdfBytes });
    material = 'Local PDF acceptance material';
    await alice.waitForFunction(value => document.querySelector('.proposal:not(.settled) textarea')?.value.trim() === value, material);
    material = await proposal.locator('textarea').inputValue(); alice.removeListener('dialog', approvePdf);

    assert.equal((await (await api('bob', 'tasks')).json()).length, 0, 'Import alone must not share');
    await proposal.getByRole('button', { name: '确认共享并委派', exact: true }).click();
    await bob.bringToFront();
    await bob.locator('#refresh').click(); await tasks(bob);
    await activate(bob, '接手');
    await bob.locator('.task-working').waitFor();
    await bob.getByRole('button', { name: '我的私密交付草稿', exact: true }).click();
    await bob.getByRole('button', { name: '让我的 Agent 起草（不交付）', exact: true }).click();
    await bob.getByLabel('私密交付草稿编辑', { exact: true }).waitFor();
    await bob.getByLabel('草稿修改要求', { exact: true }).fill('请缩短并保留验收清单');
    await bob.getByRole('button', { name: '按要求生成新版本', exact: true }).click();
    await bob.waitForFunction(() => document.querySelectorAll('textarea[aria-label="私密交付草稿编辑"]').length >= 2);
    const privateDraft = await (await api('alice', 'tasks')).json();
    assert.equal(privateDraft[0].result, null, 'Private drafting must not publish a delivery');
    await bob.getByLabel('私密交付草稿编辑', { exact: true }).last().fill('人工审阅后的交付：已检查共享文档，建议补充操作步骤。');
    await bob.getByRole('button', { name: '使用此草稿填入人工交付', exact: true }).last().click();
    await bob.getByRole('button', { name: '提交人工交付', exact: true }).click();
    await bob.locator('.action-editor').getByRole('button', { name: '预览', exact: true }).click();
    await activate(bob, '确认交付');
    await bob.locator('.task-ready_for_acceptance').waitFor();
    await alice.bringToFront();
    await alice.locator('#refresh').click(); await tasks(alice);
    await activate(alice, '验收');
    await alice.locator('.task-completed').waitFor({ state: 'attached' });
    await alice.locator('#task-filter').selectOption('completed');
    await alice.locator('.task-completed').waitFor();
    assert((await alice.locator('.task-completed').textContent()).includes('人工审阅后的交付'));
    const adoptedSource = execFileSync('python3', ['-c', 'import sqlite3,sys;db=sqlite3.connect(sys.argv[1]);print(db.execute("SELECT source_draft_id FROM deliveries WHERE status=? ORDER BY seq DESC LIMIT 1",("submitted",)).fetchone()[0] or "")', path.join(state,'hub.sqlite3')]).toString().trim();
    assert(adoptedSource, 'Reviewed and edited private draft must record its source for adoption metrics');
    console.log('PASS real local Hub / two Chromium identities: chat, text/Markdown/PDF import, delegation, draft revision, reviewed delivery, approval, archive (mock model)');

    const oldToken = credentials().bob;
    staging('add-user', '--user', 'charlie');
    assert.equal((await api('charlie', 'me')).status, 200);
    staging('rotate-token', '--user', 'bob');
    assert.equal((await api('bob', 'me', oldToken)).status, 401);
    assert.equal((await api('bob', 'me')).status, 200);
    staging('backup', '--backup-file', backup);
    await stop();
    for (const suffix of ['', '-wal', '-shm']) fs.rmSync(path.join(state, 'hub.sqlite3' + suffix), { force: true });
    staging('restore', '--backup-file', backup);
    await start();
    await login(alice, 'alice'); await tasks(alice); await alice.locator('#task-filter').selectOption('completed');
    await alice.locator('.task-completed').waitFor();
    const persisted = await (await api('bob', 'tasks')).json();
    assert.equal(persisted[0].status, 'completed'); assert.equal(persisted[0].details, material);
    assert.equal(Object.hasOwn(persisted[0], 'source_draft_id'), false, 'Private draft provenance must not appear in public task data');
    const drafts = await (await api('bob', 'tasks/' + persisted[0].id + '/drafts?limit=20')).json();
    assert(drafts.items.length >= 2, 'Draft versions persist through backup and restore');
    assert.deepEqual(errors, []);
    console.log('PASS real Hub online membership/token revocation and consistent backup → database deletion → restore → restart persistence');
  } finally {
    process.removeListener('SIGINT', interrupt); process.removeListener('SIGTERM', interrupt);
    if (browser) await browser.close(); await stop(); fs.rmSync(temporary, { recursive: true, force: true });
  }
})().catch(error => { console.error(error.message); process.exitCode = 1; });
