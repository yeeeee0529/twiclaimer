// 來源：$REF/dist/browser/ChannelPointsReader.js（readChannelPointsBalance 內 evaluateAll 的函式）
// 由 S1.2 逐字複製；修改前先寫 doc/decisions.md。
(elements) => {
            const browserGlobal = globalThis;
            return elements.map((element) => {
                const style = browserGlobal.getComputedStyle(element);
                const bounds = element.getBoundingClientRect();
                return {
                    visible: style.display !== 'none' &&
                        style.visibility !== 'hidden' &&
                        ((bounds.width > 0 && bounds.height > 0) ||
                            element.getClientRects().length > 0),
                    candidates: [
                        element.querySelector('[data-test-selector="balance-string"]')?.textContent ?? null,
                        element.getAttribute('aria-label'),
                        element.getAttribute('title'),
                        element.textContent,
                    ].filter((value) => value !== null),
                };
            });
        }
