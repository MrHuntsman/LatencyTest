/* End-to-end smoke test for the latency test site.
 * Uses headless Chrome with --use-fake-device-for-media-stream so
 * getUserMedia resolves. Sensor measurement path is driven synthetically
 * via the window.__sensorDebug hook. */
const puppeteer = require('puppeteer-core');
const http = require('http');
const fs = require('fs');
const path = require('path');

const CHROME_PATHS = [
  'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
  'C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe',
];
const ROOT = path.join(__dirname, '..');
const PORT = 8931;

const MIME = { '.html': 'text/html', '.css': 'text/css', '.js': 'text/javascript' };

function serve() {
  return new Promise((resolve) => {
    const srv = http.createServer((req, res) => {
      const p = path.join(ROOT, req.url === '/' ? 'index.html' : req.url.split('?')[0]);
      fs.readFile(p, (err, data) => {
        if (err) { res.writeHead(404); res.end('nf'); return; }
        res.writeHead(200, { 'Content-Type': MIME[path.extname(p)] || 'application/octet-stream' });
        res.end(data);
      });
    });
    srv.listen(PORT, '127.0.0.1', () => resolve(srv));
  });
}

let failures = 0;
function check(name, cond, detail) {
  console.log((cond ? 'PASS' : 'FAIL') + '  ' + name + (detail ? '  [' + detail + ']' : ''));
  if (!cond) failures++;
}

(async () => {
  const srv = await serve();
  const browser = await puppeteer.launch({
    executablePath: CHROME_PATHS.find((p) => fs.existsSync(p)),
    headless: 'new',
    args: [
      '--use-fake-device-for-media-stream',
      '--use-fake-ui-for-media-stream',
      '--autoplay-policy=no-user-gesture-required',
      '--no-sandbox',
    ],
  });

  // ---------- index ----------
  {
    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', (e) => errors.push(String(e)));
    await page.goto(`http://127.0.0.1:${PORT}/index.html`, { waitUntil: 'load' });
    check('index: title present', (await page.title()).includes('System Latency'));
    check('index: role cards present', (await page.$$('.role-card')).length === 2);
    check('index: no JS errors', errors.length === 0, errors.join(' | ') || 'clean');
    await page.close();
  }

  // ---------- emitter ----------
  {
    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', (e) => errors.push(String(e)));
    await page.goto(`http://127.0.0.1:${PORT}/emitter.html`, { waitUntil: 'load' });
    const layer = await page.$('#flashLayer');
    check('emitter: flash layer exists', !!layer);

    const before = await page.$eval('#flashLayer', (el) => el.classList.contains('white'));
    await page.mouse.down(); await page.mouse.up();
    await new Promise((r) => setTimeout(r, 40));
    const during = await page.$eval('#flashLayer', (el) => el.classList.contains('white'));
    check('emitter: click triggers white flash', !before && during);

    await new Promise((r) => setTimeout(r, 150));
    const after = await page.$eval('#flashLayer', (el) => el.classList.contains('white'));
    check('emitter: flash turns off after ~90ms', !after);
    const stat = await page.$eval('#paintStat', (el) => el.textContent);
    check('emitter: click→paint HUD updates', /click→paint: [\d.]+ ms/.test(stat), stat);
    check('emitter: no JS errors', errors.length === 0, errors.join(' | ') || 'clean');
    await page.close();
  }

  // ---------- sensor ----------
  {
    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', (e) => errors.push(String(e)));
    page.on('console', (m) => { if (m.type() === 'warning') errors.push('console: ' + m.text()); });
    await page.goto(`http://127.0.0.1:${PORT}/sensor.html`, { waitUntil: 'networkidle0' });

    // Start measurement (getUserMedia resolves thanks to fake device)
    await page.click('#startBtn');
    await new Promise((r) => setTimeout(r, 1500));
    const status = await page.$eval('#status', (el) => el.textContent);
    check('sensor: start succeeds with fake cam+mic', status.includes('Measuring'), status);

    const dbg = await page.evaluate(() => !!window.__sensorDebug);
    check('sensor: debug hook available', dbg);

    // Synthetic pairing: click precedes flash by 37ms
    await page.evaluate(() => {
      const d = window.__sensorDebug;
      d.injectClick(performance.now() - 37);
      d.detectFlashEdge(200, performance.now()); // big luminance rise
    });
    await new Promise((r) => setTimeout(r, 150));
    const pairs = await page.evaluate(() => window.__sensorDebug.pairs.length);
    check('sensor: click+flash edge pairs into 1 measurement', pairs === 1, 'pairs=' + pairs);
    const med = await page.$eval('#resMedian', (el) => el.textContent);
    check('sensor: median displays ~37ms (uncorrected)', Math.abs(parseFloat(med) - 37) < 2, med);
    const unc = await page.$eval('#resUnc', (el) => el.textContent);
    check('sensor: uncertainty shown', /± \d+/.test(unc), unc);
    const verdict = await page.$eval('#verdict', (el) => el.textContent);
    check('sensor: verdict rendered', verdict.length > 5, verdict);

    // Double-flash debounce: second edge within debounce window must be ignored
    await page.evaluate(() => {
      const d = window.__sensorDebug;
      d.detectFlashEdge(200, performance.now());
    });
    await new Promise((r) => setTimeout(r, 100));
    const pairsAfter = await page.evaluate(() => window.__sensorDebug.pairs.length);
    check('sensor: debounce blocks immediate second edge', pairsAfter === 1, 'pairs=' + pairsAfter);

    // New click after quiet period pairs again (luminance must fall first
    // so the next detectFlashEdge call is a genuine rising edge)
    await new Promise((r) => setTimeout(r, 600));
    await page.evaluate(() => {
      const d = window.__sensorDebug;
      d.detectFlashEdge(50, performance.now());   // screen back to dark
      d.injectClick(performance.now() - 50);
      d.detectFlashEdge(200, performance.now());  // flash rises
    });
    await new Promise((r) => setTimeout(r, 150));
    const pairs2 = await page.evaluate(() => window.__sensorDebug.pairs.length);
    check('sensor: subsequent click pairs again', pairs2 === 2, 'pairs=' + pairs2);

    // No click => no pair (dark then flash)
    await page.evaluate(() => {
      const d = window.__sensorDebug;
      d.detectFlashEdge(50, performance.now());
      d.detectFlashEdge(200, performance.now());
    });
    await new Promise((r) => setTimeout(r, 150));
    const pairs3 = await page.evaluate(() => window.__sensorDebug.pairs.length);
    check('sensor: flash without click is not paired', pairs3 === 2, 'pairs=' + pairs3);

    // Reset clears state
    await page.click('#resetBtn');
    const pairsReset = await page.evaluate(() => window.__sensorDebug.pairs.length);
    check('sensor: reset clears pairs', pairsReset === 0);

    const realErrors = errors.filter((e) => !e.includes('fake') && !e.includes('AudioWorklet'));
    check('sensor: no JS errors', realErrors.length === 0, realErrors.join(' | ') || 'clean');
    await page.close();
  }

  await browser.close();
  srv.close();
  console.log('\n' + (failures === 0 ? 'ALL TESTS PASSED' : failures + ' TEST(S) FAILED'));
  process.exit(failures === 0 ? 0 : 1);
})().catch((e) => { console.error('TEST RUNNER ERROR:', e); process.exit(2); });
