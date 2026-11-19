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

  // 3. Open the right Muse drawer via floating Agenct avatar, create a Cloud Node, capture its token.
  await page.click('#muse-head-toggle');
  await page.waitForSelector('#task-pane .add-node-form');
  await page.fill('#task-pane .add-node-form input[name="name"]', 'Cloud Runner');
  await page.selectOption('#task-pane .add-node-form select[name="kind"]', 'cloud');
  await page.click('#task-pane .add-node-form button[type="submit"]');
  await page.waitForSelector('#task-pane .install-box code');
  const installText = await page.textContent('#task-pane .install-box code');
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

  // 7. Verify 2-tab Muse drawer (Tasks + Soul/Memory) & Settings view.
  await page.waitForSelector('#drawer-body .panel-card h3');
  await page.click('.muse-seg button[data-tab="soul"]');
  await page.waitForSelector('#chat-tune-memory');
  await page.click('#rail-more');
  await page.click('#open-settings');
  await page.waitForSelector('#perms-form');

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
