// E2E: zero-config local owner login (no GitLab) — real Hub binary + real Chromium.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { chromium } from '@playwright/test';

const ROOT = process.cwd();
const HUB_BIN = path.join(ROOT, 'hub/target/debug/agenct-hub');
const SCREENSHOT = process.env.E2E_SCREENSHOT_DIR;

function freePort() {
  return new Promise((resolve, reject) => {
    const s = http.createServer();
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close((err) => (err ? reject(err) : resolve(port)));
    });
  });
}

const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-local-e2e-'));
const port = await freePort();
const origin = `http://127.0.0.1:${port}`;
const env = { ...process.env };
for (const k of Object.keys(env)) if (k.startsWith('AGENCT_') || k.startsWith('GITLAB_')) delete env[k];
const hub = spawn(HUB_BIN, [], {
  cwd: ROOT,
  env: {
    ...env,
    AGENCT_BIND: `127.0.0.1:${port}`,
    AGENCT_DATA_DIR: tmpDir,
    AGENCT_PUBLIC_ORIGIN: origin,
    AGENCT_ALLOW_HTTP_ORIGIN: '1',
    AGENCT_LLM_STUB: '1',
  },
  stdio: ['ignore', 'pipe', 'pipe'],
});
let logs = '';
hub.stdout.on('data', (d) => (logs += d));
hub.stderr.on('data', (d) => (logs += d));

let browser;
try {
  // 1. Hub starts with no GitLab config and prints a one-time login token.
  for (let i = 0; i < 60 && !/agl_[0-9a-f]{64}/.test(logs); i++) await new Promise((r) => setTimeout(r, 100));
  const token = logs.match(/agl_[0-9a-f]{64}/)?.[0];
  assert.ok(token, `expected login token in Hub logs:\n${logs}`);
  assert.equal(fs.readFileSync(path.join(tmpDir, 'login-token'), 'utf8').trim(), token);

  browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  const errors = [];
  page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));

  // 2. Login page shows only the local token form.
  await page.goto(origin, { waitUntil: 'networkidle' });
  await page.waitForSelector('#local-login-form:not(.hide)');
  assert.ok(await page.isHidden('#gitlab-login-btn'));
  if (SCREENSHOT) await page.screenshot({ path: path.join(SCREENSHOT, 'local-login.png') });

  // 3. Wrong token is rejected with a clear message.
  await page.fill('#local-login-token', 'agl_wrong');
  await page.click('#local-login-form button[type="submit"]');
  await page.waitForFunction(() => document.getElementById('login-error')?.textContent.includes('登录令牌不正确'));

  // 4. Correct token logs in to the workspace; chat works.
  await page.fill('#local-login-token', token);
  await page.click('#local-login-form button[type="submit"]');
  await page.waitForSelector('#room:not(.hide)', { timeout: 10000 });
  await page.fill('#chat-input', 'hello from local login');
  await page.click('#send-btn');
  await page.waitForFunction(() => document.body.textContent.includes('收到你的消息：hello from local login'));
  if (SCREENSHOT) await page.screenshot({ path: path.join(SCREENSHOT, 'local-login-room.png') });

  assert.deepEqual(errors, [], `unexpected browser errors: ${errors.join('; ')}`);
  console.log('E2E_LOCAL_OK: zero-config local token login works end to end');
} finally {
  if (browser) await browser.close();
  hub.kill('SIGINT');
  fs.rmSync(tmpDir, { recursive: true, force: true });
}
