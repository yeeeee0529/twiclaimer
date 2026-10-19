import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';

/**
 * @typedef {'chrome' | 'msedge' | 'firefox' | 'playwright-firefox'} BrowserKind
 *
 * @typedef {object} BrowserCandidate
 * @property {BrowserKind} kind
 * @property {string} label
 * @property {string} source
 * @property {string} [channel]
 * @property {string} [executablePath]
 * @property {string} [note]
 */

/** @type {ReadonlyArray<{ match: RegExp, kind: BrowserKind, channel?: string, label: string }>} */
const DEFAULT_BROWSER_RULES = [
  {
    match: /com\.google\.chrome(\.beta|\.canary|\.dev)?$/iu,
    kind: 'chrome',
    channel: 'chrome',
    label: 'Google Chrome',
  },
  {
    match: /^(chromehtml|chrome)$/iu,
    kind: 'chrome',
    channel: 'chrome',
    label: 'Google Chrome',
  },
  {
    match: /google-chrome(\.desktop)?$/iu,
    kind: 'chrome',
    channel: 'chrome',
    label: 'Google Chrome',
  },
  {
    match: /com\.microsoft\.edgemac(\.beta|\.dev|\.canary)?$/iu,
    kind: 'msedge',
    channel: 'msedge',
    label: 'Microsoft Edge',
  },
  {
    match: /com\.microsoft\.edge(\.beta|\.dev|\.canary)?$/iu,
    kind: 'msedge',
    channel: 'msedge',
    label: 'Microsoft Edge',
  },
  {
    match: /^(msedgehtm|msedge|microsoft-edge(\.desktop)?)$/iu,
    kind: 'msedge',
    channel: 'msedge',
    label: 'Microsoft Edge',
  },
  {
    match: /org\.mozilla\.firefox(\..*)?$/iu,
    kind: 'firefox',
    label: 'Mozilla Firefox',
  },
  {
    match: /^(firefoxurl(-\w+)?|firefox(\.desktop)?)$/iu,
    kind: 'firefox',
    label: 'Mozilla Firefox',
  },
  {
    match: /com\.brave\.browser$/iu,
    kind: 'chrome',
    label: 'Brave Browser',
  },
  {
    match: /brave-browser(\.desktop)?$/iu,
    kind: 'chrome',
    label: 'Brave Browser',
  },
  {
    match: /company\.thebrowser\.browser$/iu,
    kind: 'chrome',
    label: 'Arc',
  },
  {
    match: /com\.operasoftware\.opera$/iu,
    kind: 'chrome',
    label: 'Opera',
  },
];

const UNSUPPORTED_DEFAULT_MATCHERS = [
  /com\.apple\.safari/iu,
  /^safari$/iu,
  /safari\.desktop$/iu,
];

/**
 * @param {string | undefined | null} value
 * @returns {boolean}
 */
export function isUnsupportedDefaultBrowser(value) {
  if (value === undefined || value === null || value.trim() === '') {
    return false;
  }
  return UNSUPPORTED_DEFAULT_MATCHERS.some((pattern) => pattern.test(value.trim()));
}

/**
 * Map OS default-browser identifier to a launch candidate.
 *
 * @param {string | undefined | null} defaultBrowserId
 * @param {NodeJS.Platform} [platform]
 * @returns {BrowserCandidate | undefined}
 */
export function mapDefaultBrowserId(defaultBrowserId, platform = process.platform) {
  if (defaultBrowserId === undefined || defaultBrowserId === null) {
    return undefined;
  }

  const normalized = defaultBrowserId.trim();
  if (normalized === '') {
    return undefined;
  }

  if (isUnsupportedDefaultBrowser(normalized)) {
    return {
      kind: 'chrome',
      label: normalized,
      source: 'default-unsupported',
      note: 'Safari and some browsers cannot be driven by Playwright; falling back to other browsers.',
    };
  }

  for (const rule of DEFAULT_BROWSER_RULES) {
    if (!rule.match.test(normalized)) {
      continue;
    }

    /** @type {BrowserCandidate} */
    const candidate = {
      kind: rule.kind,
      label: rule.label,
      source: 'default',
      ...(rule.channel === undefined ? {} : { channel: rule.channel }),
    };

    if (rule.kind === 'firefox') {
      const executablePath = resolveSystemFirefoxExecutable(platform);
      if (executablePath !== undefined) {
        candidate.executablePath = executablePath;
        candidate.label = 'Mozilla Firefox (system)';
      } else {
        candidate.kind = 'playwright-firefox';
        candidate.label = 'Mozilla Firefox (Playwright)';
        candidate.note =
          'System Firefox was not found; Playwright Firefox may be blocked by Twitch.';
      }
      return candidate;
    }

    if (
      rule.label === 'Brave Browser' ||
      rule.label === 'Arc' ||
      rule.label === 'Opera'
    ) {
      const executablePath = resolveChromiumFamilyExecutable(rule.label, platform);
      if (executablePath === undefined) {
        return undefined;
      }
      candidate.executablePath = executablePath;
      delete candidate.channel;
      return candidate;
    }

    if (rule.kind === 'chrome') {
      const executablePath = resolveGoogleChromeExecutable(platform);
      if (executablePath !== undefined) {
        candidate.executablePath = executablePath;
      }
      return candidate;
    }

    if (rule.kind === 'msedge') {
      const executablePath = resolveMicrosoftEdgeExecutable(platform);
      if (executablePath !== undefined) {
        candidate.executablePath = executablePath;
      }
      return candidate;
    }

    return candidate;
  }

  return undefined;
}

/**
 * Build launch order: supported default browser first, then common fallbacks.
 *
 * @param {string | undefined | null} defaultBrowserId
 * @param {NodeJS.Platform} [platform]
 * @returns {BrowserCandidate[]}
 */
export function buildBrowserPreferenceOrder(
  defaultBrowserId,
  platform = process.platform,
) {
  /** @type {BrowserCandidate[]} */
  const order = [];
  const seen = new Set();

  /**
   * @param {BrowserCandidate} candidate
   */
  const add = (candidate) => {
    if (candidate.source === 'default-unsupported') {
      return;
    }
    const key = candidateKey(candidate);
    if (seen.has(key)) {
      return;
    }
    seen.add(key);
    order.push(candidate);
  };

  const mappedDefault = mapDefaultBrowserId(defaultBrowserId, platform);
  if (mappedDefault !== undefined && mappedDefault.source === 'default') {
    add(mappedDefault);
  }

  const chromePath = resolveGoogleChromeExecutable(platform);
  if (chromePath !== undefined) {
    add({
      kind: 'chrome',
      channel: 'chrome',
      executablePath: chromePath,
      label: 'Google Chrome',
      source: 'fallback',
    });
  }

  const edgePath = resolveMicrosoftEdgeExecutable(platform);
  if (edgePath !== undefined) {
    add({
      kind: 'msedge',
      channel: 'msedge',
      executablePath: edgePath,
      label: 'Microsoft Edge',
      source: 'fallback',
    });
  }

  const systemFirefox = resolveSystemFirefoxExecutable(platform);
  if (systemFirefox !== undefined) {
    add({
      kind: 'firefox',
      executablePath: systemFirefox,
      label: 'Mozilla Firefox (system)',
      source: 'fallback',
      note: 'Firefox login export uses Playwright attach and may be blocked by Twitch.',
    });
  }

  add({
    kind: 'playwright-firefox',
    label: 'Playwright Firefox',
    source: 'fallback',
    note: 'Last-resort automated browser; Twitch often blocks this.',
  });

  return order;
}

/**
 * Resolve a filesystem executable for a candidate, if available.
 *
 * @param {BrowserCandidate} candidate
 * @param {NodeJS.Platform} [platform]
 * @returns {string | undefined}
 */
export function resolveCandidateExecutable(
  candidate,
  platform = process.platform,
) {
  if (candidate.executablePath !== undefined && fs.existsSync(candidate.executablePath)) {
    return candidate.executablePath;
  }

  if (candidate.kind === 'chrome') {
    return resolveGoogleChromeExecutable(platform);
  }
  if (candidate.kind === 'msedge') {
    return resolveMicrosoftEdgeExecutable(platform);
  }
  if (candidate.kind === 'firefox') {
    return resolveSystemFirefoxExecutable(platform);
  }

  if (
    candidate.label === 'Brave Browser' ||
    candidate.label === 'Arc' ||
    candidate.label === 'Opera'
  ) {
    return resolveChromiumFamilyExecutable(candidate.label, platform);
  }

  return undefined;
}

/**
 * Detect the OS default browser identifier for https URLs.
 *
 * @param {object} [options]
 * @param {NodeJS.Platform} [options.platform]
 * @param {typeof execFileSync} [options.execFileSyncImpl]
 * @param {() => string} [options.homedir]
 * @returns {string | undefined}
 */
export function detectDefaultBrowserId(options = {}) {
  const platform = options.platform ?? process.platform;
  const execFile = options.execFileSyncImpl ?? execFileSync;
  const homedir = options.homedir ?? (() => os.homedir());

  try {
    if (platform === 'darwin') {
      return detectDefaultBrowserIdMac({ execFile, homedir });
    }
    if (platform === 'win32') {
      return detectDefaultBrowserIdWindows({ execFile });
    }
    if (platform === 'linux') {
      return detectDefaultBrowserIdLinux({ execFile });
    }
  } catch {
    return undefined;
  }

  return undefined;
}

/**
 * @param {object} deps
 * @param {typeof execFileSync} deps.execFile
 * @param {() => string} deps.homedir
 * @returns {string | undefined}
 */
function detectDefaultBrowserIdMac({ execFile, homedir }) {
  const script = `
import plistlib
import pathlib
import sys

path = pathlib.Path.home() / "Library/Preferences/com.apple.LaunchServices/com.apple.launchservices.secure.plist"
if not path.exists():
    sys.exit(0)
with path.open("rb") as handle:
    data = plistlib.load(handle)
handlers = data.get("LSHandlers", [])
chosen = None
for handler in handlers:
    scheme = handler.get("LSHandlerURLScheme")
    if scheme != "https":
        continue
    role = handler.get("LSHandlerRoleAll") or handler.get("LSHandlerRoleViewer")
    if role:
        chosen = role
if chosen:
    print(chosen)
`;

  // Ensure HOME for python pathlib.Path.home when tests override homedir.
  const result = execFile('python3', ['-c', script], {
    encoding: 'utf8',
    env: {
      ...process.env,
      HOME: homedir(),
    },
    timeout: 5_000,
    stdio: ['ignore', 'pipe', 'pipe'],
  }).trim();

  return result === '' ? undefined : result;
}

/**
 * @param {object} deps
 * @param {typeof execFileSync} deps.execFile
 * @returns {string | undefined}
 */
function detectDefaultBrowserIdWindows({ execFile }) {
  const output = execFile(
    'reg',
    [
      'query',
      'HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\Shell\\Associations\\UrlAssociations\\https\\UserChoice',
      '/v',
      'ProgId',
    ],
    {
      encoding: 'utf8',
      timeout: 5_000,
      stdio: ['ignore', 'pipe', 'pipe'],
    },
  );

  const match = output.match(/ProgId\s+REG_SZ\s+(\S+)/u);
  if (match?.[1] === undefined) {
    return undefined;
  }
  return match[1].trim();
}

/**
 * @param {object} deps
 * @param {typeof execFileSync} deps.execFile
 * @returns {string | undefined}
 */
function detectDefaultBrowserIdLinux({ execFile }) {
  try {
    const result = execFile('xdg-settings', ['get', 'default-web-browser'], {
      encoding: 'utf8',
      timeout: 5_000,
      stdio: ['ignore', 'pipe', 'pipe'],
    }).trim();
    if (result !== '') {
      return result;
    }
  } catch {
    // Fall through to xdg-mime.
  }

  try {
    const result = execFile(
      'xdg-mime',
      ['query', 'default', 'x-scheme-handler/https'],
      {
        encoding: 'utf8',
        timeout: 5_000,
        stdio: ['ignore', 'pipe', 'pipe'],
      },
    ).trim();
    return result === '' ? undefined : result;
  } catch {
    return undefined;
  }
}

/**
 * @param {NodeJS.Platform} [platform]
 * @returns {string | undefined}
 */
export function resolveSystemFirefoxExecutable(platform = process.platform) {
  /** @type {string[]} */
  const candidates = [];

  if (platform === 'darwin') {
    candidates.push('/Applications/Firefox.app/Contents/MacOS/firefox');
  } else if (platform === 'win32') {
    const programFiles = process.env['ProgramFiles'] ?? 'C:\\Program Files';
    const programFilesX86 =
      process.env['ProgramFiles(x86)'] ?? 'C:\\Program Files (x86)';
    candidates.push(
      path.join(programFiles, 'Mozilla Firefox', 'firefox.exe'),
      path.join(programFilesX86, 'Mozilla Firefox', 'firefox.exe'),
    );
  } else {
    candidates.push('/usr/bin/firefox', '/usr/local/bin/firefox', '/snap/bin/firefox');
  }

  return candidates.find((candidate) => fs.existsSync(candidate));
}

/**
 * @param {NodeJS.Platform} [platform]
 * @returns {string | undefined}
 */
export function resolveGoogleChromeExecutable(platform = process.platform) {
  /** @type {string[]} */
  const candidates = [];

  if (platform === 'darwin') {
    candidates.push(
      '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    );
  } else if (platform === 'win32') {
    const programFiles = process.env['ProgramFiles'] ?? 'C:\\Program Files';
    const programFilesX86 =
      process.env['ProgramFiles(x86)'] ?? 'C:\\Program Files (x86)';
    const localAppData = process.env.LOCALAPPDATA;
    candidates.push(
      path.join(programFiles, 'Google', 'Chrome', 'Application', 'chrome.exe'),
      path.join(programFilesX86, 'Google', 'Chrome', 'Application', 'chrome.exe'),
    );
    if (localAppData !== undefined) {
      candidates.push(
        path.join(localAppData, 'Google', 'Chrome', 'Application', 'chrome.exe'),
      );
    }
  } else {
    candidates.push(
      '/usr/bin/google-chrome-stable',
      '/usr/bin/google-chrome',
      '/usr/bin/chrome',
      '/snap/bin/chromium',
    );
  }

  return candidates.find((candidate) => fs.existsSync(candidate));
}

/**
 * @param {NodeJS.Platform} [platform]
 * @returns {string | undefined}
 */
export function resolveMicrosoftEdgeExecutable(platform = process.platform) {
  /** @type {string[]} */
  const candidates = [];

  if (platform === 'darwin') {
    candidates.push(
      '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge',
    );
  } else if (platform === 'win32') {
    const programFiles = process.env['ProgramFiles'] ?? 'C:\\Program Files';
    const programFilesX86 =
      process.env['ProgramFiles(x86)'] ?? 'C:\\Program Files (x86)';
    candidates.push(
      path.join(programFiles, 'Microsoft', 'Edge', 'Application', 'msedge.exe'),
      path.join(
        programFilesX86,
        'Microsoft',
        'Edge',
        'Application',
        'msedge.exe',
      ),
    );
  } else {
    candidates.push(
      '/usr/bin/microsoft-edge',
      '/usr/bin/microsoft-edge-stable',
      '/usr/bin/msedge',
    );
  }

  return candidates.find((candidate) => fs.existsSync(candidate));
}

/**
 * @param {string} label
 * @param {NodeJS.Platform} [platform]
 * @returns {string | undefined}
 */
export function resolveChromiumFamilyExecutable(
  label,
  platform = process.platform,
) {
  /** @type {string[]} */
  const candidates = [];

  if (platform === 'darwin') {
    if (label === 'Brave Browser') {
      candidates.push(
        '/Applications/Brave Browser.app/Contents/MacOS/Brave Browser',
      );
    } else if (label === 'Arc') {
      candidates.push('/Applications/Arc.app/Contents/MacOS/Arc');
    } else if (label === 'Opera') {
      candidates.push('/Applications/Opera.app/Contents/MacOS/Opera');
    }
  } else if (platform === 'linux') {
    if (label === 'Brave Browser') {
      candidates.push('/usr/bin/brave-browser', '/usr/bin/brave-browser-stable');
    } else if (label === 'Opera') {
      candidates.push('/usr/bin/opera');
    }
  } else if (platform === 'win32') {
    const localAppData = process.env.LOCALAPPDATA;
    if (label === 'Brave Browser' && localAppData !== undefined) {
      candidates.push(
        path.join(
          localAppData,
          'BraveSoftware',
          'Brave-Browser',
          'Application',
          'brave.exe',
        ),
      );
    }
  }

  return candidates.find((candidate) => fs.existsSync(candidate));
}

/**
 * Whether this candidate can be launched as a normal OS process (no Playwright
 * automation flags). Chromium-family browsers use remote debugging for export.
 *
 * @param {BrowserCandidate} candidate
 * @returns {boolean}
 */
export function supportsNativeChromiumLaunch(candidate) {
  if (
    candidate.kind === 'firefox' ||
    candidate.kind === 'playwright-firefox'
  ) {
    return false;
  }

  return resolveCandidateExecutable(candidate) !== undefined;
}

/**
 * @param {BrowserCandidate} candidate
 * @returns {string}
 */
function candidateKey(candidate) {
  return [
    candidate.kind,
    candidate.channel ?? '',
    candidate.executablePath ?? '',
  ].join('|');
}

/**
 * @param {import('playwright').Cookie[]} cookies
 * @returns {boolean}
 */
export function hasTwitchAuthCookie(cookies) {
  return cookies.some(
    (cookie) =>
      cookie.name === 'auth-token' &&
      typeof cookie.value === 'string' &&
      cookie.value.trim() !== '' &&
      (cookie.domain === 'twitch.tv' ||
        cookie.domain === '.twitch.tv' ||
        cookie.domain.endsWith('.twitch.tv')),
  );
}
