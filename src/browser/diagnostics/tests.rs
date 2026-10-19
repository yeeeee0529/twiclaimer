//! 瀏覽器診斷純函式的單元測試：移植舊版 `test/unit/browser-diagnostics.test.ts`
//! 尚未處置的案例（`sanitizeBrowserUrl`、端點分類等 6 個標題已在
//! `channel_url/tests.rs` 處置）。`it.each` 的標題寫模板原文、案例表照抄。
use serde_json::json;

use crate::browser::channel_url::EndpointCategory;

use super::{
    DiagnosticLogLevel, MAX_GRAPHQL_OPERATION_NAME_LENGTH, MAX_GRAPHQL_OPERATION_NAMES,
    decide_console_level, decide_http_response_level, decide_request_failure_level,
    extract_graph_ql_operation_names, truncate_text,
};

#[test]
// 原測試：從單一 operation 物件抽出 operationName
fn extracts_operation_name_from_single_operation_object() {
    let names = extract_graph_ql_operation_names(Some(
        &json!({
            "operationName": "PlaybackAccessToken",
            "variables": { "login": "secret_channel" },
            "extensions": { "persistedQuery": { "sha256Hash": "hash-value" } },
        })
        .to_string(),
    ));

    assert_eq!(names, vec!["PlaybackAccessToken".to_string()]);
}

#[test]
// 原測試：從 batch 陣列抽出唯一 operationName 並忽略 variables 與 hashes
fn extracts_unique_operation_names_from_batch_and_ignores_variables_and_hashes() {
    let names = extract_graph_ql_operation_names(Some(
        &json!([
            { "operationName": "A", "variables": { "x": 1 } },
            { "operationName": "B", "extensions": { "sha256Hash": "h" } },
            { "operationName": "A" },
        ])
        .to_string(),
    ));

    assert_eq!(
        names,
        vec!["A".to_string(), "B".to_string()],
        "variables 與 hashes 一律不保留：{names:?}"
    );
}

#[test]
// 原測試：無效 JSON 回傳空陣列且不拋錯
fn invalid_json_returns_empty_vec_without_panicking() {
    assert!(extract_graph_ql_operation_names(Some("{invalid")).is_empty());
    assert!(extract_graph_ql_operation_names(None).is_empty());
    assert!(extract_graph_ql_operation_names(Some("")).is_empty());
}

#[test]
// 原測試：非物件或非字串 operationName 一律忽略
fn non_object_or_non_string_operation_names_are_ignored() {
    assert!(extract_graph_ql_operation_names(Some(&json!([1, "a", true]).to_string())).is_empty());
    assert!(
        extract_graph_ql_operation_names(Some(&json!({ "operationName": 42 }).to_string()))
            .is_empty()
    );
    assert!(
        extract_graph_ql_operation_names(Some(&json!({ "operationName": "" }).to_string()))
            .is_empty()
    );
}

#[test]
// 原測試：超過數量上限時只保留前 N 個
fn keeps_only_first_n_names_beyond_the_cap() {
    let operations: Vec<serde_json::Value> = (0..MAX_GRAPHQL_OPERATION_NAMES + 3)
        .map(|index| json!({ "operationName": format!("Op{index}") }))
        .collect();
    let body = serde_json::to_string(&operations).unwrap_or_default();
    let names = extract_graph_ql_operation_names(Some(body.as_str()));

    assert_eq!(names.len(), MAX_GRAPHQL_OPERATION_NAMES);
    assert_eq!(
        names,
        vec![
            "Op0".to_string(),
            "Op1".to_string(),
            "Op2".to_string(),
            "Op3".to_string(),
            "Op4".to_string(),
        ]
    );
}

#[test]
// 原測試：過長 operationName 依上限裁切
fn oversized_operation_name_is_truncated_to_limit() {
    let long_name = "x".repeat(MAX_GRAPHQL_OPERATION_NAME_LENGTH + 10);
    let names =
        extract_graph_ql_operation_names(Some(&json!({ "operationName": long_name }).to_string()));

    assert_eq!(names, vec!["x".repeat(MAX_GRAPHQL_OPERATION_NAME_LENGTH)]);
}

#[test]
// 原測試：status %s / %s / %s → %s
fn decides_http_response_level_per_case() {
    // 案例表照抄舊版 it.each（status, resourceType, category, expected）。
    let cases: [(u16, &str, EndpointCategory, DiagnosticLogLevel); 17] = [
        (
            503,
            "script",
            EndpointCategory::TwitchJavascript,
            DiagnosticLogLevel::Warn,
        ),
        (
            500,
            "document",
            EndpointCategory::TwitchDocument,
            DiagnosticLogLevel::Warn,
        ),
        (
            404,
            "document",
            EndpointCategory::TwitchDocument,
            DiagnosticLogLevel::Warn,
        ),
        (
            401,
            "fetch",
            EndpointCategory::ThirdParty,
            DiagnosticLogLevel::Warn,
        ),
        (
            429,
            "xhr",
            EndpointCategory::TwitchGraphql,
            DiagnosticLogLevel::Warn,
        ),
        (
            403,
            "other",
            EndpointCategory::TwitchAntiAbuse,
            DiagnosticLogLevel::Warn,
        ),
        (
            403,
            "other",
            EndpointCategory::ThirdParty,
            DiagnosticLogLevel::None,
        ),
        (
            404,
            "fetch",
            EndpointCategory::TwitchMedia,
            DiagnosticLogLevel::None,
        ),
        (
            200,
            "script",
            EndpointCategory::TwitchJavascript,
            DiagnosticLogLevel::Debug,
        ),
        (
            200,
            "document",
            EndpointCategory::TwitchDocument,
            DiagnosticLogLevel::Debug,
        ),
        (
            200,
            "fetch",
            EndpointCategory::TwitchGraphql,
            DiagnosticLogLevel::Debug,
        ),
        (
            200,
            "xhr",
            EndpointCategory::TwitchApi,
            DiagnosticLogLevel::Debug,
        ),
        (
            200,
            "image",
            EndpointCategory::TwitchJavascript,
            DiagnosticLogLevel::None,
        ),
        (
            200,
            "font",
            EndpointCategory::TwitchDocument,
            DiagnosticLogLevel::None,
        ),
        (
            200,
            "media",
            EndpointCategory::TwitchMedia,
            DiagnosticLogLevel::None,
        ),
        (
            200,
            "fetch",
            EndpointCategory::ThirdParty,
            DiagnosticLogLevel::None,
        ),
        (
            302,
            "document",
            EndpointCategory::TwitchDocument,
            DiagnosticLogLevel::Debug,
        ),
    ];
    for (status, resource_type, category, expected) in cases {
        assert_eq!(
            decide_http_response_level(status, resource_type, category),
            expected,
            "case: status {status} / {resource_type} / {}",
            category.as_str()
        );
    }
}

#[test]
// 原測試：%s / %s → %s
fn decides_request_failure_level_per_case() {
    // 案例表照抄舊版 it.each（category, failureText, expected）；`None` 對應 undefined。
    let cases: [(EndpointCategory, Option<&str>, DiagnosticLogLevel); 17] = [
        (
            EndpointCategory::ThirdParty,
            Some("net::ERR_CONNECTION_REFUSED"),
            DiagnosticLogLevel::Debug,
        ),
        (
            EndpointCategory::ThirdParty,
            Some("net::ERR_ABORTED"),
            DiagnosticLogLevel::Debug,
        ),
        (
            EndpointCategory::ThirdParty,
            None,
            DiagnosticLogLevel::Debug,
        ),
        (
            EndpointCategory::TwitchOther,
            Some("net::ERR_CONNECTION_REFUSED"),
            DiagnosticLogLevel::Debug,
        ),
        (
            EndpointCategory::Unknown,
            Some("net::ERR_FAILED"),
            DiagnosticLogLevel::Debug,
        ),
        (
            EndpointCategory::TwitchMedia,
            Some("net::ERR_ABORTED"),
            DiagnosticLogLevel::Debug,
        ),
        (
            EndpointCategory::TwitchDocument,
            Some("net::ERR_ABORTED"),
            DiagnosticLogLevel::Debug,
        ),
        (
            EndpointCategory::TwitchJavascript,
            Some("net::ERR_ABORTED"),
            DiagnosticLogLevel::Debug,
        ),
        (
            EndpointCategory::TwitchGraphql,
            Some("net::ERR_ABORTED"),
            DiagnosticLogLevel::Debug,
        ),
        (
            EndpointCategory::TwitchAntiAbuse,
            Some("net::ERR_ABORTED"),
            DiagnosticLogLevel::Debug,
        ),
        (
            EndpointCategory::TwitchMedia,
            Some("net::ERR_CONNECTION_REFUSED"),
            DiagnosticLogLevel::Warn,
        ),
        (
            EndpointCategory::TwitchMedia,
            None,
            DiagnosticLogLevel::Warn,
        ),
        (
            EndpointCategory::TwitchDocument,
            Some("net::ERR_FAILED"),
            DiagnosticLogLevel::Warn,
        ),
        (
            EndpointCategory::TwitchJavascript,
            Some("net::ERR_FAILED"),
            DiagnosticLogLevel::Warn,
        ),
        (
            EndpointCategory::TwitchGraphql,
            Some("net::ERR_FAILED"),
            DiagnosticLogLevel::Warn,
        ),
        (
            EndpointCategory::TwitchApi,
            Some("net::ERR_CONNECTION_REFUSED"),
            DiagnosticLogLevel::Warn,
        ),
        (
            EndpointCategory::TwitchAntiAbuse,
            Some("net::ERR_FAILED"),
            DiagnosticLogLevel::Warn,
        ),
    ];
    for (category, failure_text, expected) in cases {
        assert_eq!(
            decide_request_failure_level(category, failure_text),
            expected,
            "case: {} / {failure_text:?}",
            category.as_str()
        );
    }
}

#[test]
// 原測試：%s / %s / %s → %s
fn decides_console_level_per_case() {
    // 案例表照抄舊版 it.each（type, message, category, expected）；`None` 對應 undefined。
    let cases: [(&str, &str, Option<EndpointCategory>, DiagnosticLogLevel); 17] = [
        (
            "error",
            "Failed to load resource: net::ERR_CONNECTION_REFUSED",
            Some(EndpointCategory::ThirdParty),
            DiagnosticLogLevel::Debug,
        ),
        (
            "error",
            "Failed to load resource: net::ERR_CONNECTION_REFUSED",
            Some(EndpointCategory::TwitchOther),
            DiagnosticLogLevel::Debug,
        ),
        (
            "error",
            "Failed to load resource: net::ERR_ABORTED",
            Some(EndpointCategory::Unknown),
            DiagnosticLogLevel::Debug,
        ),
        (
            "error",
            "Failed to load resource: net::ERR_ABORTED",
            None,
            DiagnosticLogLevel::Warn,
        ),
        (
            "error",
            "SpadeClient send error -1 : Failed to fetch",
            Some(EndpointCategory::TwitchJavascript),
            DiagnosticLogLevel::Debug,
        ),
        (
            "error",
            "[GraphQL] One or more GraphQL errors were detected on request ABC. DropChannelCampaignsProgress: failed integrity check [Object]",
            Some(EndpointCategory::TwitchJavascript),
            DiagnosticLogLevel::Debug,
        ),
        (
            "error",
            "Failed to load resource: the server responded with a status of 429 (Too Many Requests)",
            Some(EndpointCategory::TwitchAntiAbuse),
            DiagnosticLogLevel::Warn,
        ),
        (
            "error",
            "Failed to load resource: net::ERR_ABORTED",
            Some(EndpointCategory::TwitchJavascript),
            DiagnosticLogLevel::Warn,
        ),
        (
            "error",
            "Some other page error",
            Some(EndpointCategory::TwitchGraphql),
            DiagnosticLogLevel::Warn,
        ),
        ("warning", "", None, DiagnosticLogLevel::Debug),
        ("warn", "", None, DiagnosticLogLevel::Debug),
        ("log", "", None, DiagnosticLogLevel::None),
        ("info", "", None, DiagnosticLogLevel::None),
        ("debug", "", None, DiagnosticLogLevel::None),
        ("table", "", None, DiagnosticLogLevel::None),
        ("timeEnd", "", None, DiagnosticLogLevel::None),
        ("trace", "", None, DiagnosticLogLevel::None),
    ];
    for (console_type, message, category, expected) in cases {
        let category_label =
            category.map_or_else(|| "undefined".to_string(), |c| c.as_str().to_string());
        assert_eq!(
            decide_console_level(console_type, message, category),
            expected,
            "case: {console_type} / {message:?} / {category_label}"
        );
    }
}

#[test]
// 原測試：超過上限時裁切，未超過時原樣回傳
fn truncates_beyond_limit_and_returns_original_within_limit() {
    assert_eq!(truncate_text("short", 10), "short");
    assert_eq!(truncate_text("1234567890", 5), "12345");
    assert_eq!(truncate_text("1234567890", 5).len(), 5);
}
