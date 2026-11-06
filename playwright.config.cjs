const { defineConfig } = require('@playwright/test');
module.exports = defineConfig({
  testDir: './hub/web',
  testMatch: 'client.visual.spec.cjs',
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 1 : 0,
  reporter: [['list'], ['html', { open: 'never' }]],
  use: { browserName: 'chromium', headless: true, trace: 'retain-on-failure' },
  expect: { toHaveScreenshot: { animations: 'disabled', maxDiffPixelRatio: 0.01 } },
});
