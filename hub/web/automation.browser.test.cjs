/* Real Hub + Chromium; isolated database and mock runtime, no external services. */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const { spawn } = require('node:child_process');
const { createHash, randomBytes } = require('node:crypto');
const { chromium } = require('playwright');
(async () => {
  const root = path.resolve(__dirname, '../..');
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-automation-'));
  const shots = process.env.AGENCT_TEST_SCREENSHOT_DIR || fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-automation-screens-'));
  fs.mkdirSync(shots, { recursive: true });
  const socket = net.createServer(); await new Promise(r => socket.listen(0, '127.0.0.1', r));
  const port = socket.address().port; await new Promise(r => socket.close(r));
  const token = randomBytes(32).toString('hex'), origin = `http://127.0.0.1:${port}`;
  const hub = spawn(process.env.AGENCT_TEST_HUB_BINARY || path.join(root, 'hub/target/debug/agenct-hub'), [], {
    cwd: root, stdio: 'ignore', env: { PATH: process.env.PATH, AGENCT_BIND: `127.0.0.1:${port}`, AGENCT_DATABASE: path.join(dir, 'hub.db'), AGENCT_RUNTIME: 'mock', AGENCT_USERS_JSON: JSON.stringify([{ id: 'alice', token_sha256: createHash('sha256').update(token).digest('hex') }]) }
  });
  let browser;
  const api = async (method = 'GET', body) => {
    const res = await fetch(origin + '/api/automation/permissions', { method, headers: { authorization: 'Bearer ' + token, 'content-type': 'application/json' }, ...(body ? { body: JSON.stringify(body) } : {}) });
    assert.equal(res.status, 200); return res.json();
  };
  try {
    for (let i = 0; ; i++) { try { if ((await fetch(origin + '/health')).ok) break; } catch {} if (i > 100) throw Error('Hub did not start'); await new Promise(r => setTimeout(r, 100)); }
    browser = await chromium.launch({ headless: true });
    for (const width of [1330, 400]) {
      const page = await browser.newPage({ viewport: { width, height: 900 } });
      const errors = []; page.on('pageerror', e => errors.push(e.message));
      await page.goto(origin); await page.locator('#token').fill(token); await page.locator('#enter').click();
      await page.locator('#room:not(.hide)').waitFor();
      await page.locator('#rail-more').click(); await page.locator('#open-account').click();
      const panel = page.locator('.automation-section');
      const save = panel.getByRole('button', { name: '保存自动化权限', exact: true });
      await save.waitFor(); await page.waitForFunction(() => !document.querySelector('.automation-section button.primary')?.disabled);
      const check = panel.getByRole('checkbox').first();
      const before = await api();
      await check.focus(); await page.keyboard.press('Space');
      assert.equal((await api()).revision, before.revision, 'toggling must not persist');
      await save.click(); await panel.getByRole('status').filter({ hasText: '自动化权限已保存' }).waitFor();
      assert.equal((await api()).auto_accept, !before.auto_accept);
      const external = await api(); await api('PUT', { ...external, gitlab_reply: !external.gitlab_reply });
      await save.click(); await panel.getByRole('status').filter({ hasText: '重新读取权限' }).waitFor();
      assert(await save.isDisabled());
      await panel.getByRole('button', { name: '重新读取权限', exact: true }).click();
      await page.waitForFunction(() => !document.querySelector('.automation-section button.primary')?.disabled);
      assert.equal(await panel.getByRole('checkbox').nth(3).isChecked(), !external.gitlab_reply);
      await panel.scrollIntoViewIfNeeded();
      assert(await panel.evaluate(el => el.scrollWidth <= el.clientWidth + 1), 'permissions must fit the panel');
      await panel.screenshot({ path: path.join(shots, `automation-${width}.png`) });
      assert.deepEqual(errors, []); await page.close();
    }
    console.log('PASS automation consent, keyboard toggling, persistence, stale revision recovery, desktop/mobile layout');
    console.log('Screenshots: ' + shots);
  } finally {
    await browser?.close();
    if (hub.exitCode === null) await new Promise(resolve => { const timer = setTimeout(() => hub.kill('SIGKILL'), 5000); hub.once('exit', () => { clearTimeout(timer); resolve(); }); hub.kill('SIGINT'); });
    fs.rmSync(dir, { recursive: true, force: true });
  }
})().catch(e => { console.error(e); process.exitCode = 1; });
