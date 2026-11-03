//! ChannelPointsReader：逐函式移植舊版 `src/browser/ChannelPointsReader.ts`。
//!
//! Rust 版公開 API 照本次移植契約回傳 `u64`；舊版同時回傳的
//! `displayValue` 由呼叫端不需要後省略。批次 DOM 讀取仍使用
//! `src/browser/js/channel-points-balance.js`，解析規則照舊版。
#![allow(dead_code)]

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use super::cdp::context::DriverError;
use super::cdp::page::PageShared;
use super::selectors::COMMUNITY_POINTS_BALANCE_SELECTOR;
use super::types::BrowserPage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BalanceError {
    NotFound,
    ParseFailed,
}

/// 本檔使用的頁面 API（session 模組以 fake 直接呼叫，故 pub(crate)）。
#[async_trait]
pub(crate) trait ChannelPointsBalancePage: Send + Sync {
    async fn evaluate_all(&self) -> Result<Value, DriverError>;
}

struct SharedChannelPointsBalancePage {
    page: Arc<PageShared>,
}

#[async_trait]
impl ChannelPointsBalancePage for SharedChannelPointsBalancePage {
    async fn evaluate_all(&self) -> Result<Value, DriverError> {
        self.page
            .locator(COMMUNITY_POINTS_BALANCE_SELECTOR)
            .evaluate_all(channel_points_balance_script())
            .await
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct ChannelPointsSummarySnapshot {
    visible: bool,
    candidates: Vec<String>,
}

pub(crate) async fn read_channel_points_balance(
    page: &dyn BrowserPage,
) -> Result<u64, BalanceError> {
    let Some(shared) = page.as_page_shared() else {
        return Err(BalanceError::ParseFailed);
    };
    read_balance_on_page(&SharedChannelPointsBalancePage { page: shared }).await
}

/// 舊版 `readChannelPointsBalance()` 本體（session 模組以 fake page 直接呼叫，
/// script 與本體提升為 pub(crate)；行為不變）。
pub(crate) async fn read_balance_on_page<P: ChannelPointsBalancePage>(
    page: &P,
) -> Result<u64, BalanceError> {
    let summaries = match page.evaluate_all().await {
        Ok(summaries) => parse_summary_snapshots(&summaries),
        Err(_) => return Err(BalanceError::ParseFailed),
    };

    for summary in summaries.iter().filter(|summary| summary.visible) {
        for candidate in &summary.candidates {
            if let Some(balance) = parse_balance(candidate) {
                return Ok(balance);
            }
        }
    }

    if summaries.iter().any(|summary| summary.visible) {
        Err(BalanceError::ParseFailed)
    } else {
        Err(BalanceError::NotFound)
    }
}

fn parse_summary_snapshots(reply: &Value) -> Vec<ChannelPointsSummarySnapshot> {
    reply
        .as_array()
        .map(|summaries| {
            summaries
                .iter()
                .map(|summary| ChannelPointsSummarySnapshot {
                    visible: summary
                        .get("visible")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    candidates: summary
                        .get("candidates")
                        .and_then(Value::as_array)
                        .map(|candidates| {
                            candidates
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_balance(value: &str) -> Option<u64> {
    let normalized_value = value
        .replace(['\u{00a0}', '\u{202f}'], " ")
        .trim()
        .to_string();

    for match_result in balance_pattern().find_iter(&normalized_value) {
        let display_value = match_result.as_str().trim();
        let suffix = balance_suffix(display_value).unwrap_or_default();
        let numeric_text = if suffix.is_empty() {
            display_value
        } else {
            display_value
                .strip_suffix(suffix)
                .expect("suffix was matched at the end")
                .trim()
        };
        let numeric_value = parse_localized_number(numeric_text, !suffix.is_empty())?;

        let multiplier = match suffix.to_uppercase().as_str() {
            "K" => 1_000.0,
            "M" => 1_000_000.0,
            "B" => 1_000_000_000.0,
            "萬" => 10_000.0,
            "億" | "亿" => 100_000_000.0,
            "" => 1.0,
            _ => continue,
        };
        let balance = (numeric_value * multiplier).round();
        if balance.is_finite()
            && (0.0..=9_007_199_254_740_991.0).contains(&balance)
            && balance.fract() == 0.0
        {
            return Some(balance as u64);
        }
    }

    None
}

fn parse_localized_number(value: &str, allow_decimal: bool) -> Option<f64> {
    let compact = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    if compact.chars().all(|character| character.is_ascii_digit()) && !compact.is_empty() {
        return compact.parse().ok();
    }

    let groups = compact
        .split(['.', ','])
        .map(str::to_string)
        .collect::<Vec<_>>();
    if groups.len() > 1
        && !groups[0].is_empty()
        && groups[0].len() <= 3
        && groups[0]
            .chars()
            .all(|character| character.is_ascii_digit())
        && groups[1..].iter().all(|group| {
            group.len() == 3 && group.chars().all(|character| character.is_ascii_digit())
        })
    {
        return groups.concat().parse().ok();
    }

    if allow_decimal
        && groups.len() == 2
        && groups.iter().all(|group| {
            !group.is_empty() && group.chars().all(|character| character.is_ascii_digit())
        })
    {
        return format!("{}.{}", groups[0], groups[1]).parse().ok();
    }

    None
}

fn balance_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?:\d{1,3}(?:(?:[.,]|\s)\d{3})+|\d+(?:[.,]\d+)?)\s*(?:K|M|B|萬|億|亿)?")
            .expect("balance pattern must be valid")
    })
}

fn balance_suffix(display_value: &str) -> Option<&'static str> {
    for (lowercase, suffix) in [("k", "K"), ("m", "M"), ("b", "B")] {
        if display_value.ends_with(suffix) {
            return Some(suffix);
        }
        if display_value.ends_with(lowercase) {
            return Some(suffix);
        }
    }
    ["萬", "億", "亿"]
        .into_iter()
        .find(|suffix| display_value.ends_with(suffix))
}

/// `src/browser/js/channel-points-balance.js` 的載入（session 模組重用，pub(crate)）。
pub(crate) fn channel_points_balance_script() -> &'static str {
    static SCRIPT: OnceLock<String> = OnceLock::new();
    SCRIPT
        .get_or_init(|| {
            let raw = include_str!("js/channel-points-balance.js");
            let header_lines = raw
                .split('\n')
                .take_while(|line| line.trim_start().starts_with("//"))
                .count();
            raw.split('\n')
                .skip(header_lines)
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string()
        })
        .as_str()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct FakeBalancePage {
        summaries: Vec<ChannelPointsSummarySnapshot>,
        evaluate_all_error: Option<String>,
        evaluate_all_calls: AtomicUsize,
    }

    impl FakeBalancePage {
        fn from_candidates(summaries: &[&[&str]]) -> Self {
            Self {
                summaries: summaries
                    .iter()
                    .map(|candidates| ChannelPointsSummarySnapshot {
                        visible: true,
                        candidates: candidates.iter().map(|value| value.to_string()).collect(),
                    })
                    .collect(),
                evaluate_all_error: None,
                evaluate_all_calls: AtomicUsize::new(0),
            }
        }

        fn hidden() -> Self {
            Self {
                summaries: vec![ChannelPointsSummarySnapshot {
                    visible: false,
                    candidates: vec!["9,999".to_string()],
                }],
                evaluate_all_error: None,
                evaluate_all_calls: AtomicUsize::new(0),
            }
        }

        fn failing() -> Self {
            Self {
                summaries: Vec::new(),
                evaluate_all_error: Some("Execution context was destroyed".to_string()),
                evaluate_all_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ChannelPointsBalancePage for FakeBalancePage {
        async fn evaluate_all(&self) -> Result<Value, DriverError> {
            self.evaluate_all_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = &self.evaluate_all_error {
                return Err(DriverError::new("Error", error.clone()));
            }
            Ok(serde_json::json!(self.summaries))
        }
    }

    // 原測試：解析千分位 %s
    #[tokio::test]
    async fn parses_thousands() {
        for (display_value, expected) in [
            ("1,250", 1_250),
            ("1.250", 1_250),
            ("12 345 678", 12_345_678),
        ] {
            let page = FakeBalancePage::from_candidates(&[&["", display_value]]);
            assert_eq!(read_balance_on_page(&page).await, Ok(expected));
        }
    }

    // 原測試：解析縮寫 %s
    #[tokio::test]
    async fn parses_suffixes() {
        for (display_value, expected) in [
            ("1.2K", 1_200),
            ("2,5 M", 2_500_000),
            ("3B", 3_000_000_000),
            ("1.5萬", 15_000),
            ("2,5億", 250_000_000),
            ("3亿", 300_000_000),
        ] {
            let page = FakeBalancePage::from_candidates(&[&[display_value]]);
            assert_eq!(read_balance_on_page(&page).await, Ok(expected));
        }
    }

    // 原測試：依序檢查摘要候選值並只執行一次批次 DOM 讀取
    #[tokio::test]
    async fn checks_summary_candidates_in_order_with_one_batch_read() {
        let page = FakeBalancePage::from_candidates(&[
            &["Bits and Points Balances", "沒有可用餘額"],
            &["Channel Points: 8,765"],
        ]);

        assert_eq!(read_balance_on_page(&page).await, Ok(8_765));
        assert_eq!(page.evaluate_all_calls.load(Ordering::SeqCst), 1);
    }

    // 原測試：摘要不存在時回傳 not_found
    #[tokio::test]
    async fn returns_not_found_when_summaries_do_not_exist() {
        let page = FakeBalancePage::from_candidates(&[]);

        assert_eq!(
            read_balance_on_page(&page).await,
            Err(BalanceError::NotFound)
        );
    }

    // 原測試：只存在隱藏摘要時視為 not_found
    #[tokio::test]
    async fn treats_hidden_summary_as_not_found() {
        let page = FakeBalancePage::hidden();

        assert_eq!(
            read_balance_on_page(&page).await,
            Err(BalanceError::NotFound)
        );
    }

    // 原測試：摘要存在但沒有可解析數字時回傳 parse_failed
    #[tokio::test]
    async fn returns_parse_failed_when_no_number_is_parseable() {
        let page = FakeBalancePage::from_candidates(&[&["Channel Points unavailable"]]);

        assert_eq!(
            read_balance_on_page(&page).await,
            Err(BalanceError::ParseFailed)
        );
    }

    // 原測試：DOM 批次讀取失敗時不向外拋出錯誤
    #[tokio::test]
    async fn swallows_failed_batch_dom_read() {
        let page = FakeBalancePage::failing();

        assert_eq!(
            read_balance_on_page(&page).await,
            Err(BalanceError::ParseFailed)
        );
    }
}
