/* Real Chromium checks for contact-send races. Isolated API fixtures; no model calls. */
const assert = require('node:assert/strict');
const { chromium } = require('playwright');
const { fixture, server } = require('./client.browser.test.cjs');

function deferred() {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
}
function message(id, sender, recipient, body, authorKind = 'human') {
  return { id, sender, recipient, body, author_kind: authorKind, created_at: '2026-11-07 12:00:00' };
}
async function contactFixture(browser, origin) {
  const f = await fixture(browser, origin);
  const page = f.page;
  await page.setViewportSize({ width: 1440, height: 900 });
  // Drive reads deliberately so the tested interleavings don't depend on a timer.
  await page.evaluate(() => { clearTimeout(pollTimer); pollTimer = null; });
  await page.route('**/api/contacts', route => route.fulfill({ json: ['bob', 'carol'] }));
  await page.route('**/api/people', route => route.fulfill({ json: [
    { id: 'bob', display_name: '张明', color: 'green' },
    { id: 'carol', display_name: '李晓', color: 'blue' }
  ] }));
  await page.route('**/api/conversations', route => route.fulfill({ json: [] }));
  await page.route('**/api/contacts/*/profile', route => route.fulfill({ json: { public: {
    team: '接口组', responsibilities: '接口设计', help_with: '审核文档', request_requirements: '请提供文档'
  } } }));
  await page.route('**/api/contacts/*/read', route => route.fulfill({ json: { ok: true } }));
  return {
    ...f,
    async directory() { await page.locator('#open-contacts').click(); await page.locator('.contact-row[data-contact="bob"]').waitFor(); },
    async open(id) {
      await page.locator(`.contact-row[data-contact="${id}"]`).click();
      await page.waitForFunction(id => peerState?.id === id && !peerState.loading, id);
    },
    async sendContact(body) { await page.locator('#peer-message-input').fill(body); await page.locator('#peer-send').click(); }
  };
}

(async () => {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const browser = await chromium.launch({ headless: true,
    ...(process.env.CHROMIUM_EXECUTABLE_PATH ? { executablePath: process.env.CHROMIUM_EXECUTABLE_PATH } : {}) });
  try {
    {
      const f = await contactFixture(browser, origin), page = f.page;
      const first = deferred(), posts = []; let history = [];
      await page.route('**/api/contacts/*/messages', async route => {
        const request = route.request(), contact = new URL(request.url()).pathname.split('/')[3];
        if (request.method() === 'GET') return route.fulfill({ json: contact === 'bob' ? history : [] });
        const body = request.postDataJSON(); posts.push(body);
        if (posts.length === 1) { first.resolve(route); return; }
        const saved = message(1, 'alice', contact, body.body); history = [saved];
        await route.fulfill({ json: { message: saved, reply: null } });
      });
      await f.directory(); await f.open('bob');
      await f.sendContact('切换联系人期间也不能丢失这条消息');
      const held = await first.promise;
      assert.equal(await page.locator('#peer-message-input').inputValue(), '');
      await page.locator('#peer-messages .pending').waitFor();
      await f.open('carol');
      assert.equal(await page.locator('#peer-messages .pending').count(), 0);
      await f.open('bob');
      await held.abort('failed');
      await page.getByRole('button', { name: '重试发送', exact: true }).waitFor();
      assert.equal(await page.locator('#peer-messages .pending .peer-message-body').textContent(), '切换联系人期间也不能丢失这条消息');
      await page.getByRole('button', { name: '重试发送', exact: true }).click();
      await page.waitForFunction(() => !peerState.draft.pending && !document.getElementById('peer-send').disabled);
      assert.equal(posts.length, 2);
      assert.equal(posts[1].request_id, posts[0].request_id);
      assert.equal(posts[1].body, posts[0].body);
      assert.equal(await page.locator('#peer-messages .peer-message-body').count(), 1);
      assert.match(await page.locator('#peer-messages').textContent(), /你 · 本人/);
      await f.close();
      console.log('PASS contact send failure after leaving and reopening exposes retry and reuses the request ID');
    }
    {
      const f = await contactFixture(browser, origin), page = f.page;
      const capturedGet = deferred(); let holdNextGet = false, history = [];
      await page.route('**/api/contacts/*/messages', async route => {
        if (route.request().method() === 'GET') {
          if (holdNextGet) { holdNextGet = false; capturedGet.resolve(route); return; }
          return route.fulfill({ json: history });
        }
        const input = route.request().postDataJSON();
        const saved = message(10, 'alice', 'bob', input.body);
        const reply = message(11, 'bob', 'alice', '根据公开资料：负责接口设计。具体安排需本人确认。', 'assistant');
        history = [saved, reply];
        await route.fulfill({ json: { message: saved, reply } });
      });
      await f.directory(); await f.open('bob');
      holdNextGet = true;
      await page.evaluate(() => { void refreshPeerMessages(); });
      const stale = await capturedGet.promise;
      await f.sendContact('你负责什么？');
      // The POST receipt must render without waiting for the earlier GET.
      await page.getByText('你负责什么？', { exact: true }).waitFor();
      await page.getByText('张明的助手 · 自动回复', { exact: true }).waitFor();
      assert.equal(await page.locator('#peer-messages .pending').count(), 0);
      await stale.fulfill({ json: [] });
      await page.waitForFunction(() => peerReadFlight === null);
      assert.deepEqual(await page.locator('#peer-messages .peer-message-body').allTextContents(), history.map(m => m.body));
      await page.evaluate(() => refreshPeerMessages());
      assert.equal(await page.locator('#peer-messages .peer-message-body').count(), 2);
      assert.equal(await page.locator('#peer-message-input').inputValue(), '');
      await f.close();
      console.log('PASS contact POST receipt appears immediately and survives an older GET snapshot without duplicate bubbles');
    }
    {
      const f = await contactFixture(browser, origin), page = f.page;
      const capturedPost = deferred();
      await page.route('**/api/contacts/*/messages', route => {
        if (route.request().method() === 'GET') return route.fulfill({ json: [] });
        capturedPost.resolve({ route, input: route.request().postDataJSON() });
      });
      await f.directory(); await f.open('bob');
      await f.sendContact('只属于 Alice 的在途消息');
      const held = await capturedPost.promise;
      await page.locator('#peer-message-input').fill('只属于 Alice 的下一条草稿');
      await page.locator('#logout').click();
      await page.waitForSelector('#dev-login:not(.hide)');
      assert.equal(await page.evaluate(() => peerDrafts.size), 0);
      assert.equal(await page.evaluate(() => peerState), null);
      assert.equal(await page.locator('#peer-messages').textContent(), '');
      assert.equal(await page.locator('#peer-message-input').inputValue(), '');
      // The old response arrives after a different identity has signed in.
      await f.login('eve');
      await f.directory(); await f.open('bob');
      await held.route.fulfill({ json: { message: message(20, 'alice', 'bob', held.input.body), reply: null } }).catch(error => {
        // Logout aborts the fetch; Playwright may report the already-closed route.
        assert.match(error.message, /closed|handled|aborted|Invalid InterceptionId/i);
      });
      await page.evaluate(() => refreshPeerMessages());
      assert.equal(await page.locator('#peer-message-input').inputValue(), '');
      assert.equal(await page.locator('#peer-messages .pending').count(), 0);
      assert.equal(await page.locator('#peer-messages .peer-message-body').count(), 0);
      assert.equal(await page.evaluate(() => [...peerDrafts.keys()].every(key => key.includes(':eve:'))), true);
      assert.equal(await page.evaluate(() => [...peerDrafts.values()].every(draft => !draft.value && !draft.pending && !draft.confirmed.length)), true);
      await f.close();
      console.log('PASS logout clears pending contact sends and drafts; late receipts cannot enter another account');
    }
  } finally {
    await browser.close();
    await new Promise(resolve => server.close(resolve));
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
