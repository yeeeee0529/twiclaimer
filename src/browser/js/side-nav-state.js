// 來源：$REF/dist/browser/SideNavCollapser.js（collapseSideNav 內 toggle.evaluate 的函式）
// 由 S1.2 逐字複製；修改前先寫 doc/decisions.md。
(element, options) => {
            const browserGlobal = globalThis;
            const ariaExpanded = element.getAttribute('aria-expanded');
            if (ariaExpanded !== null) {
                return ariaExpanded === 'true';
            }
            const accessibleLabel = [
                element.getAttribute('aria-label'),
                element.getAttribute('title'),
            ]
                .filter((value) => value !== null)
                .join(' ')
                .toLocaleLowerCase('en-US');
            if (/collapse|收合|折疊|折叠|縮小|缩小/u.test(accessibleLabel)) {
                return true;
            }
            if (/expand|展開|展开/u.test(accessibleLabel)) {
                return false;
            }
            const sideNav = element.closest('nav') ??
                browserGlobal.document.querySelector(options.sideNavSelector);
            return (sideNav !== null &&
                sideNav.getBoundingClientRect().width >= options.expandedMinWidthPx);
        }
