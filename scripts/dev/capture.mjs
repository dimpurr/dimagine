// scripts/dev/capture.mjs — headless Chrome screenshots and layout checks.
//
// `--window-size` cannot go below 500 CSS pixels in headless Chrome: it lays
// the page out at 500 and clips the capture to the requested width, so a
// "390px" screenshot is really a 500px layout cropped. This helper drives
// Chrome over the DevTools Protocol instead and calls
// `Emulation.setDeviceMetricsOverride`, which sets the viewport the page
// actually lays out in. The same page is then measured: a `check` job fails
// when `document.documentElement.scrollWidth` or `window.innerWidth`
// exceeds the emulated device width — mobile shrink-to-fit inflates the
// two together (600px content on a 390px device measures 608/608), so
// comparing them to each other alone would pass pages wider than the device.
//
// Usage: node capture.mjs <jobs.json>
//
// jobs.json:
//   {
//     "chrome": "/path/to/chrome",
//     "jobs": [
//       { "url": "...", "width": 390, "height": 844,
//         "theme": "light"|"dark", "out": "/path/shot.png", "check": true }
//     ]
//   }
//
// Exit codes: 0 every shot written and every check passed; 1 a shot failed or
// a check found horizontal overflow; 2 usage error; 130/143/129 the process
// was killed by SIGINT/SIGTERM/SIGHUP after Chrome and its profile dir were
// torn down.

import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

if (process.argv.length !== 3) {
  console.error('usage: node capture.mjs <jobs.json>');
  process.exit(2);
}

const spec = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const chromeBin = spec.chrome;
const jobs = spec.jobs || [];
if (!chromeBin || !jobs.length) {
  console.error('capture: jobs.json needs "chrome" and a non-empty "jobs"');
  process.exit(2);
}

const profile = mkdtempSync(join(tmpdir(), 'dimagine-capture-'));
// No `detached: true`: Chrome stays in node's process group so it dies
// with node wherever the platform propagates signals to the group.
const chrome = spawn(
  chromeBin,
  [
    '--headless=new',
    '--disable-gpu',
    '--hide-scrollbars',
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-extensions',
    '--remote-debugging-port=0',
    `--user-data-dir=${profile}`,
    'about:blank',
  ],
  { stdio: ['ignore', 'ignore', 'pipe'] }
);
// The pid and profile let callers record which Chrome to expect.
console.error(`capture: chrome pid ${chrome.pid} profile ${profile}`);

let stderr = '';
chrome.stderr.on('data', (chunk) => {
  stderr += chunk.toString();
});

function killChrome() {
  try {
    chrome.kill('SIGKILL');
  } catch {
    // already gone
  }
}

const EXIT_WAIT_MS = 2000;
const PROFILE_QUIET_MS = 500;
const PROFILE_RETRY_DEADLINE_MS = 5000;

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// SIGKILL is asynchronous and Chrome's helpers can outlive the main
// process briefly: some are still writing under the profile when the
// first rmSync runs (it fails part-way then) and others recreate files
// right after a successful removal. So teardown waits for the child's
// exit event, then keeps removing until the profile not only is gone
// but stays gone across a quiet interval (bounded, so this never hangs
// — at worst it is still the old best effort); only after that may the
// caller exit.
async function stopChrome() {
  killChrome();
  if (chrome.exitCode === null && chrome.signalCode === null) {
    await Promise.race([
      new Promise((resolve) => chrome.once('exit', resolve)),
      sleep(EXIT_WAIT_MS),
    ]);
  }
  const deadline = Date.now() + PROFILE_RETRY_DEADLINE_MS;
  do {
    try {
      rmSync(profile, { recursive: true, force: true });
    } catch {
      // a not-yet-dead helper is still writing under the profile
    }
    await sleep(PROFILE_QUIET_MS);
    if (!existsSync(profile)) {
      return; // gone, and nothing recreated it within the quiet interval
    }
  } while (Date.now() < deadline);
}

// A node process killed by a signal (a timeout, Ctrl-C, a closed tmux
// pane) would otherwise leave Chrome running with no parent to clean it
// up, so every terminating signal tears Chrome and its profile down
// first and exits with the conventional 128+signal code.
const SIGNAL_EXIT_CODES = { SIGINT: 130, SIGTERM: 143, SIGHUP: 129 };
let signalShutdown = false;
for (const [signal, exitCode] of Object.entries(SIGNAL_EXIT_CODES)) {
  process.on(signal, () => {
    if (signalShutdown) {
      return;
    }
    signalShutdown = true;
    // stopChrome() resolves only once the profile is gone (or fully
    // retried); exiting before that would race the removal.
    stopChrome().then(() => process.exit(exitCode));
  });
}

async function browserPort() {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    const match = stderr.match(/DevTools listening on ws:\/\/127\.0\.0\.1:(\d+)\//);
    if (match) return Number(match[1]);
    if (chrome.exitCode !== null) {
      throw new Error(`Chrome exited early (${chrome.exitCode}): ${stderr}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`Chrome never reported a debugging port: ${stderr}`);
}

class Cdp {
  constructor(socket) {
    this.socket = socket;
    this.nextId = 0;
    this.pending = new Map();
    this.listeners = new Map();
    socket.addEventListener('message', (event) => {
      const message = JSON.parse(event.data);
      if (message.id && this.pending.has(message.id)) {
        const { resolve, reject } = this.pending.get(message.id);
        this.pending.delete(message.id);
        if (message.error) reject(new Error(JSON.stringify(message.error)));
        else resolve(message.result);
        return;
      }
      if (message.method) {
        const handlers = this.listeners.get(message.method) || [];
        for (const handler of handlers) handler(message.params);
      }
    });
  }

  send(method, params = {}) {
    const id = (this.nextId += 1);
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.socket.send(JSON.stringify({ id, method, params }));
    });
  }

  once(method) {
    return new Promise((resolve) => {
      const handler = (params) => {
        this.listeners.set(
          method,
          (this.listeners.get(method) || []).filter((entry) => entry !== handler)
        );
        resolve(params);
      };
      this.listeners.set(method, [...(this.listeners.get(method) || []), handler]);
    });
  }
}

async function main() {
  const port = await browserPort();
  const target = await (
    await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: 'PUT' })
  ).json();
  const socket = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve);
    socket.addEventListener('error', reject);
  });
  const cdp = new Cdp(socket);
  await cdp.send('Page.enable');
  await cdp.send('Runtime.enable');

  const failures = [];
  let shotCount = 0;

  for (const job of jobs) {
    const width = Number(job.width);
    const height = Number(job.height);
    const theme = job.theme === 'dark' ? 'dark' : 'light';
    await cdp.send('Emulation.setDeviceMetricsOverride', {
      width,
      height,
      deviceScaleFactor: 1,
      mobile: width < 768,
    });
    await cdp.send('Emulation.setEmulatedMedia', {
      features: [{ name: 'prefers-color-scheme', value: theme }],
    });

    const loaded = cdp.once('Page.loadEventFired');
    await cdp.send('Page.navigate', { url: job.url });
    await Promise.race([
      loaded,
      new Promise((resolve) => setTimeout(resolve, 15000)),
    ]);
    // Let the deferred script and the first layout settle.
    await new Promise((resolve) => setTimeout(resolve, 150));

    if (job.check) {
      const result = await cdp.send('Runtime.evaluate', {
        expression:
          'JSON.stringify({ inner: window.innerWidth,' +
          ' scroll: document.documentElement.scrollWidth,' +
          ' body: document.body.scrollWidth })',
        returnByValue: true,
      });
      const metrics = JSON.parse(result.result.value);
      // Failing either test means the page overflows: scroll wider than
      // inner catches classic overflow, and inner or scroll wider than the
      // emulated device width catches mobile shrink-to-fit inflating both.
      const overflow =
        metrics.scroll > metrics.inner ||
        metrics.inner > width ||
        metrics.scroll > width;
      const label = `${width}px ${job.url}`;
      console.log(
        `CHECK ${label} inner=${metrics.inner} scroll=${metrics.scroll} ` +
          `body=${metrics.body} ${overflow ? 'OVERFLOW' : 'ok'}`
      );
      if (overflow) failures.push(label);
    }

    if (job.out) {
      const shot = await cdp.send('Page.captureScreenshot', { format: 'png' });
      writeFileSync(job.out, Buffer.from(shot.data, 'base64'));
      shotCount += 1;
      console.log(`SHOT ${job.out}`);
    }
  }

  socket.close();
  if (failures.length) {
    console.error(`capture: horizontal overflow at ${failures.join(', ')}`);
    return 1;
  }
  console.log(`capture: ${shotCount} screenshots written, no overflow`);
  return 0;
}

async function run() {
  let code = 0;
  try {
    code = await main();
  } catch (error) {
    console.error(`capture: ${error.message}`);
    code = 1;
  }
  await stopChrome();
  process.exit(code);
}

// If the teardown itself ever rejects, still end the process.
run().catch(() => process.exit(1));
