//! CDP 層：取代 Playwright 的自製驅動（D8、§7.3）。
pub(crate) mod connection;
pub(crate) mod context;
pub(crate) mod injected;
pub(crate) mod launcher;
pub(crate) mod locator;
pub(crate) mod page;
pub(crate) mod pointer;
/// `twiclaimer probe` 子命令（§7.3.7）：流程與 `tools/ref/pw-trace.mjs` 相同。
pub(crate) mod probe;
pub(crate) mod transport;

#[cfg(test)]
mod tests;
