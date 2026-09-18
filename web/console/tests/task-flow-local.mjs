// Local, serial browser acceptance. No additional CI job or downloaded browser.
import { chromium } from '@playwright/test';
import { spawn } from 'node:child_process';
import { mkdtemp, rm, writeFile, realpath } from 'node:fs/promises';
import { resolve, join } from 'node:path';
import { tmpdir } from 'node:os';
import assert from 'node:assert/strict';
const cli = process.env.INTO_MD_CLI;
if (!cli) throw new Error('Set INTO_MD_CLI to the candidate binary');
const root = resolve(import.meta.dirname, '../../..');
const data = await mkdtemp(join(await realpath(tmpdir()), 'into-md-task-flow-'));
const server = spawn(cli, ['ui', '--no-open', '--no-config', '--data-dir', data], { stdio: ['ignore', 'pipe', 'pipe'] });
server.stderr.resume();
let browser; let page; const errors = [];
let releaseUpload;
try {
  const launch = await new Promise((resolve, reject) => {
    let output = '';
    const timer = setTimeout(() => reject(new Error('Service startup timed out')), 30000);
    server.stdout.on('data', chunk => {
      output += chunk;
      const line = output.split(/\r?\n/).find(line => line.startsWith('open this private session URL: '));
      if (line) { clearTimeout(timer); resolve(line.slice('open this private session URL: '.length)); }
    });
    server.once('exit', () => { clearTimeout(timer); reject(new Error('Service exited')); });
  });
  browser = await chromium.launch({ channel: 'msedge', headless: true, args: ['--js-flags=--max-old-space-size=512'] });
  page = await browser.newPage();
  page.on('pageerror', error => errors.push(error.message));
  const received = new Map();
  let lostCreate = false, lostRead = false;
  const simulateLostResponses = process.env.INTO_MD_LOST_RESPONSES === '1';
  let held = false;
  const hold = new Promise(resolve => { releaseUpload = resolve; });
  await page.route('**/api/uploads/*', async route => {
    const request = route.request();
    if (request.method() === 'POST') {
      received.set(request.url(), Buffer.from(request.headers()['x-into-md-filename-b64'], 'base64url').toString());
      if (simulateLostResponses && !lostCreate) {
        lostCreate = true;
        await route.fetch();
        await route.abort('failed');
        return;
      }
    }
    if (simulateLostResponses && request.method() === 'GET' && !lostRead) {
      lostRead = true;
      await route.abort('failed');
      return;
    }
    if (request.method() === 'PUT' && received.get(request.url())?.endsWith('.pptx')) { held = true; await hold; }
    await route.continue();
  });
  try { await page.goto(launch); } catch { throw new Error('Private console navigation failed'); }
  await page.locator('input[type="file"]:not([webkitdirectory])').setInputFiles([
    process.env.INTO_MD_LARGE_PPTX ?? join(root, 'fixtures/small/pptx/normal.pptx'),
    join(root, 'fixtures/small/xlsx/normal.xlsx'),
  ]);
  await page.locator('.convert-button').click();
  const small = page.locator('.current-task-link').filter({ hasText: 'normal.xlsx' });
  await page.waitForFunction(() => document.querySelectorAll('.current-batch-scroll li').length === 2);
  await new Promise(resolve => setTimeout(resolve, simulateLostResponses ? 1500 : 300));
  assert.ok(held, 'PowerPoint transport is deliberately held');
  assert.equal(received.size, 1, 'Only the first file is admitted while its transfer is held');
  await page.locator('.primary-nav a[href="/meetings"]').click();
  releaseUpload();
  await page.locator('.primary-nav a[href="/workbench"]').click();
  await page.locator('.current-task-link').filter({ hasText: '.pptx' }).waitFor({ timeout: 60000 });
  await small.waitFor({ timeout: 60000 });
  if (simulateLostResponses) assert.ok(lostCreate && lostRead, 'Lost responses recover without manual retry');
  await small.click();
  await page.locator('.markdown-preview').waitFor();
  await page.locator('.result-close').click();
  await page.reload();
  await small.waitFor();
  assert.equal(await page.locator('.current-task-link').count(), 2, 'Accepted tasks recover after refresh');
  const receiptCount = received.size;
  await page.locator('.upload-card').evaluate(zone => {
    const data = new DataTransfer();
    data.items.add(new File(['clipboard file'], '剪贴板文件.txt', { type: 'text/plain' }));
    data.items.add(new File([new Uint8Array([137, 80, 78, 71])], '截图.png', { type: 'image/png' }));
    zone.dispatchEvent(new ClipboardEvent('paste', { bubbles: true, cancelable: true, clipboardData: data }));
  });
  await page.getByRole('button', { name: '移除 剪贴板文件.txt', exact: true }).waitFor();
  assert.equal(received.size, receiptCount, 'Pasted files stay staged until submission');
  await page.getByRole('button', { name: '移除 剪贴板文件.txt', exact: true }).click();
  await page.getByRole('button', { name: '移除 截图.png', exact: true }).click();
  const batchCount = Number(process.env.INTO_MD_BATCH_COUNT ?? 0);
  if (batchCount > 0) {
    assert.ok(batchCount >= 100 && batchCount <= 200);
    await page.locator('input[type="file"]:not([webkitdirectory])').setInputFiles(Array.from({ length: batchCount }, (_, index) => ({ name: `batch-${String(index).padStart(4, '0')}.txt`, mimeType: 'text/plain', buffer: Buffer.from(`# Batch item ${index}\nIndependent result.`) })));
    await page.locator('.convert-button').click();
    await page.waitForFunction(() => document.querySelectorAll('.current-batch-scroll li.succeeded').length === 100, null, { timeout: 300000 });
    await page.getByRole('button', { name: '下一页任务', exact: true }).click();
    const remaining = batchCount + 2 - 100;
    await page.waitForFunction(count => document.querySelectorAll('.current-batch-scroll li.succeeded').length === count, remaining, { timeout: 300000 });
    await page.locator('.current-task-link').last().click();
    await page.waitForFunction(count => document.querySelectorAll('.batch-select option').length === count, batchCount);
    await page.locator('.result-close').click();
    await page.getByRole('button', { name: '上一页任务', exact: true }).click();
  }
  const previewMs = [];
  for (let index = 0; index < 20; index++) {
    const start = performance.now(); await small.click(); await page.locator('.markdown-preview').waitFor();
    previewMs.push(performance.now() - start); await page.locator('.result-close').click();
  }
  const statusMs = await page.evaluate(async () => {
    const headers = { 'X-Into-Md-Session': sessionStorage.getItem('into-md.session') };
    const historyResponse = await fetch('/api/tasks?limit=100', { headers, credentials: 'omit', redirect: 'error', referrerPolicy: 'no-referrer' });
    const history = await historyResponse.json();
    if (!historyResponse.ok) throw new Error(`History request failed: ${historyResponse.status} ${history.error?.code ?? history.code ?? ''}`);
    const ids = history.tasks.map(task => task.id); const durations = [];
    for (let index = 0; index < 30; index++) {
      const start = performance.now(); const response = await fetch('/api/tasks/summaries', { method: 'POST', headers: { ...headers, 'Content-Type': 'application/json' }, body: JSON.stringify({ ids }) });
      if (!response.ok) throw new Error('Summary request failed'); await response.json(); durations.push(performance.now() - start);
    }
    return durations;
  });
  const p95 = samples => [...samples].sort((a, b) => a - b)[Math.ceil(samples.length * .95) - 1];
  const result = { fixture: process.env.INTO_MD_LARGE_PPTX ? 'padded PPTX + repository XLSX' : 'repository PPTX + XLSX', batchCount, previewP95Ms: p95(previewMs), summaryP95Ms: p95(statusMs), browserErrors: errors };
  await writeFile(join(root, 'target/web-browser-metrics.json'), JSON.stringify(result, null, 2));
  assert.deepEqual(errors, []); assert.ok(result.previewP95Ms <= 1000); assert.ok(result.summaryP95Ms <= 200);
  console.log(JSON.stringify(result));
} catch (error) {
  console.error({ browserErrors: errors, body: (await page?.locator("body").innerText())?.slice(0, 2000) });
  await page?.screenshot({ path: join(root, "target/web-browser-failure.png") });
  throw error;
} finally {
  releaseUpload?.(); await browser?.close(); server.kill('SIGTERM');
  await new Promise(resolve => { if (server.exitCode !== null) resolve(); else { server.once('exit', resolve); setTimeout(() => { server.kill('SIGKILL'); resolve(); }, 5000).unref(); } });
  await rm(data, { recursive: true, force: true });
}
