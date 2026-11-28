// Self-contained E2E smoke test: real Rust Hub + mock GitLab OAuth + real Python Node + real Chromium + backup/restore.
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { chromium, expect } from '@playwright/test';

const ROOT = process.cwd();
const HUB_BIN = path.join(ROOT, 'hub/target/debug/agenct-hub');

function freePort() {
  return new Promise((resolve, reject) => {
    const s = http.createServer();
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close((err) => (err ? reject(err) : resolve(port)));
    });
  });
}

function startMockGitLab(port) {
  const srv = http.createServer((req, res) => {
    const u = new URL(req.url, `http://127.0.0.1:${port}`);
    if (u.pathname === '/oauth/authorize') {
      const redir = u.searchParams.get('redirect_uri');
      const state = u.searchParams.get('state');
      res.writeHead(302, { Location: `${redir}?code=e2e-code&state=${encodeURIComponent(state)}` });
      res.end();
      return;
    }
    if (u.pathname === '/oauth/token' && req.method === 'POST') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ access_token: 'mock-gl-token' }));
      return;
    }
    if (u.pathname === '/api/v4/user') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ id: 7, username: 'alice', name: 'Alice', state: 'active' }));
      return;
    }
    res.writeHead(404);
    res.end();
  });
  return new Promise((resolve) => srv.listen(port, '127.0.0.1', () => resolve(srv)));
}

async function waitHealth(url) {
  for (let i = 0; i < 40; i++) {
    try {
      const r = await fetch(`${url}/health`);
      if (r.ok) return;
    } catch {
      // retry
    }
    await new Promise((r) => setTimeout(r, 150));
  }
  throw new Error('Hub failed to become healthy');
}

const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-v2-e2e-'));
const dataDir = path.join(tmpDir, 'data');
const workDir = path.join(tmpDir, 'work');
fs.mkdirSync(workDir, { recursive: true });

const glPort = await freePort();
const hubPort = await freePort();
const hubOrigin = `http://127.0.0.1:${hubPort}`;
const glServer = await startMockGitLab(glPort);

const hub = spawn(HUB_BIN, [], {
  cwd: ROOT,
  env: {
    ...process.env,
    AGENCT_BIND: `127.0.0.1:${hubPort}`,
    AGENCT_DATA_DIR: dataDir,
    AGENCT_PUBLIC_ORIGIN: hubOrigin,
    AGENCT_ALLOW_HTTP_ORIGIN: '1',
    AGENCT_GITLAB_URL: `http://127.0.0.1:${glPort}`,
    AGENCT_GITLAB_CLIENT_ID: 'e2e-client',
    AGENCT_GITLAB_CLIENT_SECRET: 'e2e-secret',
    AGENCT_MASTER_KEY: 'e2e-test-master-key-0123456789abcdef012345',
    AGENCT_LLM_STUB: '1',
  },
  stdio: ['ignore', 'pipe', 'pipe'],
});

let browser;
try {
  await waitHealth(hubOrigin);
  browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  const errors = [];
  page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
  page.on('console', (m) => {
    if (m.type() === 'error') errors.push(`console.error: ${m.text()}`);
  });

  // 1. Open login page and click GitLab login -> completes PKCE OAuth redirect.
  await page.goto(hubOrigin, { waitUntil: 'networkidle' });
  await page.waitForSelector('#gitlab-login-btn:not(.hide)');
  await Promise.all([page.waitForURL(`${hubOrigin}/`), page.click('#gitlab-login-btn')]);

  // 2. Verify authenticated workspace, floating avatar header, and live SSE connection.
  await page.waitForSelector('#room:not(.hide)');
  await page.waitForFunction(() => document.getElementById('sse-pill')?.textContent?.includes('实时同步'));

  // 2a. Empty-state starter cards, hanging status tab, caret `/` suggest menu, and manual theme toggle (strict CSP safe).
  await page.waitForSelector('#composer-status-tab #status-ctx-label');
  await page.waitForSelector('#chat-starters:not(.hide) .starter-card');
  await page.focus('#chat-input');
  await page.fill('#chat-input', '/');
  await page.waitForSelector('#composer-suggest:not(.hide) .suggest-item');
  await page.keyboard.press('Escape');
  await page.waitForSelector('#composer-suggest.hide', { state: 'attached' });
  await page.fill('#chat-input', '');

  await page.click('#rail-more');
  await page.click('#toggle-theme');
  await page.waitForFunction(() => ['dark', 'light'].includes(document.documentElement.dataset.theme || ''));
  const pickedTheme = await page.evaluate(() => document.documentElement.dataset.theme);
  assert.equal(await page.evaluate(() => localStorage.getItem('agenct:theme')), pickedTheme);
  await page.click('body', { position: { x: 200, y: 20 } });

  // 2b. Pasting a clipboard image into the composer uploads it as an attachment tile.
  await page.focus('#chat-input');
  await page.evaluate(() => {
    const png = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4]);
    const dt = new DataTransfer();
    dt.items.add(new File([png], 'image.png', { type: 'image/png' }));
    const ev = new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true });
    document.getElementById('chat-input').dispatchEvent(ev);
  });
  await page.waitForFunction(
    () => /paste-\d{14}\.png/.test(document.getElementById('pending-atts')?.textContent || ''),
    { timeout: 5000 },
  );
  await page.click('#pending-atts [data-rm="0"]');
  await page.waitForSelector('#pending-atts.hide', { state: 'attached' });

  // 3. Open the right Muse drawer via floating Agenct avatar, create a Cloud Node, capture its token.
  await page.click('#muse-head-toggle');
  await page.waitForSelector('#task-pane .add-node-form');
  await page.fill('#task-pane .add-node-form input[name="name"]', 'Cloud Runner');
  await page.selectOption('#task-pane .add-node-form select[name="kind"]', 'cloud');
  await page.click('#task-pane .add-node-form button[type="submit"]');
  await page.waitForSelector('#task-pane .install-box code');
  const installText = await page.textContent('#task-pane .install-box code');
  assert.ok(installText.includes('--kind cloud --agents auto'), `expected default cloud flags: ${installText}`);
  const tokenMatch = installText.match(/agn_[0-9a-f]{64}/);
  assert.ok(tokenMatch, `expected node token in install command: ${installText}`);
  const nodeToken = tokenMatch[0];

  // 4. Send a /node chat message -> queues task on the node (and hides empty-state starter cards).
  await page.fill('#chat-input', '/node check cloud workspace');
  await page.click('#send-btn');
  await page.waitForFunction(() => document.body.textContent.includes('已将任务派发给在线执行节点'));
  await page.waitForSelector('#chat-starters.hide', { state: 'attached' });

  // 5. Run the real Python node daemon (--once) with --cmd to claim & submit the job.
  execFileSync(
    'python3',
    [
      path.join(ROOT, 'node/agenct-node.py'),
      '--hub', hubOrigin,
      '--token', nodeToken,
      '--kind', 'cloud',
      '--cmd', 'python3 -c "print(\'Node Result: e2e ok\')"',
      '--workdir', workDir,
      '--state-dir', path.join(tmpDir, 'node-state'),
      '--once',
    ],
    { stdio: 'inherit' },
  );

  // 6. Verify live SSE pushed the completed node result directly into the open browser!
  await page.waitForFunction(() => document.body.textContent.includes('Node Result:'), { timeout: 10000 });

  // Control a second node job to observe progress after the chat request returns.
  const workBar = page.locator('#stage-bar');
  await expect(workBar).toBeHidden();
  await page.fill('#chat-input', '/node verify live progress');
  await page.click('#send-btn');
  await expect(page.locator('#send-btn')).toBeEnabled();
  await expect(page.locator('#work-stage')).toHaveText('正在工作 · 等待节点接单');
  await expect(page.locator('#muse-head-toggle')).toHaveAttribute('data-agent-state', 'queued');
  const nodeRequest = async (endpoint, body) => {
    const res = await fetch(`${hubOrigin}/node/v1/${endpoint}`, {
      method: body ? 'POST' : 'GET',
      headers: {
        Authorization: `Bearer ${nodeToken}`,
        'X-Agenct-Node-Protocol': '2',
        'Content-Type': 'application/json',
      },
      body: JSON.stringify(body),
    });
    assert.ok(res.ok, `${endpoint} returned ${res.status}`);
    return res.json();
  };
  const { job } = await nodeRequest('work');
  const taskPath = `tasks/${job.task_id}`;
  const lease = { lease_token: job.lease_token, expected_revision: job.task_revision };
  const progress = { ...lease, stage: 'executing', elapsed_secs: 7, log_tail: '<b>test output</b>' };
  await nodeRequest(`${taskPath}/lease`, progress);
  await expect(page.locator('#work-stage')).toHaveText('正在工作 · 正在执行');
  await expect(page.locator('#muse-head-toggle')).toHaveClass(/\bis-thinking\b/);
  await expect(page.locator('#muse-head-toggle')).toHaveAttribute('data-agent-state', 'working');
  await expect(page.locator('#work-elapsed')).toHaveText(/^[7-9] 秒$/);
  const elapsed = async () => parseInt(await page.locator('#work-elapsed').textContent(), 10);
  const firstElapsed = await elapsed();
  await expect.poll(elapsed).toBeGreaterThan(firstElapsed);
  await page.locator('#work-log summary').click();
  await expect(page.locator('#work-log-tail')).toHaveText(progress.log_tail);
  await expect(page.locator('#work-log-tail b')).toHaveCount(0);

  // Hold a snapshot showing the running job until after its terminal SSE event.
  let releaseSnapshot;
  let snapshotReady = false;
  const held = new Promise((resolve) => { releaseSnapshot = resolve; });
  await page.route('**/api/bootstrap', async (route) => {
    const response = await route.fetch();
    snapshotReady = true;
    await held;
    await route.fulfill({ response });
  }, { times: 1 });
  try {
    await nodeRequest(`${taskPath}/lease`, { ...progress, elapsed_secs: 10 });
    await expect.poll(() => snapshotReady).toBe(true);
    await nodeRequest(`${taskPath}/submit`, {
      ...lease, request_id: 'e2e-live-progress', status: 'failed', content: 'progress test complete',
    });
    await expect(workBar).toBeHidden();
    await expect(page.locator('#work-log')).not.toHaveAttribute('open', '');
    await expect(page.locator('#muse-head-toggle')).not.toHaveClass(/\bis-thinking\b/);
    await expect(page.locator('#muse-head-toggle')).toHaveAttribute('data-agent-state', 'idle');
    await workBar.evaluate((bar) => {
      new MutationObserver((records) => {
        if (!bar.classList.contains('hide') || records.some((record) => !record.oldValue.includes('hide'))) {
          bar.dataset.revived = 'true';
        }
      }).observe(bar, { attributes: true, attributeFilter: ['class'], attributeOldValue: true });
    });
  } finally {
    releaseSnapshot();
  }
  await expect(page.locator('#messages')).toContainText('progress test complete');
  await expect(workBar).toBeHidden();
  assert.equal(await workBar.getAttribute('data-revived'), null);

  // 6b. Verify full-text search panel finds the recent chat turn via FTS5 /api/search.
  await page.click('#nav-search');
  await page.waitForSelector('#search-panel:not(.hide)');
  await page.fill('#search-input', 'cloud workspace');
  await page.waitForFunction(
    () => (document.getElementById('search-results')?.textContent || '').includes('check cloud workspace'),
    { timeout: 5000 },
  );
  await page.click('#close-search');
  await page.waitForSelector('#search-panel.hide', { state: 'attached' });

  // 6c. Add a multi-engine PC node and update its default engine via PUT /api/nodes/{id}.
  await page.locator('#task-pane summary:has-text("+ 添加新节点")').click();
  await page.waitForSelector('#task-pane .add-node-form');
  await page.fill('#task-pane .add-node-form input[name="name"]', 'Multi PC');
  await page.selectOption('#task-pane .add-node-form select[name="kind"]', 'pc');
  await page.check('#task-pane .add-node-form input[name="engines"][value="claude"]');
  await page.check('#task-pane .add-node-form input[name="engines"][value="opencode"]');
  await page.selectOption('#task-pane .add-node-form select[name="default_agent"]', 'claude');
  await page.click('#task-pane .add-node-form button[type="submit"]');
  await page.waitForFunction(
    () =>
      (document.querySelector('#task-pane .install-box code')?.textContent || '').includes(
        '--agent claude --agents claude,opencode',
      ) && (document.getElementById('task-pane')?.textContent || '').includes('claude · 默认'),
    { timeout: 5000 },
  );

  const multiDetails = page
    .locator('#task-pane details.node-repos')
    .filter({ has: page.locator('form.edit-node-form[data-node-id="multi-pc"]') });
  await multiDetails.locator('summary').click();
  const multiForm = page.locator('#task-pane form.edit-node-form[data-node-id="multi-pc"]');
  await multiForm.waitFor({ state: 'visible' });
  await multiForm.locator('select[name="default_agent"]').selectOption('opencode');
  const updatedNode = page.waitForResponse(
    (r) => r.url().endsWith('/api/nodes/multi-pc') && r.request().method() === 'PUT',
  );
  await multiForm.locator('button[type="submit"]').click();
  assert.equal((await updatedNode).status(), 200);
  await page.waitForFunction(
    () => (document.getElementById('task-pane')?.textContent || '').includes('opencode · 默认'),
    { timeout: 5000 },
  );

  // 6d. Chat-first Squad role creation, Muse drawer roster rendering, and deletion via DELETE /api/roles/{name}.
  await page.fill('#chat-input', '/role planner agent=claude model=opus-5.5 effort=high sandbox=readonly');
  await page.click('#send-btn');
  await page.waitForFunction(
    () => {
      const item = document.querySelector('#roles-card [data-role-name="planner"]')?.textContent || '';
      return item.includes('planner') && item.includes('opus-5.5') && item.includes('只读');
    },
    null,
    { timeout: 5000 },
  );
  await page.click('#roles-card > summary');
  // 6d-2. Visual Squad Role Editor: create & delete a role via PUT /api/roles/{name}.
  await page.click('#role-editor-fold > summary');
  await page.click('#role-editor-form [data-role-preset="reviewer"]');
  const putRole = page.waitForResponse(
    (r) => r.url().endsWith('/api/roles/reviewer') && r.request().method() === 'PUT',
  );
  await page.click('#role-editor-form button[type="submit"]');
  assert.equal((await putRole).status(), 200);
  await page.waitForFunction(
    () => {
      const item = document.querySelector('#roles-card [data-role-name="reviewer"]')?.textContent || '';
      return item.includes('reviewer') && item.includes('只读');
    },
    null,
    { timeout: 5000 },
  );
  const delReviewer = page.waitForResponse(
    (r) => r.url().endsWith('/api/roles/reviewer') && r.request().method() === 'DELETE',
  );
  await page.click('#roles-card [data-del-role="reviewer"]');
  assert.equal((await delReviewer).status(), 200);

  const delRole = page.waitForResponse(
    (r) => r.url().endsWith('/api/roles/planner') && r.request().method() === 'DELETE',
  );
  await page.click('#roles-card [data-del-role="planner"]');
  assert.equal((await delRole).status(), 200);
  await page.waitForFunction(
    () =>
      !document.querySelector('#roles-card [data-role-name="planner"]') &&
      (document.getElementById('roles-card')?.textContent || '').includes('我的军团 (0)'),
    null,
    { timeout: 5000 },
  );

  // 6e. Regression: optimistic user bubble must NOT duplicate once /api/bootstrap hits its 60-message cap.
  const padHandler = async (route) => {
    const resp = await route.fetch();
    const data = await resp.json();
    const realMsgs = data.messages || [];
    const padCount = Math.max(0, 60 - realMsgs.length);
    const oldestRealId = realMsgs[0]?.id || 100;
    const filler = Array.from({ length: padCount }, (_, i) => ({
      id: oldestRealId - padCount + i,
      peer_id: null,
      thread_id: null,
      role: i % 2 === 0 ? 'user' : 'assistant',
      content: `history-${i}`,
      request_id: null,
      status: 'complete',
      created_at: 1700000000 + i,
    }));
    data.messages = [...filler, ...realMsgs].slice(-60);
    data.has_more_messages = true;
    data.tasks = [
      ...(data.tasks || []),
      {
        id: 'tsk_e2e_outbound_collab',
        requester_id: data.user?.id || 'gl-user',
        owner_id: 'gl-peer-bob',
        owner_name: 'Bob',
        thread_id: null,
        title: '来自 E2E User 的协作：帮忙核查线上日志',
        instruction: '帮忙核查线上日志',
        status: 'pending',
        result: null,
        blocked_reason: '',
        revision: 0,
        updated_at: 1700000100,
      },
    ];
    try {
      await route.fulfill({ response: resp, json: data });
    } catch (err) {
      if (!/already handled/i.test(String(err))) throw err;
    }
  };
  const delayChatHandler = async (route) => {
    const resp = await route.fetch();
    await new Promise((r) => setTimeout(r, 450));
    await route.fulfill({ response: resp });
  };
  await page.route('**/api/bootstrap', padHandler);
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForSelector('#room:not(.hide)');
  await page.waitForFunction(() => document.getElementById('sse-pill')?.textContent?.includes('实时同步'));
  await page.waitForFunction(() => document.querySelectorAll('#messages .msg').length === 60, null, { timeout: 5000 });
  await page.route('**/api/chat', delayChatHandler);

  const bootDuringTurn = page.waitForResponse((r) => r.url().endsWith('/api/bootstrap'));
  const chatDone = page.waitForResponse((r) => r.url().endsWith('/api/chat'));
  await page.fill('#chat-input', 'dedup-window-60');
  await page.click('#send-btn');
  await bootDuringTurn;
  await page.waitForTimeout(120);
  assert.equal(
    await page.locator('.msg-user:has-text("dedup-window-60")').count(),
    1,
    'in-flight SSE refresh with 60-message cap must not duplicate the optimistic user bubble',
  );
  await chatDone;
  await page.waitForFunction(
    () => (document.getElementById('messages')?.textContent || '').includes('收到你的消息：dedup-window-60'),
    null,
    { timeout: 5000 },
  );
  assert.equal(await page.locator('.msg-user:has-text("dedup-window-60")').count(), 1);
  await page.unroute('**/api/chat', delayChatHandler);
  await page.click('#muse-head-toggle');

  // 7. Verify outbound delegation isolation (no red badge / not in attention workbench), 2-tab Muse drawer & Settings view.
  await page.waitForSelector('#drawer-body .panel-card h3');
  assert.equal(await page.locator('#rail-badge').textContent(), '', 'outbound pending collab must not light rail badge');
  const drawerText = await page.locator('#drawer-body').textContent();
  assert.ok(!drawerText.includes('来自 E2E User 的协作'), 'requester must not see inbound title perspective');
  const liveText = (await page.locator('#drawer-body .workbench-live').count()) > 0
    ? await page.locator('#drawer-body .workbench-live').textContent()
    : '';
  assert.ok(
    !liveText.includes('帮忙核查线上日志') && !liveText.includes('Bob'),
    'outbound collab must not appear in attention workbench',
  );
  const outboundFoldText = await page.locator('#drawer-body [data-drawer-fold="outbound-collab"]').textContent();
  assert.ok(outboundFoldText.includes('委派给 Bob：帮忙核查线上日志'));
  assert.ok(outboundFoldText.includes('待对方处理'));
  assert.ok(outboundFoldText.includes('撤回协作'));
  await page.unroute('**/api/bootstrap', padHandler);
  const firstThreadMore = page.locator('#task-pane .thread-card [data-thread-menu]').first();
  if ((await firstThreadMore.count()) > 0) {
    await firstThreadMore.click();
    await page.waitForSelector('#ctx-menu:not(.hide) .ctx-item');
    await page.keyboard.press('Escape');
    await page.waitForSelector('#ctx-menu.hide', { state: 'attached' });
  }
  await page.click('.muse-seg button[data-tab="soul"]');
  await page.waitForSelector('#context-meter-card .ctx-meter-track');
  await page.waitForFunction(() => document.querySelectorAll('#activity-heat-card .heat-cell').length === 84);
  await page.waitForSelector('#chat-tune-memory');
  await page.click('[data-pick-avatar="sakura"]');
  await page.waitForFunction(
    () =>
      (document.getElementById('head-avatar')?.getAttribute('src') || '').startsWith('avatars/sakura.webp') &&
      (document.getElementById('drawer-avatar')?.getAttribute('src') || '').startsWith('avatars/sakura.webp'),
    { timeout: 5000 },
  );
  await page.click('[data-pick-avatar="buddy"]');
  await page.waitForSelector('#head-avatar-buddy:not(.hide) .buddy-svg');
  await page.waitForSelector('#drawer-avatar-buddy:not(.hide) .buddy-svg');
  await expect(page.locator('#head-avatar')).toHaveClass(/\bhide\b/);
  await expect(page.locator('#muse-head-toggle')).toHaveClass(/\bis-live-buddy\b/);
  await expect(page.locator('#avatar-state-pill')).toHaveText('在线待命');
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForSelector('#room:not(.hide)');
  await page.waitForSelector('#head-avatar-buddy:not(.hide) .buddy-svg');

  // 7b. Verify mobile viewport adaptation (chat overflow containment, composer visibility, drawer + avatar grid, overlay nav, settings).
  for (const vp of [
    { width: 375, height: 667 },
    { width: 390, height: 844 },
  ]) {
    await page.setViewportSize(vp);
    await page.evaluate(() => {
      const list = document.getElementById('messages');
      if (document.getElementById('e2e-mobile-overflow-probe')) return;
      const div = document.createElement('div');
      div.id = 'e2e-mobile-overflow-probe';
      div.className = 'msg msg-assistant';
      div.innerHTML = `<div class="md-body">
        <p class="md-p">检查超长链接：https://gitlab.internal.example.com/example-org/agenct/merge_requests/12345/diffs?view=parallel&token=abcdefghijklmnopqrstuvwxyz0123456789</p>
        <pre class="md-pre"><code>cargo clippy --manifest-path hub/Cargo.toml --all-targets -- -D warnings && python3 node/agenct-node.py --self-check --agents codex,agy,opencode</code></pre>
        <div class="md-table-wrap"><table class="md-table"><thead><tr><th>模块</th><th>引擎版本</th><th>状态说明</th><th>超宽测试列信息</th></tr></thead><tbody><tr><td>hub/src/brain/mod.rs</td><td>claude-opus-4-6-extended</td><td>已通过</td><td>用于验证手机端不会横向撑破消息气泡或页面容器</td></tr></tbody></table></div>
      </div>`;
      list.appendChild(div);
    });
    const chatMetrics = await page.evaluate(() => {
      const vw = window.innerWidth;
      const vh = window.innerHeight;
      const scroller = document.getElementById('scroller');
      const composer = document.getElementById('chat-form').getBoundingClientRect();
      const sendBtn = document.getElementById('send-btn').getBoundingClientRect();
      return {
        vw,
        vh,
        docSW: document.documentElement.scrollWidth,
        scrollerSW: scroller.scrollWidth,
        scrollerCW: scroller.clientWidth,
        composerBottom: Math.round(composer.bottom),
        sendBtnRight: Math.round(sendBtn.right),
        inputFontSize: getComputedStyle(document.getElementById('chat-input')).fontSize,
      };
    });
    assert.ok(chatMetrics.docSW <= chatMetrics.vw, `mobile chat horizontal overflow (${vp.width}px): docSW=${chatMetrics.docSW} > vw=${chatMetrics.vw}`);
    assert.ok(chatMetrics.scrollerSW <= chatMetrics.scrollerCW, `mobile scroller overflow (${vp.width}px): ${chatMetrics.scrollerSW} > ${chatMetrics.scrollerCW}`);
    assert.ok(chatMetrics.composerBottom <= chatMetrics.vh + 1, `mobile composer clipped at bottom (${vp.width}px): ${chatMetrics.composerBottom} > ${chatMetrics.vh}`);
    assert.ok(chatMetrics.sendBtnRight <= chatMetrics.vw, `mobile send button clipped at right (${vp.width}px): ${chatMetrics.sendBtnRight} > ${chatMetrics.vw}`);
    assert.equal(chatMetrics.inputFontSize, '16px', 'mobile chat input must be 16px to prevent iOS focus auto-zoom');

    await page.click('#nav-tasks');
    await page.waitForSelector('#task-pane .muse-seg button[data-tab="soul"]');
    await page.click('.muse-seg button[data-tab="soul"]');
    await page.waitForSelector('#task-pane .avatar-grid');
    const drawerMetrics = await page.evaluate(() => {
      const vw = window.innerWidth;
      const body = document.getElementById('drawer-body');
      const grid = document.querySelector('#task-pane .avatar-grid');
      const maxChoiceRight = Math.max(
        ...Array.from(document.querySelectorAll('#task-pane .avatar-choice')).map((el) => Math.round(el.getBoundingClientRect().right)),
      );
      return {
        vw,
        docSW: document.documentElement.scrollWidth,
        bodySW: body.scrollWidth,
        bodyCW: body.clientWidth,
        gridSW: grid.scrollWidth,
        gridCW: grid.clientWidth,
        maxChoiceRight,
      };
    });
    assert.ok(drawerMetrics.docSW <= drawerMetrics.vw, `mobile drawer overflow (${vp.width}px): docSW=${drawerMetrics.docSW} > vw=${drawerMetrics.vw}`);
    assert.ok(drawerMetrics.bodySW <= drawerMetrics.bodyCW, `mobile drawerBody overflow (${vp.width}px): ${drawerMetrics.bodySW} > ${drawerMetrics.bodyCW}`);
    assert.ok(drawerMetrics.gridSW <= drawerMetrics.gridCW, `mobile avatarGrid overflow (${vp.width}px): ${drawerMetrics.gridSW} > ${drawerMetrics.gridCW}`);
    assert.ok(drawerMetrics.maxChoiceRight <= drawerMetrics.vw, `mobile avatarChoice clipped (${vp.width}px): ${drawerMetrics.maxChoiceRight} > ${drawerMetrics.vw}`);

    await page.click('#nav-chat');
    await page.waitForSelector('#room:not(.tasks-open)');

    await page.click('#rail-more');
    await page.click('#open-settings');
    await page.waitForSelector('#settings-view:not(.hide) #models-form');
    const settingsMetrics = await page.evaluate(() => {
      const el = document.getElementById('settings-view');
      const rc = el.getBoundingClientRect();
      return {
        vw: window.innerWidth,
        vh: window.innerHeight,
        bottom: Math.round(rc.bottom),
        sw: el.scrollWidth,
        cw: el.clientWidth,
      };
    });
    assert.ok(settingsMetrics.bottom <= settingsMetrics.vh + 1, `mobile settings pane clipped at bottom (${vp.width}px): ${settingsMetrics.bottom} > ${settingsMetrics.vh}`);
    assert.ok(settingsMetrics.sw <= settingsMetrics.cw, `mobile settings pane overflow (${vp.width}px): ${settingsMetrics.sw} > ${settingsMetrics.cw}`);
    await page.click('#close-settings');
  }
  await page.setViewportSize({ width: 1280, height: 800 });

  assert.deepEqual(errors, [], `unexpected browser errors: ${errors.join('; ')}`);

  // 7c. Categorized Settings Tabs + Per-user brain model settings (SSRF guard).
  await page.click('#rail-more');
  await page.click('#open-settings');
  await page.waitForSelector('#perms-form');
  await page.waitForSelector('#models-form');
  await page.click('#settings-tabs [data-settings-tab="channels"]');
  await page.waitForSelector('#settings-body [data-settings-panel="channels"]:not(.hide) #im-form');
  await page.click('#settings-tabs [data-settings-tab="nodes"]');
  await page.waitForSelector('#settings-body [data-settings-panel="nodes"]:not(.hide) summary:has-text("+ 添加新节点")');
  await page.click('#settings-tabs [data-settings-tab="model-auth"]');
  await page.waitForSelector('#settings-body [data-settings-panel="model-auth"]:not(.hide) #models-form');
  assert.ok((await page.textContent('#settings-body')).includes('服务器默认：stub'));
  await page.fill('#models-form input[name="brain_url"]', 'https://10.20.30.40/v1/chat/completions');
  await page.fill('#models-form input[name="brain_model"]', 'evil');
  await page.fill('#models-form input[name="brain_key"]', 'sk-test-123456789');
  const rejected = page.waitForResponse((r) => r.url().endsWith('/api/settings/models') && r.request().method() === 'PUT');
  await page.click('#models-form button[type="submit"]');
  assert.equal((await rejected).status(), 400);
  assert.deepEqual(
    errors.filter((e) => !e.includes('status of 400')),
    [],
    `unexpected browser errors during SSRF check: ${errors.join('; ')}`,
  );

  // 8. Verify backup.py backup + restore works cleanly against live SQLite DB.
  const archive = path.join(tmpDir, 'backup.zip');
  const restored = path.join(tmpDir, 'restored');
  execFileSync('python3', [path.join(ROOT, 'scripts/backup.py'), 'backup', '--data-dir', dataDir, '--archive', archive], { stdio: 'inherit' });
  execFileSync('python3', [path.join(ROOT, 'scripts/backup.py'), 'restore', '--archive', archive, '--data-dir', restored], { stdio: 'inherit' });
  assert.ok(fs.existsSync(path.join(restored, 'agenct.sqlite3')));

  console.log('E2E_OK: real Hub + GitLab OAuth PKCE + real SSE + real Python Node + real Chromium + backup/restore all passed!');
} finally {
  if (browser) await browser.close();
  hub.kill('SIGINT');
  glServer.close();
  fs.rmSync(tmpDir, { recursive: true, force: true });
}
