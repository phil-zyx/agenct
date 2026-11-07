/* Browser + fixture API only; this does not attest real deployment/model acceptance. */
const { test, expect } = require('@playwright/test');
const { fixture, server, taskFixture, showTasks, settled } = require('./client.browser.test.cjs');
let origin;
test.beforeAll(async () => {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  origin = `http://127.0.0.1:${server.address().port}`;
});
test.afterAll(async () => { await new Promise(resolve => server.close(resolve)); });

for (const width of [1330, 1440, 400]) {
  for (const colorScheme of ['light', 'dark']) {
    test(`chat and task layout ${width} ${colorScheme}`, async ({ browser }, testInfo) => {
      const f = await fixture(browser, origin);
      try {
        await f.page.setViewportSize({ width, height: 900 });
        await f.page.emulateMedia({ colorScheme, reducedMotion: 'reduce' });
        // Match the explicitly installed CI fonts, independent of desktop defaults.
        await f.page.addStyleTag({ content: 'body { font-family: "DejaVu Sans", "Droid Sans Fallback", sans-serif; }' });
        await f.page.locator('#open-chat').click();
        await f.send('请总结本周工作，并保留下面的列表和代码示例。');
        await settled(f);
        await expect(f.page.locator('#conversation')).toContainText('Saved reply');
        await expect(f.page.locator('#send')).toBeInViewport();
        await expect(f.page.locator('#text')).toBeInViewport();
        expect(await f.page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
        const masks = [f.page.locator('#last-refresh'), f.page.locator('time')];
        await expect(f.page).toHaveScreenshot(`chat-${width}-${colorScheme}.png`, { mask: masks });
        await f.page.screenshot({ path: testInfo.outputPath('chat.png'), mask: masks });
        await showTasks(f, [taskFixture()]);
        await expect(f.page.getByRole('button', { name: '在对话中处理', exact: true })).toBeInViewport();
        await expect(f.page).toHaveScreenshot(`tasks-${width}-${colorScheme}.png`, { mask: masks });
        await f.page.screenshot({ path: testInfo.outputPath('tasks.png'), mask: masks });
        // The delivery is accepted inside the conversation.
        await f.page.getByRole('button', { name: '在对话中处理', exact: true }).click();
        const accept = f.page.locator('#task-chat-task-alpha').getByRole('button', { name: '验收通过', exact: true });
        await expect(accept).toBeInViewport();
        await f.page.waitForTimeout(1800); // let the arrival highlight fade
        expect(await f.page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
        await expect(f.page).toHaveScreenshot(`task-chat-${width}-${colorScheme}.png`, { mask: masks });
      } finally { await f.close(); }
    });
  }
}
