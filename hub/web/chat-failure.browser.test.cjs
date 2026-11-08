/* Real temporary Hub + deterministic local adapter. No external model calls.
 * Build first: cargo build --locked --manifest-path hub/Cargo.toml
 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const http = require('node:http');
const net = require('node:net');
const { createHash, randomBytes } = require('node:crypto');
const { spawn } = require('node:child_process');
const { chromium } = require('playwright');

const root = path.resolve(__dirname, '../..');
const binary = process.env.AGENCT_TEST_HUB_BINARY || path.join(root, 'hub/target/debug/agenct-hub');
const STORAGE_KEY = 'agenct.pending-chat-turn.v1';
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
async function freePort() {
  const server = net.createServer();
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  return port;
}

(async () => {
  assert(fs.existsSync(binary), 'Build the Hub before running this test');
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-chat-failure-'));
  const screenshots = process.env.AGENCT_TEST_SCREENSHOT_DIR || fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-chat-failure-screens-'));
  fs.mkdirSync(screenshots, { recursive: true });
  const token = randomBytes(32).toString('hex'), adapterToken = randomBytes(32).toString('hex');
  const adapterCalls = [], held = new Map(), pageErrors = [], posts = [];
  let hub, browser, page;
  function reply(response, code = 'runtime_failed') {
    response.writeHead(code === 'runtime_timeout' ? 504 : 502, { 'Content-Type': 'application/json' });
    response.end(JSON.stringify({ error: code }));
  }
  const adapter = http.createServer(async (request, response) => {
    if (request.url !== '/v1/turn' || request.headers.authorization !== 'Bearer ' + adapterToken) {
      response.writeHead(404).end(); return;
    }
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const payload = JSON.parse(Buffer.concat(chunks).toString('utf8'));
    adapterCalls.push(payload.message);
    if (payload.message === '等待期间继续写下一条') { held.set(payload.message, response); return; }
    if (payload.message === '未知网关失败') {
      response.writeHead(502, { 'Content-Type': 'text/html' }); response.end('<html>gateway failed</html>'); return;
    }
    reply(response);
  });
  await new Promise(resolve => adapter.listen(0, '127.0.0.1', resolve));
  const port = await freePort(), origin = `http://127.0.0.1:${port}`;
  async function stop() {
    await browser?.close().catch(() => {}); browser = null;
    for (const response of held.values()) response.destroy(); held.clear();
    if (hub && hub.exitCode === null) await new Promise(resolve => {
      const child = hub, timer = setTimeout(() => child.kill('SIGKILL'), 5000);
      child.once('exit', () => { clearTimeout(timer); resolve(); }); child.kill('SIGINT');
    });
    hub = null;
    adapter.closeAllConnections?.();
    await new Promise(resolve => adapter.close(resolve));
  }
  const interrupted = () => { void stop(); };
  process.once('SIGINT', interrupted); process.once('SIGTERM', interrupted);
  async function api(route, method = 'GET', body) {
    const response = await fetch(origin + '/api/' + route, { method,
      headers: { authorization: 'Bearer ' + token, 'content-type': 'application/json' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
    assert.equal(response.status, 200);
    return response.json();
  }
  async function send(text) { await page.locator('#text').fill(text); await page.locator('#send').click(); }
  async function unlocked() {
    await page.waitForFunction(() => !document.getElementById('send').disabled && document.getElementById('turn-recovery').classList.contains('hide'));
    assert.equal(await page.evaluate(key => sessionStorage.getItem(key), STORAGE_KEY), null);
  }
  try {
    hub = spawn(binary, [], { cwd: root, stdio: ['ignore', 'ignore', 'pipe'], env: {
      PATH: process.env.PATH, HOME: process.env.HOME, LANG: process.env.LANG || 'C.UTF-8',
      AGENCT_BIND: `127.0.0.1:${port}`, AGENCT_DATABASE: path.join(temporary, 'hub.sqlite3'),
      AGENCT_RUNTIME: 'dsh', AGENCT_ADAPTER_URL: `http://127.0.0.1:${adapter.address().port}/v1/turn`,
      AGENCT_ADAPTER_TOKEN: adapterToken,
      AGENCT_USERS_JSON: JSON.stringify([{ id: 'alice', token_sha256: createHash('sha256').update(token).digest('hex') }])
    } });
    let startupError, ready = false;
    hub.once('error', error => { startupError = error; }); hub.stderr.on('data', () => {});
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
    page = await browser.newPage({ viewport: { width: 1440, height: 960 } });
    page.on('pageerror', error => pageErrors.push(error.message));
    page.on('request', request => {
      if (request.url() === origin + '/api/messages' && request.method() === 'POST') posts.push(request.postDataJSON());
    });
    await page.goto(origin); await page.locator('#token').fill(token); await page.locator('#enter').click();
    await page.locator('#room:not(.hide)').waitFor(); await page.locator('#open-chat').click();

    await send('第一次请求明确失败'); await unlocked();
    assert.equal(await page.locator('#text').inputValue(), '第一次请求明确失败');
    assert.equal(adapterCalls.length, 1);
    const firstTurn = posts[0];
    assert.equal((await api('turns/' + firstTurn.request_id)).status, 'failed');
    assert.equal((await api('messages', 'POST', firstTurn)).turn.status, 'failed');
    assert.equal(adapterCalls.length, 1, 'Replaying a known failure must not redispatch the provider');
    await page.locator('.msg-failed').filter({ hasText: '第一次请求明确失败' }).waitFor();
    assert((await page.locator('.msg-failed').innerText()).includes('没有取得回复'));
    await page.screenshot({ path: path.join(screenshots, 'failed-turn-desktop.png') });
    console.log('PASS explicit adapter failure unlocks the composer, restores the question and never auto-retries');

    await send('等待期间继续写下一条');
    for (let attempt = 0; attempt < 100 && !held.has('等待期间继续写下一条'); attempt++) await pause(50);
    assert(held.has('等待期间继续写下一条'));
    await page.locator('#text').fill('已经写好的下一条问题');
    reply(held.get('等待期间继续写下一条'), 'runtime_timeout'); held.delete('等待期间继续写下一条');
    await unlocked();
    assert.equal(await page.locator('#text').inputValue(), '已经写好的下一条问题');
    assert.equal(adapterCalls.length, 2);
    assert.equal((await api('turns/' + posts[1].request_id)).failure.code, 'runtime_timeout');
    console.log('PASS known adapter timeout preserves the next unsent draft rather than replacing or sending it');

    await page.route('**/api/messages', async route => {
      if (route.request().method() === 'POST' && route.request().postDataJSON()?.message === '保存失败回执后断线') {
        await route.fetch(); // Hub commits its explicit failed receipt, but the tab loses the response.
        await route.abort('failed'); return;
      }
      await route.continue();
    });
    await send('保存失败回执后断线');
    await page.waitForFunction(() => !document.getElementById('check-turn').disabled);
    assert.equal(await page.locator('#send').isDisabled(), true);
    const saved = await page.evaluate(key => sessionStorage.getItem(key), STORAGE_KEY);
    assert(saved && !saved.includes('保存失败回执后断线'), 'Recovery storage contains only the receipt identity');
    const countBeforeReload = adapterCalls.length;
    await page.reload();
    await page.locator('#room:not(.hide)').waitFor(); await unlocked();
    assert.equal(adapterCalls.length, countBeforeReload, 'Reload must GET the failure receipt, never repeat the model call');
    assert.equal(await page.locator('#text').inputValue(), '');
    const failed = page.locator('.msg-failed').filter({ hasText: '保存失败回执后断线' });
    await failed.getByRole('button', { name: '重新问', exact: true }).click();
    assert.equal(await page.locator('#text').inputValue(), '保存失败回执后断线');
    assert.equal(adapterCalls.length, countBeforeReload, 'Preparing the original question does not send it');
    await page.setViewportSize({ width: 390, height: 844 });
    assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
    await page.screenshot({ path: path.join(screenshots, 'recovered-failure-mobile.png') });
    console.log('PASS lost POST response recovers a failed receipt on reload without redispatch or locking the composer');

    let failRefresh = true;
    await page.route('**/api/tasks', route => failRefresh ? route.abort('failed') : route.continue());
    await send('已知失败但刷新暂不可用'); await unlocked();
    assert.equal(await page.locator('#text').inputValue(), '已知失败但刷新暂不可用');
    assert.equal((await api('turns/' + posts.at(-1).request_id)).status, 'failed');
    failRefresh = false;
    console.log('PASS failure handling remains terminal and usable when the follow-up history refresh fails');

    await send('未知网关失败');
    await page.waitForFunction(() => !document.getElementById('check-turn').disabled);
    assert.equal(await page.locator('#send').isDisabled(), true);
    const unknownCalls = adapterCalls.length;
    await page.locator('#check-turn').click();
    await page.waitForFunction(() => !document.getElementById('check-turn').disabled);
    assert.equal((await api('turns/' + posts.at(-1).request_id)).status, 'uncertain');
    assert.equal(adapterCalls.length, unknownCalls);
    assert.equal(await page.locator('#send').isDisabled(), true);
    assert.deepEqual(pageErrors, []);
    console.log('PASS malformed gateway responses remain unknown and status checks do not resend');
    console.log('Screenshots: ' + screenshots);
  } catch (error) {
    if (pageErrors.length) console.error('Browser exceptions:', pageErrors);
    await page?.screenshot({ path: path.join(screenshots, 'failure.png') }).catch(() => {});
    console.error('Failure screenshot: ' + screenshots);
    throw error;
  } finally {
    await stop(); process.removeListener('SIGINT', interrupted); process.removeListener('SIGTERM', interrupted);
    fs.rmSync(temporary, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
