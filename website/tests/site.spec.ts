import { test, expect } from '@playwright/test';
import { readdirSync } from 'node:fs';

const paths = readdirSync('build', { recursive: true })
  .filter((entry): entry is string => typeof entry === 'string' && (entry === 'index.html' || entry.endsWith('/index.html')))
  .map((entry) => '/' + entry.replace(/index\.html$/, ''));

test('every published page has content, one heading, canonical metadata, a PNG share image and a Markdown twin', async ({ page, request }) => {
  expect(paths.length).toBeGreaterThan(10);
  const titles = new Set<string>();
  for (const path of paths) {
    const response = await page.goto(path);
    expect(response?.status(), path).toBe(200);
    await expect(page.locator('main')).toHaveCount(1);
    await expect(page.locator('h1')).toHaveCount(1);
    const title = await page.title();
    expect(titles.has(title), title).toBe(false);
    titles.add(title);
    const canonical = await page.locator('link[rel="canonical"]').getAttribute('href');
    expect(new URL(canonical!).pathname).toBe(path);
    await expect(page.locator('meta[name="description"]')).toHaveAttribute('content', /\S.{30}/);
    await expect(page.locator('meta[name="twitter:card"]')).toHaveAttribute('content', 'summary_large_image');
    const og = await page.locator('meta[property="og:image"]').getAttribute('content');
    const image = await request.get(new URL(og!).pathname);
    expect(image.ok(), og!).toBe(true);
    const png = await image.body();
    expect(png.subarray(1, 4).toString()).toBe('PNG');
    expect(png.readUInt32BE(16)).toBe(1200);
    expect(png.readUInt32BE(20)).toBe(630);
    const markdown = await request.get(`${path}index.md`);
    expect(markdown.ok()).toBe(true);
    expect((await markdown.text()).length).toBeGreaterThan(100);
    const structured = await page.locator('script[type="application/ld+json"]').allTextContents();
    expect(structured.length).toBeGreaterThan(0);
    for (const json of structured) expect(JSON.parse(json)['@context']).toBe('https://schema.org');
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth);
    expect(overflow, path).toBe(false);
  }
  expect((await request.get('/robots.txt')).ok()).toBe(true);
  const sitemap = await (await request.get('/sitemap.xml')).text();
  expect((sitemap.match(/<url>/g) ?? []).length).toBe(paths.length);
  expect((await (await request.get('/llms.txt')).text())).toContain('Hokan');
  expect((await (await request.get('/llms-full.txt')).text())).toContain('hokan doctor');
});

test('landing interactions preserve review and copy the complete installation command', async ({ page, context }, info) => {
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await page.emulateMedia({ colorScheme: 'light' });
  await page.goto('/');
  await page.screenshot({ path: info.outputPath('home-light.png'), fullPage: true });
  await page.getByRole('button', { name: 'Project', exact: true }).click();
  await expect(page.locator('.completion-box')).toContainText('pnpm build');
  await page.getByRole('button', { name: 'Insert pnpm build in this demo', exact: true }).click();
  await expect(page.locator('.demo-status')).toContainText('Nothing executed.');
  await expect(page.locator('.terminal-command')).toContainText('pnpm build');
  await page.getByRole('button', { name: 'History', exact: true }).click();
  await expect(page.locator('.completion-box')).toContainText('cargo test --lib');
  await page.getByRole('button', { name: 'Copy install command' }).click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toContain('HOKAN_VERSION=0.1.0-beta.10 sh');
  await page.getByRole('button', { name: 'Switch to dark theme' }).click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  await page.screenshot({ path: info.outputPath('home-dark.png'), fullPage: true });
  await page.reload();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  expect(errors).toEqual([]);
});

test('documentation search, code copy and anchors work', async ({ page, context }, info) => {
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await page.goto('/docs/getting-started/install/');
  await expect(page.locator('.sd-sidebar').getByRole('link', { name: 'Install Hokan', exact: true })).toHaveAttribute('aria-current', 'page');
  await expect(page.locator('.sd-sidebar').getByRole('link', { name: 'Your first shell session', exact: true })).toBeVisible();
  await expect(page.locator('.sd-sidebar a[href="/docs/reference/configuration"]')).toBeVisible();
  await page.screenshot({ path: info.outputPath('docs-desktop.png'), fullPage: true });
  await page.getByRole('button', { name: 'Search documentation', exact: true }).first().click();
  await page.getByRole('combobox').fill('nerd_fonts');
  await expect(page.getByRole('option').first()).toBeVisible();
  await page.getByRole('option').first().click();
  await expect(page.locator('main')).toContainText('nerd_fonts');
  await page.goto('/docs/getting-started/install/#on-demand-mode');
  await expect(page.locator('#on-demand-mode')).toBeInViewport();
  await page.locator('.sd-code-copy').first().click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toContain('hokan-installer.sh');
});

test('mobile navigation, docs and reduced motion fit the screen', async ({ page }, info) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.emulateMedia({ reducedMotion: 'reduce', colorScheme: 'light' });
  await page.goto('/');
  await page.screenshot({ path: info.outputPath('home-mobile.png'), fullPage: true });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.getByRole('button', { name: 'Open menu' }).click();
  await page.getByRole('link', { name: 'Guides', exact: true }).click();
  await expect(page).toHaveURL(/\/docs\/getting-started\/install\/?$/);
  await expect(page.getByRole('heading', { name: 'Install Hokan', exact: true })).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.screenshot({ path: info.outputPath('docs-mobile.png'), fullPage: true });
});

test('missing routes return a real 404 and the static error page is not indexed', async ({ page, request }) => {
  const missing = await page.goto('/this-page-does-not-exist/');
  expect(missing?.status()).toBe(404);
  const fallback = await request.get('/404.html');
  expect(fallback.ok()).toBe(true);
  expect(await fallback.text()).toContain('name="robots" content="noindex"');
});
