import process from 'node:process';

import { describe, expect, it } from 'vitest';

import {
  buildBrowserPreferenceOrder,
  hasTwitchAuthCookie,
  isUnsupportedDefaultBrowser,
  mapDefaultBrowserId,
  supportsNativeChromiumLaunch,
} from '../scripts/lib/default-browser.mjs';

describe('default-browser helpers', () => {
  it('maps common default browser identifiers', () => {
    expect(mapDefaultBrowserId('com.google.Chrome', 'darwin')).toMatchObject({
      kind: 'chrome',
      channel: 'chrome',
      source: 'default',
    });
    expect(mapDefaultBrowserId('ChromeHTML', 'win32')).toMatchObject({
      kind: 'chrome',
      channel: 'chrome',
    });
    expect(mapDefaultBrowserId('MSEdgeHTM', 'win32')).toMatchObject({
      kind: 'msedge',
      channel: 'msedge',
    });
    expect(
      mapDefaultBrowserId('google-chrome.desktop', 'linux'),
    ).toMatchObject({
      kind: 'chrome',
      channel: 'chrome',
    });
  });

  it('marks Safari as unsupported for automation', () => {
    expect(isUnsupportedDefaultBrowser('com.apple.Safari')).toBe(true);
    expect(mapDefaultBrowserId('com.apple.Safari', 'darwin')).toMatchObject({
      source: 'default-unsupported',
    });
  });

  it('puts a supported default browser first in preference order', () => {
    const order = buildBrowserPreferenceOrder('com.microsoft.Edge', 'darwin');
    expect(order[0]).toMatchObject({
      kind: 'msedge',
      channel: 'msedge',
      source: 'default',
    });
    // Chrome/Edge fallbacks are only added when the executable exists.
    expect(order.some((item) => item.kind === 'playwright-firefox')).toBe(true);
    expect(order.filter((item) => item.source === 'default')).toHaveLength(1);
  });

  it('does not put Safari in the launch order as a candidate', () => {
    const order = buildBrowserPreferenceOrder('com.apple.Safari', 'darwin');
    expect(order.every((item) => item.source !== 'default-unsupported')).toBe(
      true,
    );
    expect(order[0]?.source).toBe('fallback');
  });

  it('detects Twitch auth-token cookies', () => {
    expect(
      hasTwitchAuthCookie([
        {
          name: 'auth-token',
          value: 'secret',
          domain: '.twitch.tv',
          path: '/',
          expires: -1,
          httpOnly: true,
          secure: true,
          sameSite: 'None',
        },
      ]),
    ).toBe(true);

    expect(
      hasTwitchAuthCookie([
        {
          name: 'unique_id',
          value: 'abc',
          domain: '.twitch.tv',
          path: '/',
          expires: -1,
          httpOnly: false,
          secure: true,
          sameSite: 'Lax',
        },
      ]),
    ).toBe(false);
  });

  it('treats chromium family candidates as native launch when executable exists', () => {
    expect(
      supportsNativeChromiumLaunch({
        kind: 'chrome',
        channel: 'chrome',
        label: 'Google Chrome',
        source: 'fallback',
        // process.execPath always exists in the test environment.
        executablePath: process.execPath,
      }),
    ).toBe(true);

    expect(
      supportsNativeChromiumLaunch({
        kind: 'playwright-firefox',
        label: 'Playwright Firefox',
        source: 'fallback',
      }),
    ).toBe(false);
  });
});
