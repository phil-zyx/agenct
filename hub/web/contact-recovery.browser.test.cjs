/* Real Chromium checks for colleague directory (no private peer chat window) and @mention in main chat. */
const assert = require('node:assert/strict');
const { chromium } = require('playwright');
const { fixture, server } = require('./client.browser.test.cjs');

async function contactFixture(browser, origin) {
  const f = await fixture(browser, origin);
  const page = f.page;
  await page.setViewportSize({ width: 1440, height: 900 });
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
  return {
    ...f,
    async directory() {
      await page.locator('#open-contacts').click();
      await page.locator('.contact-row[data-contact="bob"]').waitFor();
    },
    async open(id) {
      await page.locator(`.contact-row[data-contact="${id}"]`).click();
      await page.locator('#contacts-view:not(.hide)').waitFor();
    }
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
      await f.directory();
      await f.open('bob');
      // Private peer chat panel stays hidden; contacts view shows collaboration profile and @TA action
      assert.equal(await page.locator('#peer-view').isVisible(), false);
      assert.match(await page.locator('#contacts-view').textContent(), /接口设计/);
      await page.getByRole('button', { name: '在对话中 @TA', exact: true }).click();
      assert.equal(await page.locator('#text').inputValue(), '@张明 ');
      await f.close();
      console.log('PASS contacts directory shows collaboration profile without private chat window and inserts @mention into main chat');
    }
    {
      const f = await contactFixture(browser, origin), page = f.page;
      await page.locator('#text').fill('给@张');
      await page.locator('#mention-picker:not(.hide)').waitFor();
      assert.match(await page.locator('#mention-picker').textContent(), /@张明/);
      await page.locator('#text').press('Enter');
      assert.equal(await page.locator('#text').inputValue(), '给@张明 ');
      assert.equal(await page.locator('#mention-picker').isVisible(), false);

      await page.locator('#rail-more').click();
      await page.locator('#logout').click();
      assert.equal(await page.locator('#mention-picker').isVisible(), false);
      await f.close();
      console.log('PASS main chat @mention autocomplete filters colleagues, completes on Enter, and clears on logout');
    }
  } finally {
    await browser.close();
    server.close();
  }
})().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
