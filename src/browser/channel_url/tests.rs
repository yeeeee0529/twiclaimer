//! channel_url 消毒邏輯的單元測試：逐案例對照舊版 `test/unit/browser-diagnostics.test.ts`。
use super::{
    EndpointCategory, MAX_HOST_LENGTH, MAX_PATH_LENGTH, REDACTED_URL, SanitizedEndpoint,
    create_channel_url, join_sanitized_url, sanitize_browser_url,
};

/// 對應舊版 `{ host: REDACTED_URL, path: '', endpointCategory: 'unknown' }` 的期望值。
fn redacted() -> SanitizedEndpoint {
    SanitizedEndpoint {
        host: REDACTED_URL.to_string(),
        path: String::new(),
        endpoint_category: EndpointCategory::Unknown,
    }
}

#[test]
// 原測試：移除 query、fragment、credentials 與 port
fn removes_query_fragment_credentials_and_port() {
    let result = sanitize_browser_url(Some(
        "https://user:pass@static.twitchcdn.net:8443/assets/main.js?token=secret&challenge=abc#frag",
    ));
    assert_eq!(
        result,
        SanitizedEndpoint {
            host: "static.twitchcdn.net".to_string(),
            path: "/assets/main.js".to_string(),
            endpoint_category: EndpointCategory::TwitchJavascript,
        }
    );
}

#[test]
// 原測試：空值與非字串輸入回傳 redacted placeholder
fn nullish_and_empty_input_returns_redacted_placeholder() {
    // `None` 對應舊版的 `undefined` 與 `null`；`Some("")` 對應空字串。
    assert_eq!(sanitize_browser_url(None), redacted());
    assert_eq!(sanitize_browser_url(Some("")), redacted());
}

#[test]
// 原測試：無法解析的 URL 回傳 bounded 的 redacted 結果且不拋錯
fn unparseable_url_returns_bounded_redacted_result_without_panicking() {
    assert_eq!(sanitize_browser_url(Some("not a url")), redacted());
    assert_eq!(sanitize_browser_url(Some("https://")), redacted());
}

#[test]
// 原測試：query 值與敏感 token 永遠不會出現在輸出
fn query_values_and_sensitive_tokens_never_appear_in_output() {
    let result = sanitize_browser_url(Some(
        "https://k.twitchcdn.net/kauth.js?token=Bearer+secret&client_id=abc",
    ));
    assert_eq!(result.host, "k.twitchcdn.net");
    assert_eq!(result.path, "/kauth.js");
    let dumped = format!("{result:?}");
    assert!(!dumped.contains("secret"));
    assert!(!dumped.contains("client_id"));
}

#[test]
// 原測試：過長的 host 與 path 依上限裁切
fn oversized_host_and_path_are_truncated_to_limits() {
    let long_host = format!("sub.{}.example.com", "a".repeat(400));
    let long_path = format!("/{}.js", "b".repeat(400));
    let result = sanitize_browser_url(Some(&format!("https://{long_host}{long_path}?query=1")));
    assert_eq!(result.host.len(), MAX_HOST_LENGTH);
    assert_eq!(result.path.len(), MAX_PATH_LENGTH);
}

#[test]
// 原測試：%s 分類為 %s
fn classifies_www_twitch_tv_as_twitch_document() {
    assert_eq!(
        sanitize_browser_url(Some("https://www.twitch.tv/some_channel")).endpoint_category,
        EndpointCategory::TwitchDocument
    );
}

#[test]
// 原測試：%s 分類為 %s
fn classifies_m_twitch_tv_as_twitch_document() {
    assert_eq!(
        sanitize_browser_url(Some("https://m.twitch.tv/some_channel")).endpoint_category,
        EndpointCategory::TwitchDocument
    );
}

#[test]
// 原測試：%s 分類為 %s
fn classifies_static_twitchcdn_net_as_twitch_javascript() {
    assert_eq!(
        sanitize_browser_url(Some("https://static.twitchcdn.net/assets/main.js")).endpoint_category,
        EndpointCategory::TwitchJavascript
    );
}

#[test]
// 原測試：%s 分類為 %s
fn classifies_webpack_static_twitchcdn_net_as_twitch_javascript() {
    assert_eq!(
        sanitize_browser_url(Some("https://webpack.static.twitchcdn.net/main.123.js"))
            .endpoint_category,
        EndpointCategory::TwitchJavascript
    );
}

#[test]
// 原測試：%s 分類為 %s
fn classifies_assets_twitch_tv_as_twitch_javascript() {
    assert_eq!(
        sanitize_browser_url(Some("https://assets.twitch.tv/main.js")).endpoint_category,
        EndpointCategory::TwitchJavascript
    );
}

#[test]
// 原測試：%s 分類為 %s
fn classifies_static_cdn_jtvnw_net_as_twitch_javascript() {
    assert_eq!(
        sanitize_browser_url(Some("https://static-cdn.jtvnw.net/app.js")).endpoint_category,
        EndpointCategory::TwitchJavascript
    );
}

#[test]
// 原測試：%s 分類為 %s
fn classifies_gql_twitch_tv_as_twitch_graphql() {
    assert_eq!(
        sanitize_browser_url(Some("https://gql.twitch.tv/gql")).endpoint_category,
        EndpointCategory::TwitchGraphql
    );
}

#[test]
// 原測試：%s 分類為 %s
fn classifies_k_twitchcdn_net_as_twitch_anti_abuse() {
    assert_eq!(
        sanitize_browser_url(Some("https://k.twitchcdn.net/kauth.js")).endpoint_category,
        EndpointCategory::TwitchAntiAbuse
    );
}

#[test]
// 原測試：%s 分類為 %s
fn classifies_api_twitch_tv_as_twitch_api() {
    assert_eq!(
        sanitize_browser_url(Some("https://api.twitch.tv/helix/streams")).endpoint_category,
        EndpointCategory::TwitchApi
    );
}

#[test]
// 原測試：%s 分類為 %s
fn classifies_hls_ttvnw_net_as_twitch_media() {
    assert_eq!(
        sanitize_browser_url(Some(
            "https://video-weaver.atl01.hls.ttvnw.net/v1/playlist/segment.ts",
        ))
        .endpoint_category,
        EndpointCategory::TwitchMedia
    );
}

#[test]
// 原測試：%s 分類為 %s
fn classifies_passport_twitch_tv_as_twitch_other() {
    assert_eq!(
        sanitize_browser_url(Some("https://passport.twitch.tv/auth")).endpoint_category,
        EndpointCategory::TwitchOther
    );
}

#[test]
// 原測試：%s 分類為 %s
fn classifies_example_com_as_third_party() {
    assert_eq!(
        sanitize_browser_url(Some("https://example.com/analytics.js")).endpoint_category,
        EndpointCategory::ThirdParty
    );
}

#[test]
// 舊版無 joinSanitizedUrl 的專屬測試；此為補測：path 為空時只回傳 host。
fn joins_host_with_path_and_returns_host_only_when_path_is_empty() {
    let endpoint = sanitize_browser_url(Some("https://www.twitch.tv/some_channel?query=1"));
    assert_eq!(join_sanitized_url(&endpoint), "www.twitch.tv/some_channel");
    // redacted 結果的 path 為空字串，接合後只有佔位 host。
    let redacted_endpoint = sanitize_browser_url(None);
    assert_eq!(join_sanitized_url(&redacted_endpoint), REDACTED_URL);
}

// 原測試：createChannelUrl 使用 URL encoding 並拒絕不合法 channel
// （此測試出自 test/unit/channel-session.test.ts 的 helpers 段落；createChannelUrl
// 移植到本模組，故測試一起放在這裡。）
#[test]
fn create_channel_url_uses_url_encoding_and_rejects_invalid_channel() {
    assert_eq!(
        create_channel_url("Valid_Channel").unwrap(),
        "https://www.twitch.tv/Valid_Channel"
    );
    let error = create_channel_url("invalid/channel").unwrap_err();
    assert_eq!(error.name, "Error");
    assert_eq!(error.message, "Channel 必須是 1 到 25 字元的英數字或底線");
}
