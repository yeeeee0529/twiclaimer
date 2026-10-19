// 來源：$REF/dist/browser/RewardClaimer.js（findRewardCandidates 內 evaluateAll 的函式）
// 由 S1.2 逐字複製；修改前先寫 doc/decisions.md。
(elements) => elements.map((element) => {
        const candidate = element;
        const style = globalThis
            .getComputedStyle(candidate);
        const bounds = candidate.getBoundingClientRect();
        const visible = style.display !== 'none' &&
            style.visibility !== 'hidden' &&
            ((bounds.width > 0 && bounds.height > 0) ||
                candidate.getClientRects().length > 0);
        return {
            visible,
            disabled: candidate.disabled === true ||
                candidate.matches(':disabled') ||
                candidate.closest('[aria-disabled="true"]') !== null,
            ariaLabel: candidate.getAttribute('aria-label'),
            className: candidate.getAttribute('class'),
        };
    })
