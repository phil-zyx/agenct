/* Regression scenarios from the UX audit; isolated API fixtures, no provider calls. */
const assert = require('node:assert/strict');
const { chromium } = require('playwright');
const { fixture, server, showTasks, taskFixture } = require('./client.browser.test.cjs');
(async () => {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const f = await fixture(browser, origin, true);
    const page = f.page;
    await page.setViewportSize({ width: 1440, height: 900 });
    let profile = { revision: 1, agents: [{ id: 'butler', name: '我的管家', role: 'coordinator', model_ref: 'server-default', enabled: true }] };
    await page.route('**/api/profile', async route => {
      if (route.request().method() === 'PUT') profile = { ...route.request().postDataJSON(), revision: profile.revision + 1 };
      await route.fulfill({ json: profile });
    });
    await page.route('**/api/profile/catalog', route => route.fulfill({ json: { roles: ['coordinator', 'researcher'], models: [{ id: 'server-default' }], max_agents: 8 } }));
    await page.route('**/api/contacts/*/profile', route => route.fulfill({ json: { public: {} } }));
    await page.route('**/api/contacts', route => route.fulfill({ json: ['bob', 'carol'] }));
    await page.route('**/api/people', route => route.fulfill({ json: [{ id: 'bob', display_name: '张明' }, { id: 'carol', display_name: '李晓' }] }));
    await page.locator('#open-contacts').click();
    await page.getByRole('button', { name: /张明/ }).click();
    if (page.viewportSize().width > 1100) await page.getByRole('button', { name: '请他帮忙', exact: true }).click();
    await page.getByLabel('委派摘要').fill('给张明的未提交摘要');
    await page.getByRole('button', { name: /李晓/ }).click();
    await page.getByRole('button', { name: '请他帮忙', exact: true }).click();
    assert.equal(await page.getByLabel('委派摘要').inputValue(), '');
    await page.getByLabel('委派摘要').fill('给李晓的摘要');
    await page.keyboard.press('Escape');
    await page.locator('#open-contacts').click();
    await page.getByRole('button', { name: /张明/ }).click();
    if (page.viewportSize().width > 1100) await page.getByRole('button', { name: '请他帮忙', exact: true }).click();
    assert.equal(await page.getByLabel('委派摘要').inputValue(), '给张明的未提交摘要');
    await page.locator('#open-agents').click();
    await page.getByLabel('名称', { exact: true }).fill('尚未保存的名字');
    await page.locator('#open-chat').click();
    await page.locator('#open-agents').click();
    assert.equal(await page.getByLabel('名称', { exact: true }).inputValue(), '尚未保存的名字');
    await page.getByRole('button', { name: '检查更改', exact: true }).click();
    await page.getByRole('button', { name: '确认保存个人配置', exact: true }).click();
    await page.getByText('已保存个人配置', { exact: false }).waitFor();
    await page.locator('#open-chat').click();
    await page.locator('#open-agents').click();
    assert.equal(await page.getByLabel('名称', { exact: true }).inputValue(), '尚未保存的名字');
    await page.getByLabel('名称', { exact: true }).fill('应该放弃的名字');
    await page.getByRole('button', { name: '取消，不保存', exact: true }).click();
    await page.locator('#open-agents').click();
    assert.equal(await page.getByLabel('名称', { exact: true }).inputValue(), '尚未保存的名字');
    console.log('PASS contact/role drafts survive navigation; explicit cancel discards; save persists');

    await page.setViewportSize({ width: 390, height: 844 });
    await showTasks(f, [taskFixture()]);
    await page.waitForFunction(() => document.querySelector('.chat-pane').inert);
    await page.locator('#close-tasks').focus();
    for (let i = 0; i < 28; i++) {
      await page.keyboard.press('Tab');
      assert.equal(await page.evaluate(() => document.querySelector('.chat-pane').contains(document.activeElement)), false);
    }
    await page.keyboard.press('Escape');
    await page.waitForFunction(() => !document.querySelector('.chat-pane').inert);
    await page.locator('#open-agents').click();
    await page.getByRole('button', { name: '检查更改', exact: true }).focus();
    for (let i = 0; i < 18; i++) {
      await page.keyboard.press('Tab');
      assert.equal(await page.evaluate(() => document.querySelector('.chat-pane').contains(document.activeElement)), false);
    }
    await page.locator('#open-chat').click();
    assert.equal(await page.evaluate(() => document.activeElement.id), 'text');
    await page.locator('#open-contacts').click();
    await page.getByRole('button', { name: /张明/ }).click();
    if (page.viewportSize().width > 1100) await page.getByRole('button', { name: '请他帮忙', exact: true }).click();
    await page.locator('#peer-details').click();
    await page.keyboard.press('Escape');
    assert.equal(await page.evaluate(() => document.activeElement.id), 'open-contacts');
    console.log('PASS mobile overlays isolate background focus; Escape/chat restore focus');

    let offline = true;
    await page.route('**/api/tasks', route => offline ? route.abort('failed') : route.fulfill({ json: f.state.tasks }));
    await page.locator('#refresh').evaluate(button => button.click());
    await page.locator('#sync-error:not(.hide)').waitFor();
    await page.locator('#toggle-tasks').click();
    await page.waitForFunction(() => document.querySelector('.task-pane').getBoundingClientRect().top >= document.getElementById('sync-error').getBoundingClientRect().bottom);
    assert.equal(await page.getByRole('button', { name: '重试同步' }).isVisible(), true);
    await page.screenshot({ path: '/tmp/agenct-harden-mobile-offline.png' });
    offline = false;
    await page.getByRole('button', { name: '重试同步' }).click();
    await page.locator('#sync-error').waitFor({ state: 'hidden' });
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.waitForFunction(() => !document.querySelector('#peer-view').inert);
    console.log('PASS mobile offline banner persists above panels; retry clears it; desktop chat remains usable');
    assert.equal(await page.locator('.float-pill').count(), 0);
    assert.equal(await page.locator('#refresh').isVisible(), false);
    await page.route('**/api/me', route => route.fulfill({ json: { user: 'alice', runtime: 'mock', member: { id: 'alice', display_name: 'Alice', color: 'green', status: 'active', revision: 0, admin: false } } }));
    await page.locator('#open-account').click();
    await page.getByRole('heading', { name: '会话与通知' }).waitFor();
    assert.equal(await page.locator('#enable-notifications').isVisible(), true);
    await page.getByRole('button', { name: '刷新会话与任务', exact: true }).click();
    await page.locator('#open-chat').click();
    assert.equal(await page.locator('#refresh').isVisible(), false);
    await page.screenshot({ path: '/tmp/agenct-clean-chat.png' });
    await page.locator('#open-account').click();
    await page.getByRole('heading', { name: '会话与通知' }).waitFor();
    assert.equal(await page.locator('#refresh').count(), 1);
    console.log('PASS chat has no utility pills; account retains usable refresh/notification controls across reopening');
    await page.locator('#logout').click();
    assert.equal(await page.evaluate(() => contactDrafts.size), 0);
    assert.equal(await page.evaluate(() => profileUI.hasUnsaved()), false);
    console.log('PASS logout clears private form drafts');
    await f.close();
  } finally { await browser.close(); await new Promise(resolve => server.close(resolve)); }
})().catch(error => { console.error(error); process.exitCode = 1; });
