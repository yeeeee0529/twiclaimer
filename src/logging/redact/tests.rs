//! 消毒層的單元測試：移植舊版 `test/unit/logging.test.ts` 與消毒直接相關的案例（§7.7）。
//! Logger 本體（層級過濾、輸出格式、flush）的測試屬 S2.1 下半，另行移植。
use super::{
    LogError, REDACTED_VALUE, SanitizedValue, js_number_to_string, redact_sensitive_data,
    redact_sensitive_string,
};

/// 測試輔助：字串值。
fn s(value: &str) -> SanitizedValue {
    SanitizedValue::String(value.to_string())
}

/// 測試輔助：物件（插入順序保留）。
fn obj(entries: Vec<(&str, SanitizedValue)>) -> SanitizedValue {
    SanitizedValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

/// 測試輔助：陣列。
fn arr(items: Vec<SanitizedValue>) -> SanitizedValue {
    SanitizedValue::Array(items)
}

// 原測試：遞迴遮罩大小寫不同的巢狀 object 與 array 敏感欄位
#[test]
fn redacts_case_insensitive_nested_object_and_array_sensitive_fields() {
    let secrets = [
        "cookie-secret",
        "cookies-secret",
        "token-secret",
        "access-snake-secret",
        "access-camel-secret",
        "oauth-secret",
        "authorization-secret",
        "storage-state-secret",
    ];
    let input = obj(vec![
        (
            "profile",
            obj(vec![
                ("Cookie", s(secrets[0])),
                (
                    "nested",
                    arr(vec![
                        obj(vec![("COOKIES", s(secrets[1]))]),
                        obj(vec![
                            ("token", s(secrets[2])),
                            ("access_token", s(secrets[3])),
                            ("accessToken", s(secrets[4])),
                        ]),
                        obj(vec![
                            ("OAuth", s(secrets[5])),
                            ("AUTHORIZATION", s(secrets[6])),
                            ("storageState", s(secrets[7])),
                        ]),
                    ]),
                ),
            ]),
        ),
        ("channel", s("streamer_one")),
    ]);

    let sanitized = redact_sensitive_data(&input);
    let serialized = sanitized.to_json_string();

    for secret in secrets {
        assert!(!serialized.contains(secret), "不該出現 {secret}");
    }
    assert_eq!(
        serialized,
        r#"{"profile":{"Cookie":"[REDACTED]","nested":[{"COOKIES":"[REDACTED]"},{"token":"[REDACTED]","access_token":"[REDACTED]","accessToken":"[REDACTED]"},{"OAuth":"[REDACTED]","AUTHORIZATION":"[REDACTED]","storageState":"[REDACTED]"}]},"channel":"streamer_one"}"#
    );
}

// 原測試：遞迴遮罩 Telegram 與 Discord 私人 identifier 欄位
#[test]
fn redacts_telegram_and_discord_private_identifier_fields() {
    let input = obj(vec![
        ("chatId", s("telegram-chat-secret")),
        (
            "discord",
            obj(vec![
                ("channel_id", s("discord-channel-secret")),
                ("userId", s("discord-user-secret")),
                ("guildID", s("discord-guild-secret")),
            ]),
        ),
        (
            "nested",
            arr(vec![obj(vec![(
                "allowedChatIds",
                arr(vec![s("nested-chat-secret")]),
            )])]),
        ),
        ("channel", s("public-channel-name")),
        ("updateId", SanitizedValue::Number(42.0)),
    ]);

    let sanitized = redact_sensitive_data(&input);
    let serialized = sanitized.to_json_string();

    for identifier in [
        "telegram-chat-secret",
        "discord-channel-secret",
        "discord-user-secret",
        "discord-guild-secret",
        "nested-chat-secret",
    ] {
        assert!(!serialized.contains(identifier), "不該出現 {identifier}");
    }
    assert_eq!(
        serialized,
        r#"{"chatId":"[REDACTED]","discord":{"channel_id":"[REDACTED]","userId":"[REDACTED]","guildID":"[REDACTED]"},"nested":[{"allowedChatIds":"[REDACTED]"}],"channel":"public-channel-name","updateId":42}"#
    );
}

// 原測試：遮罩錯誤字串中的 Bearer token 與常見敏感指派
// 舊版經 logger.error 記錄；logger 本體屬 S2.1 下半，此處直接對欄位字串套
// redactSensitiveString——logger 的消毒對字串欄位效果相同。
#[test]
fn redacts_bearer_token_and_common_sensitive_assignments_in_strings() {
    let error_field = "Bearer bearer-secret token=token-secret accessToken: access-secret \
        oauth_token=oauth-secret Cookie: session=cookie-one; other=cookie-two";
    let details_field = "authorization=authorization-secret, storage_state: storage-secret";
    let secrets = [
        "bearer-secret",
        "token-secret",
        "access-secret",
        "oauth-secret",
        "cookie-one",
        "cookie-two",
        "authorization-secret",
        "storage-secret",
    ];

    for field in [error_field, details_field] {
        let sanitized = redact_sensitive_string(field);
        for secret in secrets {
            assert!(!sanitized.contains(secret), "不該出現 {secret}");
        }
        assert!(sanitized.contains(REDACTED_VALUE));
    }
}

// 原測試：遮罩錯誤字串中直接內嵌的 storageState 與 cookie 結構
// 舊版經 logger.error 記錄，此處直接對欄位字串套 redactSensitiveString（說明同上一測試）。
#[test]
fn redacts_inline_storage_state_and_cookie_structures_in_strings() {
    let storage_error =
        r#"storageState={"cookies":[{"name":"auth-token","value":"storage-secret"}],"origins":[]}"#;
    let cookie_error = "Cookies: session=cookie-secret; persistent=second-cookie-secret";

    assert_eq!(
        redact_sensitive_string(storage_error),
        "storageState=[REDACTED]"
    );
    assert_eq!(redact_sensitive_string(cookie_error), "Cookies: [REDACTED]");
}

// 原測試：安全序列化 Error、Error cause 與循環資料
// Error 輸出 {name, message, cause?, …其他欄位}；舊版另有 stack 欄位，Rust 沒有堆疊字串，
// 依允許差異 X5 省略。循環資料以 Shared（Rc）重現舊版的 cyclic.self = cyclic。
#[test]
fn sanitizes_error_cause_and_cyclic_data() {
    let cause = LogError {
        name: "Error".to_string(),
        message: "token=cause-secret".to_string(),
        cause: None,
        extra: vec![],
    };
    let error = LogError {
        name: "Error".to_string(),
        message: "request failed: Bearer error-secret".to_string(),
        cause: Some(Box::new(SanitizedValue::Error(cause))),
        extra: vec![
            ("accessToken".to_string(), s("property-secret")),
            (
                "context".to_string(),
                obj(vec![("authorization", s("nested-secret"))]),
            ),
        ],
    };
    // 舊版：const cyclic = { error }; cyclic.self = cyclic;
    // Rust 先建立空物件，再填入 error 與指向自己的 Shared 參照。
    let cyclic = std::rc::Rc::new(std::cell::RefCell::new(SanitizedValue::Object(vec![])));
    {
        let mut borrowed = cyclic.borrow_mut();
        let SanitizedValue::Object(entries) = &mut *borrowed else {
            unreachable!("測試節點固定是 Object");
        };
        entries.push(("error".to_string(), SanitizedValue::Error(error)));
        entries.push((
            "self".to_string(),
            SanitizedValue::Shared(std::rc::Rc::clone(&cyclic)),
        ));
    }

    let sanitized = redact_sensitive_data(&SanitizedValue::Shared(std::rc::Rc::clone(&cyclic)));
    let serialized = sanitized.to_json_string();

    assert_eq!(
        serialized,
        r#"{"error":{"name":"Error","message":"request failed: Bearer [REDACTED]","cause":{"name":"Error","message":"token=[REDACTED]"},"accessToken":"[REDACTED]","context":{"authorization":"[REDACTED]"}},"self":"[Circular]"}"#
    );
    for secret in [
        "cause-secret",
        "error-secret",
        "property-secret",
        "nested-secret",
    ] {
        assert!(!serialized.contains(secret), "不該出現 {secret}");
    }
}

// 原測試：遮罩巢狀 Error 的私人 identifier 屬性與訊息指派
#[test]
fn redacts_private_identifier_properties_and_message_assignments_in_nested_errors() {
    let cause = LogError {
        name: "Error".to_string(),
        message: "chatId=nested-chat-secret".to_string(),
        cause: None,
        extra: vec![("userId".to_string(), s("nested-user-secret"))],
    };
    let error = LogError {
        name: "Error".to_string(),
        message: "channel_id=channel-secret".to_string(),
        cause: Some(Box::new(SanitizedValue::Error(cause))),
        extra: vec![
            ("guildId".to_string(), s("guild-secret")),
            (
                "context".to_string(),
                obj(vec![("chat_id", s("context-chat-secret"))]),
            ),
        ],
    };

    let sanitized = redact_sensitive_data(&obj(vec![("error", SanitizedValue::Error(error))]));
    let serialized = sanitized.to_json_string();

    for identifier in [
        "nested-chat-secret",
        "nested-user-secret",
        "channel-secret",
        "guild-secret",
        "context-chat-secret",
    ] {
        assert!(!serialized.contains(identifier), "不該出現 {identifier}");
    }
    assert_eq!(
        serialized,
        r#"{"error":{"name":"Error","message":"channel_id=[REDACTED]","cause":{"name":"Error","message":"chatId=[REDACTED]","userId":"[REDACTED]"},"guildId":"[REDACTED]","context":{"chat_id":"[REDACTED]"}}}"#
    );
}

// BEARER_TOKEN_PATTERN 的樣式轉換補充測試（\b → (?-u:\b)、\s → 展開字元類別、i → (?i)），
// 輸入取自同一舊測試。
// 原測試：遮罩錯誤字串中的 Bearer token 與常見敏感指派
#[test]
fn bearer_token_pattern_matches_js_semantics() {
    assert_eq!(
        redact_sensitive_string("Bearer bearer-secret"),
        "Bearer [REDACTED]"
    );
    // 旗標 i：大小寫都要遮，且替換字串照舊版是字面 "Bearer [REDACTED]"。
    assert_eq!(
        redact_sensitive_string("BEARER bearer-secret"),
        "Bearer [REDACTED]"
    );
    // \b 在 Bearer 前需要邊界：前面是連字號可以，前面是字母不行。
    assert_eq!(
        redact_sensitive_string("x-Bearer bearer-secret"),
        "x-Bearer [REDACTED]"
    );
    assert_eq!(
        redact_sensitive_string("xxBearer secret"),
        "xxBearer secret"
    );
    // 沒有空白接續的 bearer 不是 token 開頭，照舊版不遮。
    assert_eq!(
        redact_sensitive_string("bearer=bearer-secret"),
        "bearer=bearer-secret"
    );
    // JS \s 含 \u{feff}，Rust 內建 \s 不含：靠展開字元類別重現。
    assert_eq!(
        redact_sensitive_string("Bearer\u{feff}bearer-secret"),
        "Bearer [REDACTED]"
    );
}

// TOKEN_ASSIGNMENT_PATTERN 的樣式轉換補充測試，輸入取自同一舊測試。
// 原測試：遮罩錯誤字串中的 Bearer token 與常見敏感指派
#[test]
fn token_assignment_pattern_matches_js_semantics() {
    assert_eq!(
        redact_sensitive_string("token=token-secret"),
        "token=[REDACTED]"
    );
    assert_eq!(
        redact_sensitive_string("accessToken: access-secret"),
        "accessToken: [REDACTED]"
    );
    assert_eq!(
        redact_sensitive_string("oauth_token=oauth-secret"),
        "oauth_token=[REDACTED]"
    );
    // 引號包裹的值整段遮罩（含 \\" 的跳脫分支）。
    assert_eq!(
        redact_sensitive_string(r#"access_token="quoted secret""#),
        "access_token=[REDACTED]"
    );
    assert_eq!(
        redact_sensitive_string(r#"access_token="a\"b" tail"#),
        r#"access_token=[REDACTED] tail"#
    );
    // JS \b 只認 ASCII 單字字元：前後接單字字元（x、數字）時不構成邊界，照舊版不遮。
    assert_eq!(
        redact_sensitive_string("xtoken=secret token=secret"),
        "xtoken=secret token=[REDACTED]"
    );
    assert_eq!(redact_sensitive_string("token1=secret"), "token1=secret");
    // é 非 ASCII 單字字元，cafétoken 的 token 前有邊界（Rust 需 (?-u:\b) 才會成立）。
    assert_eq!(
        redact_sensitive_string("cafétoken=secret"),
        "cafétoken=[REDACTED]"
    );
    // JS \s 含 \u{feff}：照舊版要能遮罩（\ufeff 屬於 group 1 的 \s*，原樣保留）；
    // Rust 內建 \s 不含，需靠展開字元類別。
    assert_eq!(
        redact_sensitive_string("token\u{feff}=\u{feff}secret"),
        "token\u{feff}=\u{feff}[REDACTED]"
    );
    // JS \s 不含 \u{85}（NEL）：照舊版不遮；Rust 內建 \s 多含 \u{85}，需靠展開字元類別排除。
    assert_eq!(
        redact_sensitive_string("token\u{85}=secret"),
        "token\u{85}=secret"
    );
}

// BROAD_ASSIGNMENT_PATTERN 的樣式轉換補充測試，輸入取自同一舊測試。
// 原測試：遮罩錯誤字串中的 Bearer token 與常見敏感指派
#[test]
fn broad_assignment_pattern_matches_js_semantics() {
    // 未加引號的值整段吃到行尾。
    assert_eq!(
        redact_sensitive_string(
            "authorization=authorization-secret, storage_state: storage-secret"
        ),
        "authorization=[REDACTED]"
    );
    // storage_state 的底線屬於 [\s_-]；group 1 含分隔符「: 」，原樣保留。
    assert_eq!(
        redact_sensitive_string("storage_state: storage-secret"),
        "storage_state: [REDACTED]"
    );
    // 引號包裹的鍵與值。
    assert_eq!(
        redact_sensitive_string(r#""cookies": "a, b""#),
        r#""cookies": [REDACTED]"#
    );
    // 引號鍵後的值含分號時，未引號分支只吃到行尾，分號後的內容保留。
    assert_eq!(
        redact_sensitive_string("Cookie: session=cookie-one; other=cookie-two"),
        "Cookie: [REDACTED]"
    );
    // 一般鍵值不遮。
    assert_eq!(
        redact_sensitive_string("other=cookie-two"),
        "other=cookie-two"
    );
}

// PRIVATE_IDENTIFIER_ASSIGNMENT_PATTERN 的樣式轉換補充測試，輸入取自同一舊測試。
// 原測試：遮罩巢狀 Error 的私人 identifier 屬性與訊息指派
#[test]
fn private_identifier_assignment_pattern_matches_js_semantics() {
    assert_eq!(
        redact_sensitive_string("chatId=nested-chat-secret"),
        "chatId=[REDACTED]"
    );
    assert_eq!(
        redact_sensitive_string("channel_id=channel-secret"),
        "channel_id=[REDACTED]"
    );
    assert_eq!(
        redact_sensitive_string("userId=nested-user-secret"),
        "userId=[REDACTED]"
    );
    assert_eq!(
        redact_sensitive_string("guildID=guild-secret"),
        "guildID=[REDACTED]"
    );
    // 非 chat/channel/user/guild 前綴的 id 照舊版不遮。
    assert_eq!(redact_sensitive_string("updateId=42"), "updateId=42");
}

// 補充測試：§7.7 規定數字輸出用 ryu-js，與 JS `String(number)`／`JSON.stringify` 相同。
#[test]
fn js_number_to_string_matches_json_stringify_semantics() {
    assert_eq!(js_number_to_string(0.1), "0.1");
    assert_eq!(js_number_to_string(1e21), "1e+21");
    assert_eq!(js_number_to_string(-0.0), "0");
    assert_eq!(js_number_to_string(42.0), "42");
    assert_eq!(js_number_to_string(f64::NAN), "NaN");
    assert_eq!(js_number_to_string(f64::INFINITY), "Infinity");
    assert_eq!(js_number_to_string(f64::NEG_INFINITY), "-Infinity");
}

// 補充測試：非有限數字在消毒時轉成字串（照舊版 String(value)），有限數字維持數字；
// Undefined 照舊版輸出 [Undefined]。
#[test]
fn non_finite_numbers_and_undefined_become_marker_strings() {
    assert_eq!(
        redact_sensitive_data(&SanitizedValue::Number(f64::NAN)),
        s("NaN")
    );
    assert_eq!(
        redact_sensitive_data(&SanitizedValue::Number(f64::INFINITY)),
        s("Infinity")
    );
    assert_eq!(
        redact_sensitive_data(&SanitizedValue::Number(f64::NEG_INFINITY)),
        s("-Infinity")
    );
    // -0 是有限數字，維持數字；序列化時照 JSON.stringify 輸出 "0"。
    assert_eq!(
        redact_sensitive_data(&SanitizedValue::Number(-0.0)),
        SanitizedValue::Number(-0.0)
    );
    assert_eq!(
        redact_sensitive_data(&SanitizedValue::Number(-0.0)).to_json_string(),
        "0"
    );
    assert_eq!(
        redact_sensitive_data(&SanitizedValue::Undefined),
        s("[Undefined]")
    );
}

// 補充測試：序列化的 undefined 語意與 JSON.stringify 相同（物件欄位省略、陣列輸出 null）。
#[test]
fn json_stringification_omits_undefined_fields_like_json_stringify() {
    let value = obj(vec![
        ("a", SanitizedValue::Undefined),
        ("b", arr(vec![SanitizedValue::Undefined])),
        ("c", s("x")),
    ]);
    assert_eq!(value.to_json_string(), r#"{"b":[null],"c":"x"}"#);
}
