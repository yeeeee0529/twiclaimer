// 來源：$REF/dist/browser/ChannelHealthChecker.js（readChannelHealthSnapshot 內 page.evaluate 的函式）
// 由 S1.2 逐字複製；修改前先寫 doc/decisions.md。
(selectors) => {
        const browserGlobal = globalThis;
        const isElementVisible = (element) => {
            const style = browserGlobal.getComputedStyle(element);
            if (style.display === 'none' || style.visibility === 'hidden') {
                return false;
            }
            const bounds = element.getBoundingClientRect();
            return ((bounds.width > 0 && bounds.height > 0) ||
                element.getClientRects().length > 0);
        };
        const hasVisibleElement = (selector) => Array.from(browserGlobal.document.querySelectorAll(selector)).some(isElementVisible);
        const playerText = Array.from(browserGlobal.document.querySelectorAll(selectors.playerErrorText))
            .map((element) => element.textContent ?? '')
            .join(' ');
        return {
            loginRequired: hasVisibleElement(selectors.loginRequired),
            error: hasVisibleElement(selectors.error),
            offline: hasVisibleElement(selectors.offline),
            liveContent: hasVisibleElement(selectors.liveContent),
            unsupportedPlayerError: /This video is either unavailable or not supported in this browser/iu
                .test(playerText) || /Error\s*#4000/iu.test(playerText),
        };
    }
