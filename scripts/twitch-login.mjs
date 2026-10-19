#!/usr/bin/env node
/**
 * Interactive Twitch login helper.
 *
 * Phase 1: launch the system browser as a normal OS process with a temporary
 * profile and NO remote-debugging / Playwright flags, so Twitch login is less
 * likely to show "browser not supported".
 *
 * Phase 2: after the user confirms login (Enter), relaunch the same temporary
 * profile with remote debugging only long enough to export storageState.
 *
 * The everyday browser profile/session is never opened or modified.
 */

import { spawn } from 'node:child_process';
import fs from 'node:fs/promises';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';
import readline from 'node:readline';
import { setTimeout as delay } from 'node:timers/promises';
import { clearTimeout, setTimeout } from 'node:timers';
import { fileURLToPath } from 'node:url';

import { chromium, firefox } from 'playwright';

import {
  buildBrowserPreferenceOrder,
  detectDefaultBrowserId,
  hasTwitchAuthCookie,
  isUnsupportedDefaultBrowser,
  mapDefaultBrowserId,
  resolveCandidateExecutable,
  supportsNativeChromiumLaunch,
} from './lib/default-browser.mjs';

const DEFAULT_OUTPUT = path.join('data', 'browser-state', 'storage-state.json');
const TWITCH_URL = 'https://www.twitch.tv/';
const DEFAULT_TIMEOUT_MS = 15 * 60 * 1000;
const CDP_READY_TIMEOUT_MS = 30_000;
const BROWSER_EXIT_TIMEOUT_MS = 15_000;
const EXPORT_SETTLE_MS = 2_000;

/**
 * @typedef {import('./lib/default-browser.mjs').BrowserCandidate} BrowserCandidate
 */

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) {
    printHelp();
    return;
  }

  const outputPath = path.resolve(options.output);
  const defaultBrowserId = detectDefaultBrowserId();
  const preferenceOrder = resolvePreferenceOrder(defaultBrowserId, options.browser);

  printDetectionSummary(defaultBrowserId, preferenceOrder);

  await fs.mkdir(path.dirname(outputPath), { recursive: true, mode: 0o700 });

  const tempRoot = await fs.mkdtemp(
    path.join(os.tmpdir(), 'twitch-watchdog-login-'),
  );

  try {
    const selected = await selectLaunchableCandidate(preferenceOrder);
    const profileDir = path.join(
      tempRoot,
      sanitizeProfileName(selected.label),
    );
    await fs.mkdir(profileDir, { recursive: true });

    if (supportsNativeChromiumLaunch(selected)) {
      await runNativeChromiumLogin(selected, profileDir, outputPath, options.timeoutMs);
    } else {
      await runPlaywrightFallbackLogin(
        selected,
        profileDir,
        outputPath,
        options.timeoutMs,
      );
    }
  } finally {
    await removeDirectory(tempRoot);
  }
}

/**
 * @param {BrowserCandidate} candidate
 * @param {string} profileDir
 * @param {string} outputPath
 * @param {number} timeoutMs
 */
async function runNativeChromiumLogin(
  candidate,
  profileDir,
  outputPath,
  timeoutMs,
) {
  const executablePath = resolveCandidateExecutable(candidate);
  if (executablePath === undefined) {
    throw new Error(`Executable not found for ${candidate.label}`);
  }

  process.stdout.write(
    `\nPhase 1/2: opening ${candidate.label} with a temporary profile.\n` +
      'This uses a normal browser process (no remote debugging, no Playwright flags).\n' +
      'Your everyday browser profile and existing sessions are not opened.\n' +
      `Opened ${TWITCH_URL}\n\n` +
      'In the browser window:\n' +
      '  1. Log in to Twitch (including 2FA).\n' +
      '  2. Confirm the top-right shows you are logged in.\n' +
      '  3. If Twitch still says the browser is unsupported, try:\n' +
      '       npm run twitch:login -- --browser chrome\n' +
      '  4. Return here and press Enter to export storage state.\n\n',
  );

  const loginChild = spawnBrowser(executablePath, profileDir, {
    remoteDebuggingPort: undefined,
    url: TWITCH_URL,
  });

  try {
    await waitForEnterOrExit(loginChild, timeoutMs);
  } finally {
    await stopChild(loginChild);
  }

  process.stdout.write(
    '\nPhase 2/2: relaunching the same temporary profile briefly to export cookies...\n',
  );

  const debuggingPort = await getFreePort();
  const exportChild = spawnBrowser(executablePath, profileDir, {
    remoteDebuggingPort: debuggingPort,
    url: TWITCH_URL,
  });

  try {
    await waitForCdpReady(debuggingPort, CDP_READY_TIMEOUT_MS);
    // Give Chromium a moment to load profile cookies into the context.
    await delay(EXPORT_SETTLE_MS);

    const browser = await chromium.connectOverCDP(
      `http://127.0.0.1:${debuggingPort}`,
    );
    try {
      const context = browser.contexts()[0];
      if (context === undefined) {
        throw new Error('No browser context available over CDP for storage export');
      }

      // Prefer an existing page; open one if the browser started without one.
      const page = context.pages()[0] ?? (await context.newPage());
      if (!page.url().includes('twitch.tv')) {
        await page.goto(TWITCH_URL, { waitUntil: 'domcontentloaded' }).catch(
          () => undefined,
        );
        await delay(1_000);
      }

      await context.storageState({ path: outputPath });
    } finally {
      // Stop the OS process first; then drop the CDP client.
      await stopChild(exportChild);
      await browser.close().catch(() => undefined);
    }
  } catch (error) {
    await stopChild(exportChild);
    throw error;
  }

  await finalizeSavedState(outputPath);
}

/**
 * @param {BrowserCandidate} candidate
 * @param {string} profileDir
 * @param {string} outputPath
 * @param {number} timeoutMs
 */
async function runPlaywrightFallbackLogin(
  candidate,
  profileDir,
  outputPath,
  timeoutMs,
) {
  process.stderr.write(
    `Warning: launching ${candidate.label} via Playwright. Twitch often blocks this.\n` +
      'Prefer Chrome, Edge, or Brave with a native launch.\n',
  );

  const context = await firefox.launchPersistentContext(profileDir, {
    headless: false,
    viewport: { width: 1280, height: 720 },
    ...(candidate.executablePath === undefined
      ? {}
      : { executablePath: candidate.executablePath }),
  });

  try {
    const page = context.pages()[0] ?? (await context.newPage());
    await page.goto(TWITCH_URL, { waitUntil: 'domcontentloaded' });

    process.stdout.write(
      `\nUsing ${candidate.label} (Playwright fallback).\n` +
        'Log in to Twitch, then press Enter here to save.\n\n',
    );

    await waitForEnter(timeoutMs);
    await context.storageState({ path: outputPath });
  } finally {
    await context.close().catch(() => undefined);
  }

  await finalizeSavedState(outputPath);
}

/**
 * @param {string} outputPath
 */
async function finalizeSavedState(outputPath) {
  await fs.chmod(outputPath, 0o600);

  const saved = JSON.parse(await fs.readFile(outputPath, 'utf8'));
  const cookieCount = Array.isArray(saved.cookies) ? saved.cookies.length : 0;
  if (cookieCount === 0) {
    throw new Error(
      `Wrote ${outputPath} but cookies are empty. Login may be incomplete.`,
    );
  }

  const hasAuth = hasTwitchAuthCookie(saved.cookies ?? []);
  process.stdout.write(
    `\nSaved Playwright storage state to ${outputPath}\n` +
      `cookies: ${cookieCount}` +
      (hasAuth ? ' (auth-token present)\n' : ' (auth-token not found; verify login)\n') +
      'This file is equivalent to Twitch login credentials. Do not commit it.\n',
  );

  if (!hasAuth) {
    process.exitCode = 2;
    process.stderr.write(
      'Tip: make sure the browser showed a successful Twitch login before pressing Enter.\n',
    );
  }
}

/**
 * @param {string} executablePath
 * @param {string} profileDir
 * @param {{ remoteDebuggingPort?: number, url: string }} options
 */
function spawnBrowser(executablePath, profileDir, options) {
  /** @type {string[]} */
  const args = [
    `--user-data-dir=${profileDir}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-sync',
    // Do not pass Playwright automation flags.
  ];

  if (options.remoteDebuggingPort !== undefined) {
    args.push(
      `--remote-debugging-port=${options.remoteDebuggingPort}`,
      '--remote-debugging-address=127.0.0.1',
    );
  }

  args.push(options.url);

  const child = spawn(executablePath, args, {
    stdio: 'ignore',
    detached: false,
  });

  if (child.pid === undefined) {
    throw new Error('failed to spawn browser process');
  }

  child.on('error', (error) => {
    process.stderr.write(`Browser process error: ${error.message}\n`);
  });

  return child;
}

/**
 * @param {BrowserCandidate[]} preferenceOrder
 * @returns {Promise<BrowserCandidate>}
 */
async function selectLaunchableCandidate(preferenceOrder) {
  /** @type {string[]} */
  const errors = [];

  for (const candidate of preferenceOrder) {
    if (supportsNativeChromiumLaunch(candidate)) {
      const executablePath = resolveCandidateExecutable(candidate);
      if (executablePath !== undefined) {
        return candidate;
      }
      errors.push(`${candidate.label}: executable not found`);
      continue;
    }

    if (
      candidate.kind === 'firefox' ||
      candidate.kind === 'playwright-firefox'
    ) {
      return candidate;
    }

    errors.push(`${candidate.label}: unsupported launch mode`);
  }

  throw new Error(
    `Unable to find a launchable browser.\n- ${errors.join('\n- ')}\n` +
      'Install Google Chrome, Microsoft Edge, or Brave and try again.',
  );
}

/**
 * @param {import('node:child_process').ChildProcess} child
 * @param {number} timeoutMs
 */
async function waitForEnterOrExit(child, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  const enterWait = createEnterWaiter();

  try {
    while (Date.now() < deadline) {
      if (enterWait.isResolved()) {
        return;
      }
      if (child.exitCode !== null) {
        throw new Error(
          'Browser window was closed before Enter was pressed. ' +
            'Log in successfully, leave the window open, then press Enter in this terminal.',
        );
      }
      await delay(200);
    }

    throw new Error(
      `Timed out after ${timeoutMs}ms waiting for Enter after Twitch login.`,
    );
  } finally {
    enterWait.close();
  }
}

/**
 * @param {number} timeoutMs
 */
async function waitForEnter(timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  const enterWait = createEnterWaiter();

  try {
    while (Date.now() < deadline) {
      if (enterWait.isResolved()) {
        return;
      }
      await delay(200);
    }
    throw new Error(
      `Timed out after ${timeoutMs}ms waiting for Enter after Twitch login.`,
    );
  } finally {
    enterWait.close();
  }
}

/**
 * @param {import('node:child_process').ChildProcess} child
 */
async function stopChild(child) {
  if (child.killed || child.exitCode !== null) {
    return;
  }

  child.kill('SIGTERM');
  const exited = await waitForChildExit(child, BROWSER_EXIT_TIMEOUT_MS);
  if (!exited && child.exitCode === null) {
    child.kill('SIGKILL');
    await waitForChildExit(child, 3_000);
  }
}

/**
 * @param {import('node:child_process').ChildProcess} child
 * @param {number} timeoutMs
 */
function waitForChildExit(child, timeoutMs) {
  if (child.exitCode !== null) {
    return Promise.resolve(true);
  }

  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      child.removeListener('exit', onExit);
      resolve(false);
    }, timeoutMs);

    function onExit() {
      clearTimeout(timer);
      resolve(true);
    }

    child.once('exit', onExit);
  });
}

/**
 * @param {number} port
 * @param {number} timeoutMs
 */
async function waitForCdpReady(port, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const response = await globalThis.fetch(
        `http://127.0.0.1:${port}/json/version`,
      );
      if (response.ok) {
        return;
      }
    } catch {
      // Retry until the browser finishes starting.
    }
    await delay(200);
  }
  throw new Error(
    `Timed out waiting for browser remote debugging on port ${port}`,
  );
}

function getFreePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.listen(0, '127.0.0.1', () => {
      const address = server.address();
      if (address === null || typeof address === 'string') {
        server.close();
        reject(new Error('Unable to allocate a free TCP port'));
        return;
      }
      const { port } = address;
      server.close((error) => {
        if (error) {
          reject(error);
          return;
        }
        resolve(port);
      });
    });
    server.on('error', reject);
  });
}

function createEnterWaiter() {
  let resolved = false;
  /** @type {readline.Interface | undefined} */
  let rl;

  if (process.stdin.isTTY) {
    rl = readline.createInterface({
      input: process.stdin,
      output: process.stdout,
      terminal: true,
    });
    rl.once('line', () => {
      resolved = true;
      rl?.close();
    });
  }

  return {
    isResolved: () => resolved,
    close: () => {
      rl?.close();
    },
  };
}

/**
 * @param {string[]} argv
 */
function parseArgs(argv) {
  let output = DEFAULT_OUTPUT;
  let timeoutMs = DEFAULT_TIMEOUT_MS;
  /** @type {string | undefined} */
  let browser;
  let help = false;

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '--help' || arg === '-h') {
      help = true;
      continue;
    }
    if (arg === '--out' || arg === '--output') {
      const value = argv[index + 1];
      if (value === undefined) {
        throw new Error(`${arg} requires a path`);
      }
      output = value;
      index += 1;
      continue;
    }
    if (arg.startsWith('--out=')) {
      output = arg.slice('--out='.length);
      continue;
    }
    if (arg === '--timeout-ms') {
      const value = argv[index + 1];
      if (value === undefined) {
        throw new Error('--timeout-ms requires a number');
      }
      timeoutMs = parsePositiveInt(value, '--timeout-ms');
      index += 1;
      continue;
    }
    if (arg.startsWith('--timeout-ms=')) {
      timeoutMs = parsePositiveInt(arg.slice('--timeout-ms='.length), '--timeout-ms');
      continue;
    }
    if (arg === '--browser') {
      const value = argv[index + 1];
      if (value === undefined) {
        throw new Error('--browser requires chrome|msedge|brave|firefox');
      }
      browser = value;
      index += 1;
      continue;
    }
    if (arg.startsWith('--browser=')) {
      browser = arg.slice('--browser='.length);
      continue;
    }
    throw new Error(`Unknown argument: ${arg}`);
  }

  return { output, timeoutMs, browser, help };
}

/**
 * @param {string} value
 * @param {string} flag
 */
function parsePositiveInt(value, flag) {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) {
    throw new Error(`${flag} must be a positive integer`);
  }
  return parsed;
}

function printHelp() {
  process.stdout.write(
    `Usage: npm run twitch:login -- [options]\n\n` +
      `Open a temporary browser profile for manual Twitch login and save\n` +
      `Playwright storage state for the watchdog container.\n\n` +
      `Login uses a normal OS browser process without remote debugging.\n` +
      `After you press Enter, the same temporary profile is relaunched briefly\n` +
      `with CDP only to export storage state.\n\n` +
      `Options:\n` +
      `  --out <path>           Output path (default: ${DEFAULT_OUTPUT})\n` +
      `  --browser <name>       Force chrome|msedge|brave|firefox\n` +
      `  --timeout-ms <ms>      Login wait timeout (default: ${DEFAULT_TIMEOUT_MS})\n` +
      `  -h, --help             Show this help\n`,
  );
}

/**
 * @param {string | undefined} defaultBrowserId
 * @param {string | undefined} forcedBrowser
 * @returns {BrowserCandidate[]}
 */
function resolvePreferenceOrder(defaultBrowserId, forcedBrowser) {
  if (forcedBrowser !== undefined) {
    const normalized = forcedBrowser.trim().toLocaleLowerCase('en-US');
    if (normalized === 'chrome') {
      return buildBrowserPreferenceOrder('com.google.Chrome').filter(
        (candidate) =>
          candidate.kind === 'chrome' && candidate.label.includes('Chrome'),
      );
    }
    if (normalized === 'msedge' || normalized === 'edge') {
      return buildBrowserPreferenceOrder('com.microsoft.Edge').filter(
        (candidate) => candidate.kind === 'msedge',
      );
    }
    if (normalized === 'brave') {
      const brave = mapDefaultBrowserId('com.brave.Browser');
      if (brave === undefined || brave.source === 'default-unsupported') {
        throw new Error('Brave Browser executable was not found');
      }
      return [{ ...brave, source: 'forced' }];
    }
    if (normalized === 'firefox') {
      return buildBrowserPreferenceOrder('firefox.desktop').filter(
        (candidate) =>
          candidate.kind === 'firefox' || candidate.kind === 'playwright-firefox',
      );
    }
    throw new Error('--browser must be one of: chrome, msedge, brave, firefox');
  }

  return buildBrowserPreferenceOrder(defaultBrowserId);
}

/**
 * @param {string | undefined} defaultBrowserId
 * @param {BrowserCandidate[]} preferenceOrder
 */
function printDetectionSummary(defaultBrowserId, preferenceOrder) {
  if (defaultBrowserId === undefined) {
    process.stdout.write('Default browser: (not detected)\n');
  } else {
    process.stdout.write(`Default browser id: ${defaultBrowserId}\n`);
    if (isUnsupportedDefaultBrowser(defaultBrowserId)) {
      process.stdout.write(
        'Default browser is not automation-friendly (for example Safari).\n' +
          'Falling back to Chrome / Edge / Firefox candidates.\n',
      );
    } else {
      const mapped = mapDefaultBrowserId(defaultBrowserId);
      if (mapped?.note) {
        process.stdout.write(`${mapped.note}\n`);
      }
    }
  }

  process.stdout.write('Browser preference order:\n');
  for (const [index, candidate] of preferenceOrder.entries()) {
    const launchMode = supportsNativeChromiumLaunch(candidate)
      ? 'native-two-phase'
      : 'playwright-fallback';
    process.stdout.write(
      `  ${index + 1}. ${candidate.label} [${candidate.source}, ${launchMode}]\n`,
    );
  }
}

/**
 * @param {string} label
 */
function sanitizeProfileName(label) {
  return label.toLocaleLowerCase('en-US').replace(/[^a-z0-9]+/gu, '-');
}

/**
 * @param {string} directory
 */
async function removeDirectory(directory) {
  await fs.rm(directory, { recursive: true, force: true }).catch(() => undefined);
}

function isMainModule() {
  const entry = process.argv[1];
  if (entry === undefined) {
    return false;
  }
  return path.resolve(entry) === fileURLToPath(import.meta.url);
}

if (isMainModule()) {
  main().catch((error) => {
    const message = error instanceof Error ? error.message : String(error);
    process.stderr.write(`twitch-login failed: ${message}\n`);
    process.exitCode = 1;
  });
}
