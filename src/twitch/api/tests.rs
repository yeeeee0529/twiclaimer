//! `TwitchApiClient` 的測試：移植舊版 `test/unit/twitch-api.test.ts`（17 個 `it`
//! 加 5 個 `it.each` 的案例表；標題與 `doc/spec/ts-test-titles.tsv` 逐字相同）。
//!
//! HTTP 一律以注入的 [`MockTransport`]（對應舊測試的 fetch mock）模擬，不連
//! 真實 Twitch；逾時場景用 `start_paused` 的虛擬時間（`tokio::time::timeout`
//! 的計時照舊版 AbortController 的語意），單元測試都在 1 秒內完成。另有數個
//! 純邏輯直接測試（URL 組裝、回應解析）與 [`ReqwestTransport`] 對本機 HTTP
//! 伺服器的整合測試（HTTP/1.1、redirect 拒絕）。

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

use super::{
    ReqwestTransport, TwitchApiClient, TwitchApiClientOptions, TwitchApiClock, TwitchApiLogger,
    TwitchApiTemporaryErrorReason, TwitchHttpHeaders, TwitchHttpRequest, TwitchHttpResponse,
    TwitchHttpTransport, TwitchTransportError, iso_utc_string_from_epoch_ms,
};
use crate::logging::redact::SanitizedValue;
use crate::twitch::errors::{TwitchApiError, TwitchApiErrorKind, TwitchApiTemporaryError};
use crate::twitch::live_status::ChannelLiveStatus;

/// 舊測試的 `CLIENT_ID`。
const CLIENT_ID: &str = "fixture-client-id";
/// 舊測試的 `ACCESS_TOKEN`。
const ACCESS_TOKEN: &str = "fixture-access-token";
/// 舊測試的 `CLIENT_SECRET`。
const CLIENT_SECRET: &str = "fixture-client-secret";
/// 舊測試 `FIXED_TIME` 的 ISO 字串（checkedAt 等欄位的預期值）。
const FIXED_TIME_ISO: &str = "2026-06-14T12:00:00.000Z";

/// 舊測試的 `FIXED_TIME`。
fn fixed_time() -> OffsetDateTime {
    OffsetDateTime::parse(FIXED_TIME_ISO, &Rfc3339).expect("fixed time")
}

/// 舊測試 `FIXED_TIME.getTime()` 的 epoch 毫秒。
fn fixed_time_ms() -> i64 {
    fixed_time().unix_timestamp() * 1_000 + i64::from(fixed_time().millisecond())
}

// 原測試：沒有初始 token 時先用 Client Secret 取得 token 再查詢
#[tokio::test]
async fn fetches_token_with_client_secret_before_first_query() {
    let (transport, calls) = MockTransport::sequence(vec![
        MockStep::Respond(json_response(&serde_json::json!({
            "access_token": "initial-app-token",
            "expires_in": 5_000_000,
            "token_type": "bearer",
        }))),
        MockStep::Respond(json_response(&serde_json::json!({ "data": [] }))),
    ]);
    let client = create_client(transport, recording_logger(), |options| {
        options.access_token = Some(String::new());
        options.client_secret = Some(CLIENT_SECRET.to_string());
    });

    client
        .get_live_statuses(&channels(&["streamer"]))
        .await
        .expect("getLiveStatuses 應成功");

    let calls = calls.lock().expect("calls lock");
    assert_eq!(calls.len(), 2);
    assert!(calls[0].url.contains("/oauth2/token"));
    assert_eq!(authorization_header(&calls[1]), "Bearer initial-app-token");
}

// 原測試：有 client secret 時每小時驗證有效 token，未近到期不換發
#[tokio::test]
async fn validates_valid_token_hourly_and_keeps_it_until_near_expiry() {
    let (transport, calls) = MockTransport::sequence(vec![
        MockStep::Respond(json_response(&serde_json::json!({
            "client_id": CLIENT_ID,
            "expires_in": 100_000,
        }))),
        MockStep::Respond(json_response(&serde_json::json!({ "data": [] }))),
    ]);
    let client = create_client(transport, recording_logger(), |options| {
        options.client_secret = Some(CLIENT_SECRET.to_string());
    });

    client
        .get_live_statuses(&channels(&["streamer"]))
        .await
        .expect("getLiveStatuses 應成功");

    let calls = calls.lock().expect("calls lock");
    assert_eq!(calls.len(), 2);
    assert!(calls[0].url.contains("/oauth2/validate"));
    assert_eq!(
        authorization_header(&calls[0]),
        format!("OAuth {ACCESS_TOKEN}")
    );
    assert!(calls[1].url.contains("/helix/streams"));
}

// 原測試：token 剩餘少於一天時自動換發後使用新 token 查詢
#[tokio::test]
async fn refreshes_token_when_expiry_is_within_one_day() {
    let (transport, calls) = MockTransport::sequence(vec![
        MockStep::Respond(json_response(&serde_json::json!({
            "client_id": CLIENT_ID,
            "expires_in": 3_600,
        }))),
        MockStep::Respond(json_response(&serde_json::json!({
            "access_token": "new-app-token",
            "expires_in": 5_000_000,
            "token_type": "bearer",
        }))),
        MockStep::Respond(json_response(&serde_json::json!({ "data": [] }))),
    ]);
    let client = create_client(transport, recording_logger(), |options| {
        options.client_secret = Some(CLIENT_SECRET.to_string());
    });

    client
        .get_live_statuses(&channels(&["streamer"]))
        .await
        .expect("getLiveStatuses 應成功");

    let calls = calls.lock().expect("calls lock");
    assert_eq!(calls[1].method, "POST");
    assert!(calls[1].url.contains("/oauth2/token"));
    let body = calls[1].body.as_deref().expect("token request body");
    assert!(body.contains("client_secret=fixture-client-secret"));
    assert_eq!(authorization_header(&calls[2]), "Bearer new-app-token");
}

// 原測試：Helix 回傳 401 時換發 token 並只重試一次
#[tokio::test]
async fn refreshes_token_and_retries_once_on_helix_401() {
    let (transport, calls) = MockTransport::sequence(vec![
        MockStep::Respond(json_response(&serde_json::json!({
            "client_id": CLIENT_ID,
            "expires_in": 100_000,
        }))),
        MockStep::Respond(status_response(401)),
        MockStep::Respond(json_response(&serde_json::json!({
            "access_token": "recovered-token",
            "expires_in": 5_000_000,
            "token_type": "bearer",
        }))),
        MockStep::Respond(json_response(&serde_json::json!({ "data": [] }))),
    ]);
    let client = create_client(transport, recording_logger(), |options| {
        options.client_secret = Some(CLIENT_SECRET.to_string());
    });

    let statuses = client
        .get_live_statuses(&channels(&["streamer"]))
        .await
        .expect("getLiveStatuses 應在換發後成功");

    assert_eq!(statuses.len(), 1);
    let calls = calls.lock().expect("calls lock");
    assert_eq!(calls.len(), 4);
    assert_eq!(authorization_header(&calls[3]), "Bearer recovered-token");
}

// 原測試：使用 URLSearchParams 與必要 headers 查詢多頻道並轉換部分 live
#[tokio::test]
async fn queries_multi_channel_with_url_search_params_and_required_headers() {
    let (transport, calls) =
        MockTransport::always(MockStep::Respond(json_response(&serde_json::json!({
            "data": [
                helix_stream(&[
                    ("id", serde_json::json!("stream-two")),
                    ("user_login", serde_json::json!("streamer_two")),
                    ("title", serde_json::json!("Second stream")),
                    ("started_at", serde_json::json!("2026-06-14T11:30:00.000Z")),
                    ("viewer_count", serde_json::json!(42)),
                ]),
                helix_stream(&[
                    ("id", serde_json::json!("stream-one")),
                    ("user_login", serde_json::json!("streamer_one")),
                    ("title", serde_json::json!("First stream")),
                    ("started_at", serde_json::json!("2026-06-14T11:00:00.000Z")),
                    ("viewer_count", serde_json::json!(100)),
                ]),
            ],
        }))));
    let client = create_client(transport, recording_logger(), |_| {});

    let statuses = client
        .get_live_statuses(&channels(&[
            "streamer_one",
            "offline_channel",
            "streamer_two",
        ]))
        .await
        .expect("getLiveStatuses 應成功");

    // 欄位逐一對應（照舊測試 toEqual 的完整物件）。
    assert_eq!(
        statuses,
        vec![
            live_status(
                "streamer_one",
                Some((
                    "stream-one",
                    "First stream",
                    "2026-06-14T11:00:00.000Z",
                    100
                ))
            ),
            live_status("offline_channel", None),
            live_status(
                "streamer_two",
                Some((
                    "stream-two",
                    "Second stream",
                    "2026-06-14T11:30:00.000Z",
                    42
                ))
            ),
        ],
    );

    let calls = calls.lock().expect("calls lock");
    assert_eq!(calls.len(), 1);
    let url = Url::parse(&calls[0].url).expect("URL 應可解析");
    // 舊測試的 url.origin／url.pathname。
    assert_eq!(url.scheme(), "https");
    assert_eq!(url.host_str(), Some("api.twitch.tv"));
    assert_eq!(url.path(), "/helix/streams");
    // 舊測試的 url.searchParams.getAll('user_login')。
    assert_eq!(
        query_pairs(&url).get("user_login").expect("user_login"),
        &vec![
            "streamer_one".to_string(),
            "offline_channel".to_string(),
            "streamer_two".to_string(),
        ],
    );
    assert_eq!(calls[0].method, "GET");
    // 舊測試的 init.redirect === 'error'：屬預設 transport 的設定，
    // 由下方 ReqwestTransport 的 redirect 測試涵蓋（mock transport 看不到 init）。
    assert_eq!(header_of(&calls[0], "Client-Id"), Some(CLIENT_ID));
    assert_eq!(
        header_of(&calls[0], "Authorization"),
        Some("Bearer fixture-access-token")
    );
}

// 原測試：data 為空時依設定順序回傳所有 offline
#[tokio::test]
async fn returns_all_offline_in_configured_order_when_data_is_empty() {
    let (transport, _calls) = MockTransport::always(MockStep::Respond(json_response(
        &serde_json::json!({ "data": [] }),
    )));
    let client = create_client(transport, recording_logger(), |_| {});

    let statuses = client
        .get_live_statuses(&channels(&["one", "two", "three"]))
        .await
        .expect("getLiveStatuses 應成功");

    assert_eq!(
        statuses,
        vec![
            live_status("one", None),
            live_status("two", None),
            live_status("three", None),
        ],
    );
}

// 原測試：API 回傳順序不同與 login 大小寫不同時仍保持設定順序
#[tokio::test]
async fn keeps_configured_order_when_api_order_and_login_case_differ() {
    let (transport, _calls) = MockTransport::always(MockStep::Respond(json_response(
        &serde_json::json!({
            "data": [
                helix_stream(&[("id", serde_json::json!("third")), ("user_login", serde_json::json!("THREE"))]),
                helix_stream(&[("id", serde_json::json!("first")), ("user_login", serde_json::json!("one"))]),
            ],
        }),
    )));
    let client = create_client(transport, recording_logger(), |_| {});

    let statuses = client
        .get_live_statuses(&channels(&["One", "two", "three"]))
        .await
        .expect("getLiveStatuses 應成功");

    let summary: Vec<(&str, bool)> = statuses
        .iter()
        .map(|status| (status.channel.as_str(), status.is_live))
        .collect();
    assert_eq!(
        summary,
        vec![("One", true), ("two", false), ("three", true)],
    );
}

// 原測試：超過 100 個 user_login 時分批查詢且保持完整設定順序
#[tokio::test]
async fn batches_queries_beyond_100_user_logins_and_keeps_full_order() {
    let channels: Vec<String> = (0..205).map(|index| format!("channel_{index}")).collect();
    let (transport, calls) = MockTransport::always(MockStep::Respond(json_response(
        &serde_json::json!({ "data": [] }),
    )));
    let client = create_client(transport, recording_logger(), |_| {});

    let statuses = client
        .get_live_statuses(&channels)
        .await
        .expect("getLiveStatuses 應成功");

    let calls = calls.lock().expect("calls lock");
    assert_eq!(calls.len(), 3);
    let counts: Vec<usize> = calls
        .iter()
        .map(|call| query_pairs(&Url::parse(&call.url).expect("URL 應可解析"))["user_login"].len())
        .collect();
    assert_eq!(counts, vec![100, 100, 5]);
    let all: Vec<String> = calls
        .iter()
        .flat_map(|call| {
            query_pairs(&Url::parse(&call.url).expect("URL 應可解析"))["user_login"].clone()
        })
        .collect();
    assert_eq!(all, channels);
    let reported: Vec<&str> = statuses
        .iter()
        .map(|status| status.channel.as_str())
        .collect();
    assert_eq!(
        reported,
        channels.iter().map(String::as_str).collect::<Vec<_>>()
    );
}

// 原測試：空頻道清單不發送 request
#[tokio::test]
async fn does_not_send_request_for_empty_channel_list() {
    // 沒有任何步驟：只要 transport 被呼叫就會 panic（unexpected extra fetch call）。
    let (transport, calls) = MockTransport::sequence(Vec::new());
    let client = create_client(transport, recording_logger(), |_| {});

    let statuses = client
        .get_live_statuses(&[])
        .await
        .expect("getLiveStatuses 應成功");

    assert_eq!(statuses, Vec::new());
    assert!(calls.lock().expect("calls lock").is_empty());
}

// 原測試：HTTP %s 記錄 auth 事件並拋出可分類錯誤
// it.each([401, 403])
#[tokio::test]
async fn logs_auth_event_and_throws_classifiable_error_for_http_status() {
    for status_code in [401u16, 403] {
        let (transport, calls) =
            MockTransport::always(MockStep::Respond(status_response(status_code)));
        let logger = recording_logger();
        let client = create_client(transport, logger.clone(), |_| {});

        let error = expect_error(client.get_live_statuses(&channels(&["streamer"])).await);

        let TwitchApiError::Auth(auth_error) = &error else {
            panic!("expected TwitchApiAuthError, got {error:?}")
        };
        assert_eq!(auth_error.name(), "TwitchApiAuthError");
        assert_eq!(auth_error.status_code(), status_code);
        assert_eq!(auth_error.kind(), TwitchApiErrorKind::Auth);
        assert_eq!(calls.lock().expect("calls lock").len(), 1);
        let error_calls = logger.error_calls.lock().expect("calls lock");
        assert_eq!(
            *error_calls,
            vec![serde_json::json!({
                "event": "twitch_api_auth_failed",
                "fields": { "statusCode": status_code },
            })],
        );
    }
}

// 原測試：HTTP 429 優先使用 Ratelimit-Reset epoch seconds
#[tokio::test]
async fn rate_limit_prefers_ratelimit_reset_epoch_seconds() {
    let fixed_ms = fixed_time_ms();
    let retry_at_ms = fixed_ms + 90_000;
    let logger = recording_logger();
    let (transport, _calls) = MockTransport::always(MockStep::Respond(MockResponse {
        status: 429,
        // 舊測試：String(retryAt.getTime() / 1_000)，retryAt 是 FIXED_TIME + 90 秒。
        headers: vec![(
            "Ratelimit-Reset".to_string(),
            ((fixed_ms + 90_000) / 1_000).to_string(),
        )],
        body: MockBody::Bytes(Vec::new()),
    }));
    let client = create_client(transport, logger.clone(), |_| {});

    let error = expect_error(client.get_live_statuses(&channels(&["streamer"])).await);

    let TwitchApiError::RateLimit(rate_limit) = &error else {
        panic!("expected TwitchApiRateLimitError, got {error:?}")
    };
    assert_eq!(rate_limit.name(), "TwitchApiRateLimitError");
    assert_eq!(rate_limit.kind(), TwitchApiErrorKind::RateLimit);
    assert_eq!(rate_limit.retry_after_ms(), 90_000);
    assert_eq!(rate_limit.retry_at_ms(), retry_at_ms);
    let warn_calls = logger.warn_calls.lock().expect("calls lock");
    assert_eq!(
        *warn_calls,
        vec![serde_json::json!({
            "event": "twitch_api_rate_limited",
            "fields": {
                "statusCode": 429,
                "retryAt": "2026-06-14T12:01:30.000Z",
                "retryAfterMs": 90_000,
            },
        })],
    );
}

// 原測試：HTTP 429 遠期 Ratelimit-Reset 最多只退避五分鐘
#[tokio::test]
async fn rate_limit_caps_long_ratelimit_reset_at_five_minutes() {
    let fixed_ms = fixed_time_ms();
    let (transport, _calls) = MockTransport::always(MockStep::Respond(MockResponse {
        status: 429,
        headers: vec![("Ratelimit-Reset".to_string(), "253402300799".to_string())],
        body: MockBody::Bytes(Vec::new()),
    }));
    let client = create_client(transport, recording_logger(), |_| {});

    let error = expect_error(client.get_live_statuses(&channels(&["streamer"])).await);

    let TwitchApiError::RateLimit(rate_limit) = &error else {
        panic!("expected TwitchApiRateLimitError, got {error:?}")
    };
    assert_eq!(rate_limit.kind(), TwitchApiErrorKind::RateLimit);
    assert_eq!(rate_limit.retry_after_ms(), 300_000);
    assert_eq!(rate_limit.retry_at_ms(), fixed_ms + 300_000);
}

// 原測試：HTTP 429 reset 無法解析時以 %s 秒輪詢間隔計算 fallback
// it.each([[60, 120_000], [200, 300_000]])
#[tokio::test]
async fn rate_limit_fallback_uses_check_interval_seconds_when_reset_unparseable() {
    const CASES: [(f64, u64); 2] = [(60.0, 120_000), (200.0, 300_000)];
    let fixed_ms = fixed_time_ms();
    for (check_interval_seconds, expected_retry_after_ms) in CASES {
        let (transport, _calls) = MockTransport::always(MockStep::Respond(MockResponse {
            status: 429,
            headers: vec![("Ratelimit-Reset".to_string(), "invalid".to_string())],
            body: MockBody::Bytes(Vec::new()),
        }));
        let client = create_client(transport, recording_logger(), |options| {
            options.check_interval_seconds = check_interval_seconds;
        });

        let error = expect_error(client.get_live_statuses(&channels(&["streamer"])).await);

        let TwitchApiError::RateLimit(rate_limit) = &error else {
            panic!("expected TwitchApiRateLimitError, got {error:?}")
        };
        assert_eq!(rate_limit.kind(), TwitchApiErrorKind::RateLimit);
        assert_eq!(rate_limit.retry_after_ms(), expected_retry_after_ms);
        assert_eq!(
            rate_limit.retry_at_ms(),
            fixed_ms + i64::try_from(expected_retry_after_ms).expect("非負毫秒"),
        );
    }
}

// 原測試：HTTP %s 拋出 temporary server 錯誤
// it.each([500, 503])
#[tokio::test]
async fn throws_temporary_server_error_for_http_status() {
    for status_code in [500u16, 503] {
        let (transport, _calls) =
            MockTransport::always(MockStep::Respond(status_response(status_code)));
        let client = create_client(transport, recording_logger(), |_| {});

        let error = expect_temporary(client.get_live_statuses(&channels(&["streamer"])).await);

        assert_eq!(error.name(), "TwitchApiTemporaryError");
        assert_eq!(error.kind(), TwitchApiErrorKind::Temporary);
        assert_eq!(error.reason(), TwitchApiTemporaryErrorReason::Server);
        assert_eq!(error.status_code(), Some(status_code));
    }
}

// 原測試：request timeout 會中止 fetch 並拋出 temporary timeout 錯誤
// 舊測試以 fake timers 推進 AbortController 的 setTimeout；Rust 版以
// start_paused 的虛擬時間推進 tokio::time::timeout。
#[tokio::test(start_paused = true)]
async fn aborts_fetch_on_request_timeout_and_throws_temporary_timeout() {
    let (transport, _calls) = MockTransport::always(MockStep::Hang);
    let client = create_client(transport, recording_logger(), |options| {
        options.request_timeout_ms = Some(1_000.0);
    });

    let error = expect_temporary(client.get_live_statuses(&channels(&["streamer"])).await);

    assert_eq!(error.name(), "TwitchApiTemporaryError");
    assert_eq!(error.kind(), TwitchApiErrorKind::Temporary);
    assert_eq!(error.reason(), TwitchApiTemporaryErrorReason::Timeout);
}

// 原測試：request timeout 會涵蓋收到 headers 後的 response body
#[tokio::test(start_paused = true)]
async fn request_timeout_covers_response_body_after_headers() {
    let (transport, _calls) = MockTransport::always(MockStep::Respond(MockResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        // body 串流永不結束：對應舊測試一直不完成、最後被 abort 的 ReadableStream。
        body: MockBody::Pending,
    }));
    let client = create_client(transport, recording_logger(), |options| {
        options.request_timeout_ms = Some(1_000.0);
    });

    let error = expect_temporary(client.get_live_statuses(&channels(&["streamer"])).await);

    assert_eq!(error.name(), "TwitchApiTemporaryError");
    assert_eq!(error.kind(), TwitchApiErrorKind::Temporary);
    assert_eq!(error.reason(), TwitchApiTemporaryErrorReason::Timeout);
}

// 原測試：拒絕超過一百萬 bytes 的 response body
#[tokio::test]
async fn rejects_response_body_larger_than_one_million_bytes() {
    let (transport, _calls) = MockTransport::always(MockStep::Respond(MockResponse {
        status: 200,
        headers: vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Content-Length".to_string(), "1000001".to_string()),
        ],
        body: MockBody::Bytes(b"x".to_vec()),
    }));
    let client = create_client(transport, recording_logger(), |_| {});

    let error = expect_temporary(client.get_live_statuses(&channels(&["streamer"])).await);

    assert_eq!(error.name(), "TwitchApiTemporaryError");
    assert_eq!(error.kind(), TwitchApiErrorKind::Temporary);
    assert_eq!(
        error.reason(),
        TwitchApiTemporaryErrorReason::InvalidResponse
    );
}

// 原測試：fetch AbortError 分類為 temporary aborted 錯誤
#[tokio::test]
async fn classifies_fetch_abort_error_as_temporary_aborted() {
    let (transport, _calls) = MockTransport::always(MockStep::Fail(TwitchTransportError::Aborted));
    let client = create_client(transport, recording_logger(), |_| {});

    let error = expect_temporary(client.get_live_statuses(&channels(&["streamer"])).await);

    assert_eq!(error.name(), "TwitchApiTemporaryError");
    assert_eq!(error.kind(), TwitchApiErrorKind::Temporary);
    assert_eq!(error.reason(), TwitchApiTemporaryErrorReason::Aborted);
}

// 原測試：網路錯誤分類為 temporary 且不轉送原始錯誤內容
#[tokio::test]
async fn classifies_network_error_as_temporary_without_forwarding_details() {
    /// 舊測試 mock 的錯誤訊息內含的機密。
    const SECRET: &str = "network-error-secret";
    let (transport, _calls) = MockTransport::always(MockStep::Fail(TwitchTransportError::Network));
    let logger = recording_logger();
    let client = create_client(transport, logger.clone(), |options| {
        options.access_token = Some(SECRET.to_string());
    });

    let error = expect_error(client.get_live_statuses(&channels(&["streamer"])).await);
    let TwitchApiError::Temporary(temporary) = &error else {
        panic!("expected temporary error, got {error:?}")
    };
    let serialized = serialize_error_and_logs(&error, &logger);

    assert_eq!(temporary.reason(), TwitchApiTemporaryErrorReason::Network);
    assert!(!serialized.contains(SECRET));
    assert!(!serialized.contains("Authorization"));
    assert!(!serialized.contains("Bearer"));
}

// 原測試：無法解析的 JSON 分類為 temporary invalid-response
#[tokio::test]
async fn classifies_unparseable_json_as_temporary_invalid_response() {
    let (transport, _calls) = MockTransport::always(MockStep::Respond(MockResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: MockBody::Bytes(b"{\"data\":[".to_vec()),
    }));
    let client = create_client(transport, recording_logger(), |_| {});

    let error = expect_temporary(client.get_live_statuses(&channels(&["streamer"])).await);

    assert_eq!(error.name(), "TwitchApiTemporaryError");
    assert_eq!(error.kind(), TwitchApiErrorKind::Temporary);
    assert_eq!(
        error.reason(),
        TwitchApiTemporaryErrorReason::InvalidResponse
    );
    assert_eq!(error.status_code(), Some(200));
}

// 原測試：%s 時保守拒絕 response schema
// it.each 的 6 個案例（標題、payload）照舊測試順序。
#[tokio::test]
async fn rejects_response_schema_conservatively() {
    let cases = vec![
        ("缺少 data", serde_json::json!({})),
        ("data 不是陣列", serde_json::json!({ "data": {} })),
        (
            "stream 欄位缺少",
            serde_json::json!({
                "data": [{ "id": "stream-id", "user_login": "streamer" }],
            }),
        ),
        (
            "viewer_count 不是非負整數",
            serde_json::json!({
                "data": [helix_stream(&[("user_login", serde_json::json!("streamer")), ("viewer_count", serde_json::json!(-1))])],
            }),
        ),
        (
            "started_at 不是時間",
            serde_json::json!({
                "data": [helix_stream(&[("user_login", serde_json::json!("streamer")), ("started_at", serde_json::json!("not-a-time"))])],
            }),
        ),
        (
            "同一 login 重複",
            serde_json::json!({
                "data": [
                    helix_stream(&[("id", serde_json::json!("one")), ("user_login", serde_json::json!("streamer"))]),
                    helix_stream(&[("id", serde_json::json!("two")), ("user_login", serde_json::json!("STREAMER"))]),
                ],
            }),
        ),
    ];
    for (case_name, payload) in cases {
        let (transport, _calls) = MockTransport::always(MockStep::Respond(json_response(&payload)));
        let client = create_client(transport, recording_logger(), |_| {});

        let error = expect_temporary(client.get_live_statuses(&channels(&["streamer"])).await);

        assert_eq!(error.name(), "TwitchApiTemporaryError", "case {case_name}");
        assert_eq!(
            error.kind(),
            TwitchApiErrorKind::Temporary,
            "case {case_name}"
        );
        assert_eq!(
            error.reason(),
            TwitchApiTemporaryErrorReason::InvalidResponse,
            "case {case_name}"
        );
    }
}

// 原測試：HTTP %s 的 error 與 logger fields 不洩漏 token 或 headers
// it.each([401, 403, 429, 500])
#[tokio::test]
async fn error_and_logger_fields_do_not_leak_token_or_headers_for_http_status() {
    for status_code in [401u16, 403, 429, 500] {
        let secret = format!("secret-for-{status_code}");
        let headers = if status_code == 429 {
            vec![("Ratelimit-Reset".to_string(), "invalid".to_string())]
        } else {
            Vec::new()
        };
        let (transport, _calls) = MockTransport::always(MockStep::Respond(MockResponse {
            status: status_code,
            headers,
            body: MockBody::Bytes(Vec::new()),
        }));
        let logger = recording_logger();
        let client = create_client(transport, logger.clone(), |options| {
            options.access_token = Some(secret.clone());
        });

        let error = expect_error(client.get_live_statuses(&channels(&["streamer"])).await);
        let serialized = serialize_error_and_logs(&error, &logger);

        assert!(!serialized.contains(&secret), "status {status_code}");
        assert!(
            !serialized.contains("Authorization"),
            "status {status_code}"
        );
        assert!(!serialized.contains("Client-Id"), "status {status_code}");
        assert!(!serialized.contains("Bearer"), "status {status_code}");
    }
}

// ===== 直接測試（純邏輯：URL 組裝、回應解析、分頁與 rate limit 換算） =====

// 直接測試：自訂 baseUrl 缺結尾 '/' 時先補 '/' 再解析（照舊版 new URL 行為）
#[tokio::test]
async fn assembles_streams_url_from_custom_base_without_trailing_slash() {
    let (transport, calls) = MockTransport::always(MockStep::Respond(json_response(
        &serde_json::json!({ "data": [] }),
    )));
    let client = create_client(transport, recording_logger(), |options| {
        options.base_url = Some("https://api.example.com/helix".to_string());
    });

    client
        .get_live_statuses(&channels(&["streamer"]))
        .await
        .expect("getLiveStatuses 應成功");

    let calls = calls.lock().expect("calls lock");
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].url,
        "https://api.example.com/helix/streams?user_login=streamer"
    );
}

// 直接測試：user_login 以 application/x-www-form-urlencoded 編碼（空格 → '+'，
// 對應 URLSearchParams.toString() 的序列化）
#[tokio::test]
async fn encodes_user_login_query_with_form_urlencoding() {
    let (transport, calls) = MockTransport::always(MockStep::Respond(json_response(
        &serde_json::json!({ "data": [] }),
    )));
    let client = create_client(transport, recording_logger(), |_| {});

    client
        .get_live_statuses(&channels(&["a b"]))
        .await
        .expect("getLiveStatuses 應成功");

    let calls = calls.lock().expect("calls lock");
    assert_eq!(
        calls[0].url,
        "https://api.twitch.tv/helix/streams?user_login=a+b"
    );
}

// 直接測試：Helix 串流的欄位解析（title 允許空字串、viewer_count 的安全整數邊界）
#[test]
fn parses_helix_stream_fields() {
    // viewer_count = 2^53 − 1（安全整數上限）應通過。
    let ok = super::parse_helix_streams(&serde_json::json!({
        "data": [helix_stream(&[("title", serde_json::json!("")), ("viewer_count", serde_json::json!(9_007_199_254_740_991i64))])],
    }))
    .expect("應可解析");
    assert_eq!(ok[0].id, "stream-id");
    assert_eq!(ok[0].user_login, "streamer");
    assert_eq!(ok[0].title, "");
    assert_eq!(ok[0].started_at, "2026-06-14T11:00:00.000Z");
    assert_eq!(ok[0].viewer_count, 9_007_199_254_740_991);

    // viewer_count = 2^53（超出安全整數）應拒絕（照舊版 Number.isSafeInteger）。
    assert!(super::parse_helix_streams(&serde_json::json!({
        "data": [helix_stream(&[("viewer_count", serde_json::json!(9_007_199_254_740_992i64))])],
    }))
    .is_err());

    // 頂層不是物件（陣列）應拒絕。
    assert!(super::parse_helix_streams(&serde_json::json!([1, 2])).is_err());
}

// 直接測試：parseResetEpochSeconds 的邊界（回傳值是「秒」，照舊版）
#[test]
fn parses_reset_epoch_seconds_boundaries() {
    assert_eq!(
        super::parse_reset_epoch_seconds(Some("1781512800")),
        Some(1_781_512_800)
    );
    assert_eq!(super::parse_reset_epoch_seconds(Some("007")), Some(7));
    // 非純數字。
    assert_eq!(super::parse_reset_epoch_seconds(Some("invalid")), None);
    assert_eq!(super::parse_reset_epoch_seconds(Some("12x")), None);
    assert_eq!(super::parse_reset_epoch_seconds(Some("")), None);
    assert_eq!(super::parse_reset_epoch_seconds(Some("1.5")), None);
    assert_eq!(super::parse_reset_epoch_seconds(None), None);
    // 超出 Date 範圍（epoch ms 超過 ±8.64e15）。
    assert_eq!(
        super::parse_reset_epoch_seconds(Some("99999999999999")),
        None
    );
}

// 直接測試：normalizeChannel 的大小寫正規化
#[test]
fn normalizes_channel_to_lowercase() {
    assert_eq!(super::normalize_channel("Streamer_One"), "streamer_one");
}

// 直接測試：ReqwestTransport 對本機 HTTP 伺服器的端到端行為
// （HTTP/1.1 收發、headers、body 串流與 JSON 解析；不連真實 Twitch）
#[tokio::test]
async fn reqwest_transport_reads_json_from_local_server() {
    let port = spawn_local_server(LocalServerResponse::Json(HELIX_PAYLOAD));
    let transport = Arc::new(ReqwestTransport::new().expect("transport 應可建立"))
        as Arc<dyn TwitchHttpTransport>;
    let client = create_client(transport, recording_logger(), |options| {
        options.base_url = Some(format!("http://127.0.0.1:{port}/helix/"));
    });

    let statuses = client
        .get_live_statuses(&channels(&["streamer"]))
        .await
        .expect("getLiveStatuses 應成功");

    assert_eq!(statuses.len(), 1);
    assert!(statuses[0].is_live);
    assert_eq!(statuses[0].stream_id.as_deref(), Some("stream-id"));
    assert_eq!(statuses[0].viewer_count, Some(10));
}

// 直接測試：ReqwestTransport 拒絕 redirect（對應舊版 fetch 的 redirect: 'error'）
#[tokio::test]
async fn reqwest_transport_rejects_redirect() {
    let port = spawn_local_server(LocalServerResponse::Redirect {
        status: 302,
        location: "/elsewhere",
    });
    let transport = Arc::new(ReqwestTransport::new().expect("transport 應可建立"))
        as Arc<dyn TwitchHttpTransport>;
    let client = create_client(transport, recording_logger(), |options| {
        options.base_url = Some(format!("http://127.0.0.1:{port}/helix/"));
    });

    let error = expect_temporary(client.get_live_statuses(&channels(&["streamer"])).await);

    // 舊版 fetch 的 redirect: 'error' 會 reject → fetchWithTimeout 的 catch →
    // temporary 'network'。
    assert_eq!(error.reason(), TwitchApiTemporaryErrorReason::Network);
}

// ===== 測試輔助（對應舊測試的 mock 與 fixtures） =====

/// 本機伺服器回傳的 Helix payload（照舊測試 helixStream 的預設值）。
const HELIX_PAYLOAD: &str = r#"{"data":[{"id":"stream-id","user_login":"streamer","title":"Live title","started_at":"2026-06-14T11:00:00.000Z","viewer_count":10}]}"#;

/// 舊測試 `vi.fn<typeof fetch>()` 的逐步行為。
#[derive(Clone)]
enum MockStep {
    /// `mockResolvedValueOnce(response)`。
    Respond(MockResponse),
    /// `mockRejectedValue(error)`（transport 層失敗）。
    Fail(TwitchTransportError),
    /// 永不完成（對應舊測試用 signal 控制、等逾時 abort 的 pending fetch）。
    Hang,
}

/// 舊測試 `new Response(...)` 的 mock。
#[derive(Clone)]
struct MockResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: MockBody,
}

/// mock 的 body 形態。
#[derive(Clone)]
enum MockBody {
    /// 一次送完的 body（`new Response(JSON.stringify(...))`）。
    Bytes(Vec<u8>),
    /// 永不完成的 body（「timeout 會涵蓋 response body」的場景）。
    Pending,
}

/// 舊測試的呼叫記錄（對應 `fetchMock.mock.calls`）。
type FetchCalls = Arc<Mutex<Vec<TwitchHttpRequest>>>;

/// 注入的 mock transport（對應舊測試的 fetch mock）。
struct MockTransport {
    steps: Mutex<VecDeque<MockStep>>,
    /// 步驟用完時重複最後一步（對應 `mockResolvedValue` 恆回同一值）。
    repeat_last: bool,
    calls: FetchCalls,
}

impl MockTransport {
    /// 逐次取用步驟的 mock（`mockResolvedValueOnce` 的序列）。
    fn sequence(steps: Vec<MockStep>) -> (Arc<dyn TwitchHttpTransport>, FetchCalls) {
        Self::with_steps(steps, false)
    }

    /// 恆回同一值的 mock（`mockResolvedValue`／`mockImplementation`）。
    fn always(step: MockStep) -> (Arc<dyn TwitchHttpTransport>, FetchCalls) {
        Self::with_steps(vec![step], true)
    }

    fn with_steps(
        steps: Vec<MockStep>,
        repeat_last: bool,
    ) -> (Arc<dyn TwitchHttpTransport>, FetchCalls) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let transport = Arc::new(Self {
            steps: Mutex::new(steps.into()),
            repeat_last,
            calls: Arc::clone(&calls),
        });
        (transport, calls)
    }
}

#[async_trait::async_trait]
impl TwitchHttpTransport for MockTransport {
    async fn fetch(
        &self,
        request: TwitchHttpRequest,
    ) -> Result<TwitchHttpResponse, TwitchTransportError> {
        self.calls.lock().expect("calls lock").push(request);
        let step = {
            let mut steps = self.steps.lock().expect("steps lock");
            if steps.is_empty() {
                None
            } else if self.repeat_last && steps.len() == 1 {
                // mockResolvedValue：唯一的步驟取用不移除（恆回同一值）。
                steps.front().cloned()
            } else {
                steps.pop_front()
            }
        };
        match step {
            None => panic!("unexpected extra fetch call"),
            Some(MockStep::Hang) => std::future::pending().await,
            Some(MockStep::Fail(error)) => Err(error),
            Some(MockStep::Respond(response)) => Ok(TwitchHttpResponse {
                status: response.status,
                headers: TwitchHttpHeaders::new(response.headers),
                body: match response.body {
                    MockBody::Bytes(bytes) => Box::new(MockHttpBody {
                        chunks: VecDeque::from(vec![bytes]),
                    }),
                    MockBody::Pending => Box::new(PendingBody),
                },
            }),
        }
    }
}

/// 一次送完的 mock body。
struct MockHttpBody {
    chunks: VecDeque<Vec<u8>>,
}

#[async_trait::async_trait]
impl super::TwitchHttpBody for MockHttpBody {
    async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, TwitchTransportError>> {
        self.chunks.pop_front().map(Ok)
    }
}

/// 永不完成的 mock body（逾時由 client 端觸發）。
struct PendingBody;

#[async_trait::async_trait]
impl super::TwitchHttpBody for PendingBody {
    async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, TwitchTransportError>> {
        std::future::pending().await
    }
}

/// 舊測試的 `jsonResponse(payload)`。
fn json_response(payload: &serde_json::Value) -> MockResponse {
    MockResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: MockBody::Bytes(payload.to_string().into_bytes()),
    }
}

/// 舊測試的 `new Response(null, { status })`。
fn status_response(status: u16) -> MockResponse {
    MockResponse {
        status,
        headers: Vec::new(),
        body: MockBody::Bytes(Vec::new()),
    }
}

/// 舊測試的 `helixStream(overrides)`。
fn helix_stream(overrides: &[(&str, serde_json::Value)]) -> serde_json::Value {
    let mut stream = serde_json::json!({
        "id": "stream-id",
        "user_login": "streamer",
        "title": "Live title",
        "started_at": "2026-06-14T11:00:00.000Z",
        "viewer_count": 10,
    });
    if let serde_json::Value::Object(entries) = &mut stream {
        for (key, value) in overrides {
            entries.insert((*key).to_string(), value.clone());
        }
    }
    stream
}

/// 舊測試 createClient 預設的 offline／live 狀態（checkedAt 為 FIXED_TIME）。
fn live_status(channel: &str, stream: Option<(&str, &str, &str, u64)>) -> ChannelLiveStatus {
    let channel = channel.to_string();
    let checked_at = FIXED_TIME_ISO.to_string();
    match stream {
        Some((stream_id, title, started_at, viewer_count)) => ChannelLiveStatus {
            channel,
            is_live: true,
            stream_id: Some(stream_id.to_string()),
            title: Some(title.to_string()),
            started_at: Some(started_at.to_string()),
            viewer_count: Some(viewer_count),
            checked_at,
        },
        None => ChannelLiveStatus {
            channel,
            is_live: false,
            stream_id: None,
            title: None,
            started_at: None,
            viewer_count: None,
            checked_at,
        },
    }
}

/// 舊測試的 logger mock：記錄四個層級的呼叫（`{"event", "fields"}`）。
#[derive(Default)]
struct RecordingLogger {
    debug_calls: RecordedCalls,
    info_calls: RecordedCalls,
    warn_calls: RecordedCalls,
    error_calls: RecordedCalls,
}

/// 記錄緩衝區。
type RecordedCalls = Arc<Mutex<Vec<serde_json::Value>>>;

impl RecordingLogger {
    fn push(calls: &RecordedCalls, event: &str, fields: Option<&SanitizedValue>) {
        let fields_json = fields.map_or_else(|| "null".to_string(), SanitizedValue::to_json_string);
        let fields =
            serde_json::from_str::<serde_json::Value>(&fields_json).expect("欄位 JSON 應可解析");
        calls
            .lock()
            .expect("calls lock")
            .push(serde_json::json!({ "event": event, "fields": fields }));
    }
}

impl TwitchApiLogger for RecordingLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        Self::push(&self.debug_calls, event, fields);
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        Self::push(&self.info_calls, event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        Self::push(&self.warn_calls, event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        Self::push(&self.error_calls, event, fields);
    }
}

/// 建立收集用的 logger（對應舊測試的 `createLoggerMock()`）。
fn recording_logger() -> Arc<RecordingLogger> {
    Arc::new(RecordingLogger::default())
}

/// 舊測試的 `createClient(overrides)`；logger 與 transport 必給
/// （分別對應 options.logger 與 options.fetch），其餘由 `customize` 調整。
fn create_client(
    transport: Arc<dyn TwitchHttpTransport>,
    logger: Arc<dyn TwitchApiLogger>,
    customize: impl FnOnce(&mut TwitchApiClientOptions),
) -> TwitchApiClient {
    let mut options = TwitchApiClientOptions {
        client_id: CLIENT_ID.to_string(),
        access_token: Some(ACCESS_TOKEN.to_string()),
        client_secret: None,
        check_interval_seconds: 60.0,
        logger,
        transport: Some(transport),
        base_url: None,
        oauth_base_url: None,
        request_timeout_ms: None,
        token_validation_interval_ms: None,
        token_refresh_threshold_seconds: None,
        now: Some(Box::new(fixed_time) as TwitchApiClock),
    };
    customize(&mut options);
    match TwitchApiClient::new(options) {
        Ok(client) => client,
        Err(error) => panic!("unexpected client init failure: {error:?}"),
    }
}

/// 舊測試的字串清單 → `Vec<String>`。
fn channels(list: &[&str]) -> Vec<String> {
    list.iter().map(|channel| (*channel).to_string()).collect()
}

/// 舊測試 `new Headers(init.headers).get(name)`：大小寫不敏感。
fn header_of<'a>(call: &'a TwitchHttpRequest, name: &str) -> Option<&'a str> {
    call.headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// 舊測試的 `headers.get('Authorization')`。
fn authorization_header(call: &TwitchHttpRequest) -> &str {
    header_of(call, "Authorization").expect("Authorization header")
}

/// URL query 的同名參數值清單（對應 `searchParams.getAll(name)`）。
fn query_pairs(url: &Url) -> std::collections::HashMap<String, Vec<String>> {
    let mut pairs: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (key, value) in url.query_pairs() {
        pairs
            .entry(key.into_owned())
            .or_default()
            .push(value.into_owned());
    }
    pairs
}

/// 舊測試的 `captureError`。
fn expect_error(result: Result<Vec<ChannelLiveStatus>, TwitchApiError>) -> TwitchApiError {
    match result {
        Err(error) => error,
        Ok(statuses) => panic!("expected an error, got {statuses:?}"),
    }
}

/// 取出 temporary 錯誤（對應舊測試 `rejects.toMatchObject` 的 kind 斷言）。
fn expect_temporary(
    result: Result<Vec<ChannelLiveStatus>, TwitchApiError>,
) -> TwitchApiTemporaryError {
    match expect_error(result) {
        TwitchApiError::Temporary(error) => error,
        other => panic!("expected temporary error, got {other:?}"),
    }
}

/// 舊測試的 `${String(error)} ${JSON.stringify(error)} ${JSON.stringify(loggerCalls)}`：
/// 以 Display 加上 name／message／kind 與分類欄位的 JSON，再接全部 logger 呼叫
/// （Rust 錯誤值的序列化形態不同，但「不洩漏」的斷言一致）。
fn serialize_error_and_logs(error: &TwitchApiError, logger: &RecordingLogger) -> String {
    let mut fields = vec![
        ("name".to_string(), serde_json::json!(error.name())),
        ("message".to_string(), serde_json::json!(error.message())),
        ("kind".to_string(), serde_json::json!(error.kind().as_str())),
    ];
    match error {
        TwitchApiError::Auth(auth_error) => {
            fields.push((
                "statusCode".to_string(),
                serde_json::json!(auth_error.status_code()),
            ));
        }
        TwitchApiError::RateLimit(rate_limit) => {
            fields.push((
                "retryAt".to_string(),
                serde_json::json!(iso_utc_string_from_epoch_ms(rate_limit.retry_at_ms())),
            ));
            fields.push((
                "retryAfterMs".to_string(),
                serde_json::json!(rate_limit.retry_after_ms()),
            ));
        }
        TwitchApiError::Temporary(temporary) => {
            fields.push((
                "reason".to_string(),
                serde_json::json!(temporary.reason().as_str()),
            ));
            if let Some(status) = temporary.status_code() {
                fields.push(("statusCode".to_string(), serde_json::json!(status)));
            }
        }
    }
    let fields = serde_json::Value::Object(fields.into_iter().collect());
    let mut serialized = format!("{} {}", error, fields);
    for calls in [
        &logger.debug_calls,
        &logger.info_calls,
        &logger.warn_calls,
        &logger.error_calls,
    ] {
        let lines = calls.lock().expect("calls lock");
        serialized.push(' ');
        serialized.push_str(&serde_json::to_string(&*lines).expect("呼叫記錄應可序列化"));
    }
    serialized
}

/// 本機 HTTP 伺服器的回應模式。
enum LocalServerResponse {
    /// 固定的 JSON body（200）。
    Json(&'static str),
    /// 帶 Location 的重導向（`redirect: 'error'` 的驗證）。
    Redirect { status: u16, location: &'static str },
}

/// 啟動本機 HTTP 伺服器（照 `src/tests/browser_diagnostics.rs` 的模式：
/// 逐條連線開執行緒處理、讀完請求後直接回應並關線）。
fn spawn_local_server(response: LocalServerResponse) -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap_or(0);
            if read == 0 {
                continue;
            }
            let (head, body): (String, &[u8]) = match &response {
                LocalServerResponse::Json(payload) => (
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    ),
                    payload.as_bytes(),
                ),
                LocalServerResponse::Redirect { status, location } => (
                    format!(
                        "HTTP/1.1 {status} Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    ),
                    b"",
                ),
            };
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
        }
    });
    port
}
