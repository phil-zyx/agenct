// Self-contained E2E smoke test: real Rust Hub + mock GitLab OAuth + real Python Node + real Chromium + backup/restore.
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { chromium } from '@playwright/test';

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

  // 2b. Pasting a clipboard image into the composer uploads it as an attachment chip.
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

  // 4. Send a /node chat message -> queues task on the node.
  await page.fill('#chat-input', '/node check cloud workspace');
  await page.click('#send-btn');
  await page.waitForFunction(() => document.body.textContent.includes('已将任务派发给在线执行节点'));

  // 5. Run the real Python node daemon (--once) in template mode to claim & submit the job.
  execFileSync(
    'python3',
    [
      path.join(ROOT, 'node/agenct-node.py'),
      '--hub', hubOrigin,
      '--token', nodeToken,
      '--kind', 'cloud',
      '--agent', 'template',
      '--workdir', workDir,
      '--state-dir', path.join(tmpDir, 'node-state'),
      '--once',
    ],
    { stdio: 'inherit' },
  );

  // 6. Verify live SSE pushed the completed node result directly into the open browser!
  await page.waitForFunction(() => document.body.textContent.includes('Node Result:'), { timeout: 10000 });

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
    await route.fulfill({ response: resp, json: data });
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
  await page.unroute('**/api/bootstrap', padHandler);
  await page.click('#muse-head-toggle');

  // 7. Verify 2-tab Muse drawer (Work Threads + Soul/Memory), live dynamic avatar picker & Settings view.
  await page.waitForSelector('#drawer-body .panel-card h3');
  await page.click('.muse-seg button[data-tab="soul"]');
  await page.waitForSelector('#chat-tune-memory');
  await page.click('[data-pick-avatar="sakura"]');
  await page.waitForFunction(
    () =>
      (document.getElementById('head-avatar')?.getAttribute('src') || '').startsWith('avatars/sakura.webp') &&
      (document.getElementById('drawer-avatar')?.getAttribute('src') || '').startsWith('avatars/sakura.webp'),
    { timeout: 5000 },
  );
  await page.click('#rail-more');
  await page.click('#open-settings');
  await page.waitForSelector('#perms-form');

  // 7b. Per-user brain model settings: private URLs are rejected by the Hub (SSRF guard).
  await page.waitForSelector('#models-form');
  assert.ok((await page.textContent('#settings-body')).includes('服务器默认：stub'));
  await page.fill('#models-form input[name="brain_url"]', 'https://10.0.0.11/v1/chat/completions');
  await page.fill('#models-form input[name="brain_model"]', 'evil');
  await page.fill('#models-form input[name="brain_key"]', 'sk-test-123456789');
  const rejected = page.waitForResponse((r) => r.url().endsWith('/api/settings/models') && r.request().method() === 'PUT');
  await page.click('#models-form button[type="submit"]');
  assert.equal((await rejected).status(), 400);

  assert.deepEqual(errors, [], `unexpected browser errors: ${errors.join('; ')}`);

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
