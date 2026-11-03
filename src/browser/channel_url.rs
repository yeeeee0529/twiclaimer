//! 頻道 URL 的消毒工具：`sanitizeBrowserUrl`、`joinSanitizedUrl`（R9、§7.5）。
//!
//! 行為對照舊版 `src/browser/browser-diagnostics.ts` 的 URL 消毒部分：只保留
//! 正規化後的 hostname 與 pathname，username、password、port、query、fragment
//! 一律丟棄；解析失敗時回傳有界的 redacted 佔位值，絕不 panic。
//!
//! DEC-006：舊版 `channel-url.ts` 本身的 `CHANNEL_PATTERN`／`createChannelUrl`／
//! `isExpectedChannelUrl`／`stableJitter` 也在此移植（S3.4，ChannelSession 與
//! ChannelHealthChecker 使用端）。
#![allow(dead_code)]

use url::Url;

use super::cdp::context::DriverError;

/// Hostname 輸出上限，超過即裁切（對應舊版 `MAX_HOST_LENGTH`）。
pub(crate) const MAX_HOST_LENGTH: usize = 128;

/// Path 輸出上限，超過即裁切（對應舊版 `MAX_PATH_LENGTH`）。
pub(crate) const MAX_PATH_LENGTH: usize = 256;

/// 無法消毒時的佔位字串（對應舊版 `REDACTED_URL`）。
pub(crate) const REDACTED_URL: &str = "[REDACTED]";

/// 端點分類（對應舊版 `EndpointCategory`），分類規則刻意保守：
/// 只描述觀察到的端點，不得宣稱請求是否惡意、純追蹤或可封鎖。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum EndpointCategory {
    TwitchDocument,
    TwitchJavascript,
    TwitchGraphql,
    TwitchAntiAbuse,
    TwitchApi,
    TwitchMedia,
    TwitchOther,
    ThirdParty,
    Unknown,
}

impl EndpointCategory {
    /// 分類的 snake_case 名稱，與舊版字面值一致，供診斷日誌輸出。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::TwitchDocument => "twitch_document",
            Self::TwitchJavascript => "twitch_javascript",
            Self::TwitchGraphql => "twitch_graphql",
            Self::TwitchAntiAbuse => "twitch_anti_abuse",
            Self::TwitchApi => "twitch_api",
            Self::TwitchMedia => "twitch_media",
            Self::TwitchOther => "twitch_other",
            Self::ThirdParty => "third_party",
            Self::Unknown => "unknown",
        }
    }
}

/// 消毒後的端點（對應舊版 `SanitizedEndpoint`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SanitizedEndpoint {
    /// 正規化後的 hostname，超過 [`MAX_HOST_LENGTH`] 即裁切。
    pub(crate) host: String,
    /// 正規化後的 pathname，超過 [`MAX_PATH_LENGTH`] 即裁切；不含 query 與 fragment。
    pub(crate) path: String,
    /// 依 hostname 判定的端點分類。
    pub(crate) endpoint_category: EndpointCategory,
}

/// 解析並消毒任意的瀏覽器 URL（對應舊版 `sanitizeBrowserUrl`）。
///
/// 只保留正規化後的 hostname 與 pathname；username、password、port、query
/// 與 fragment 一律丟棄。`None`（對應舊版的 `null`／`undefined`）、空字串與
/// 解析失敗都回傳有界的 redacted 佔位值，絕不 panic。
pub(crate) fn sanitize_browser_url(raw_url: Option<&str>) -> SanitizedEndpoint {
    let Some(raw_url) = raw_url else {
        return redacted_endpoint();
    };
    if raw_url.is_empty() {
        return redacted_endpoint();
    }

    let parsed = match parse_url(raw_url) {
        Ok(parsed) => parsed,
        Err(_) => return redacted_endpoint(),
    };
    // WHATWG URL 規範保證 hostname 恆為小寫；空 host 時 `host_str()` 回傳 `None`。
    let hostname = parsed.host_str().unwrap_or_default();
    SanitizedEndpoint {
        host: truncate_text(hostname, MAX_HOST_LENGTH),
        path: truncate_text(parsed.path(), MAX_PATH_LENGTH),
        // 分類用未裁切的 hostname，比照舊版。
        endpoint_category: classify_hostname(hostname),
    }
}

/// 解析絕對 URL，或診斷事件發出的 host+path 片段（無 scheme 時補 `https://`）。
///
/// 對應舊版 `parseUrl`：先以 `/^[A-Za-z][A-Za-z0-9+.-]*:\/\//u` 判斷是否帶
/// scheme，帶 scheme 才直接解析，否則一律當成 https 的縮寫。
fn parse_url(raw_url: &str) -> Result<Url, url::ParseError> {
    if has_scheme_with_authority(raw_url) {
        Url::parse(raw_url)
    } else {
        Url::parse(&format!("https://{raw_url}"))
    }
}

/// 手寫對應舊版正規式 `/^[A-Za-z][A-Za-z0-9+.-]*:\/\//u` 的判斷。
fn has_scheme_with_authority(raw_url: &str) -> bool {
    let Some(colon) = raw_url.find(':') else {
        return false;
    };
    if !raw_url[colon + 1..].starts_with("//") {
        return false;
    }
    let mut chars = raw_url[..colon].chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '.' | '-'))
}

/// 依 hostname 分類端點（對應舊版 `classifyHostname`）。
///
/// 比對順序比照舊版：graphql → anti-abuse → api → document（僅完全相符）→
/// javascript → media → other → third_party。
pub(crate) fn classify_hostname(hostname: &str) -> EndpointCategory {
    let normalized = hostname.trim().to_lowercase();
    if normalized.is_empty() {
        return EndpointCategory::Unknown;
    }
    if normalized == "gql.twitch.tv" || is_subdomain_of(&normalized, "gql.twitch.tv") {
        return EndpointCategory::TwitchGraphql;
    }
    if normalized == "k.twitchcdn.net" || is_subdomain_of(&normalized, "k.twitchcdn.net") {
        return EndpointCategory::TwitchAntiAbuse;
    }
    if normalized == "api.twitch.tv" || is_subdomain_of(&normalized, "api.twitch.tv") {
        return EndpointCategory::TwitchApi;
    }
    // document 僅認這三個 host 本身，不含子網域。
    if matches!(&*normalized, "twitch.tv" | "www.twitch.tv" | "m.twitch.tv") {
        return EndpointCategory::TwitchDocument;
    }
    if normalized == "assets.twitch.tv"
        || normalized == "static.twitchcdn.net"
        || is_subdomain_of(&normalized, "static.twitchcdn.net")
        || is_subdomain_of(&normalized, "jtvnw.net")
    {
        return EndpointCategory::TwitchJavascript;
    }
    if is_subdomain_of(&normalized, "ttvnw.net") {
        return EndpointCategory::TwitchMedia;
    }
    if is_subdomain_of(&normalized, "twitch.tv") {
        return EndpointCategory::TwitchOther;
    }
    EndpointCategory::ThirdParty
}

/// 是否為 `suffix` 的子網域：必須比後綴長，且以「.後綴」結尾（完全是後綴本身不算）。
fn is_subdomain_of(hostname: &str, suffix: &str) -> bool {
    hostname.len() > suffix.len()
        && hostname.ends_with(suffix)
        && hostname.as_bytes()[hostname.len() - suffix.len() - 1] == b'.'
}

/// 將自由文字裁切至最多 `max` 個字元（對應舊版 `truncateText`）。
///
/// 舊版以 UTF-16 code unit 計數，實際輸入（URL parser 輸出的 host 與
/// percent-encode 後的 path）恆為 ASCII，故此處直接以位元組上限裁切，
/// 並退回字元邊界以避免切斷多位元組字元。
fn truncate_text(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut boundary = max;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_string()
}

/// 將消毒後的 host 與 path 接回單一不含 query 的 URL（對應舊版 `joinSanitizedUrl`）。
///
/// path 為空時（例如 redacted 佔位值）只回傳 host。
pub(crate) fn join_sanitized_url(endpoint: &SanitizedEndpoint) -> String {
    if endpoint.path.is_empty() {
        return endpoint.host.clone();
    }
    format!("{}{}", endpoint.host, endpoint.path)
}

/// redacted 佔位結果（對應舊版 `{ host: REDACTED_URL, path: '', endpointCategory: 'unknown' }`）。
fn redacted_endpoint() -> SanitizedEndpoint {
    SanitizedEndpoint {
        host: REDACTED_URL.to_string(),
        path: String::new(),
        endpoint_category: EndpointCategory::Unknown,
    }
}

// ---------------------------------------------------------------------------
// 舊版 `src/browser/channel-url.ts` 的其餘函式（ChannelSession／ChannelHealthChecker 使用）
// ---------------------------------------------------------------------------

/// 舊版 `CHANNEL_PATTERN` (`/^[A-Za-z0-9_]{1,25}$/u`)。
pub(crate) fn channel_pattern(channel: &str) -> bool {
    let bytes = channel.as_bytes();
    (1..=25).contains(&bytes.len())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

/// 舊版 `createChannelUrl(channel)`：pattern 檢查失敗拋錯，其餘回 Twitch 頻道頁 URL。
///
/// pattern 已把 channel 限制在 `[A-Za-z0-9_]`，此字元集就是
/// `encodeURIComponent` 的 unreserved 集合，因此 encoding 恆為原樣（保留等價）。
pub(crate) fn create_channel_url(channel: &str) -> Result<String, DriverError> {
    if !channel_pattern(channel) {
        return Err(DriverError::new(
            "Error",
            "Channel 必須是 1 到 25 字元的英數字或底線",
        ));
    }
    Ok(format!(
        "https://www.twitch.tv/{}",
        js_encode_uri_component(channel)
    ))
}

/// 舊版 `isExpectedChannelUrl(currentUrl, targetUrl)`：origin（en-US 小寫）與
/// normalize 後的 pathname 相同才算預期內。
pub(crate) fn is_expected_channel_url(current_url: &str, target_url: &str) -> bool {
    let Ok(current) = Url::parse(current_url) else {
        return false;
    };
    let Ok(target) = Url::parse(target_url) else {
        return false;
    };
    current.origin().ascii_serialization().to_lowercase()
        == target.origin().ascii_serialization().to_lowercase()
        && normalize_path(current.path()) == normalize_path(target.path())
}

/// 舊版 `stableJitter(value, maximumMs)`：FNV-1a 雜湊的穩定抖動毫秒數。
///
/// `Math.imul` 為 32 位元整數乘法，`>>> 0` 對應 u32；`character.codePointAt(0)`
/// 對應 Rust 的 code point。`maximumMs` 在 Rust 型別下必為正整數，`<= 0` 的
/// 分支等價於 0（輸入保證 u64）。
pub(crate) fn stable_jitter(value: &str, maximum_ms: u64) -> u64 {
    if maximum_ms == 0 {
        return 0;
    }
    let mut hash: u32 = 2_166_136_261;
    for character in value.chars() {
        hash ^= u32::from(character);
        hash = hash.wrapping_mul(16_777_619);
    }
    u64::from(hash) % maximum_ms
}

/// 舊版 `normalizePath(pathname)`：移除結尾的 `/+` 後 en-US 小寫，空值回 `/`。
fn normalize_path(pathname: &str) -> String {
    let trimmed = pathname.trim_end_matches('/');
    let normalized = if trimmed.is_empty() { "/" } else { trimmed };
    normalized.to_lowercase()
}

/// JS `encodeURIComponent(channel)`：pattern 保證了 channel 只含
/// `[A-Za-z0-9_]`（`encodeURIComponent` 的 unreserved 字元集），恆為原樣。
/// 此函式顯式表達同規則，供 pattern 更動時跟著處理其餘字元。
fn js_encode_uri_component(channel: &str) -> String {
    channel.replace(|character: char| {
        !matches!(character, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.' | '~')
    }, "")
}

#[cfg(test)]
mod tests;
