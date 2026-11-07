/* Run inside an isolated X/DBus session with Orca; see check-screen-reader.py. */
const { chromium } = require('playwright');
const { fixture, server } = require('../hub/web/client.browser.test.cjs');
(async () => {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const browser = await chromium.launch({ headless: false, args: ['--force-renderer-accessibility'] });
  try {
    const f = await fixture(browser, `http://127.0.0.1:${server.address().port}`);
    await f.page.waitForTimeout(3000); // Allow AT-SPI and Orca to discover this document.
    await f.page.locator('#onboarding-skip').click();
    await f.page.locator('#text').focus();
    await f.page.keyboard.type('Local accessibility check');
    for (let i = 0; i < 4; i++) {
      await f.page.keyboard.press('Tab');
      await f.page.waitForTimeout(600);
    }
    await f.page.locator('#text').focus();
    await f.page.keyboard.press('Control+Shift+K');
    await f.page.waitForTimeout(800);
    await f.page.keyboard.press('Escape');
    await f.page.waitForTimeout(800);
    await f.close();
  } finally { await browser.close(); server.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; server.close(); });
