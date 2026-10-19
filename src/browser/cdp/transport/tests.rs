//! transport 的單元測試：訊息切分、一次多則、超大訊息（S1.3）。
use super::{DecodeError, MAX_MESSAGE_BYTES, MessageDecoder, encode_message};

#[test]
fn decode_complete_messages_in_single_read() {
    let mut decoder = MessageDecoder::new();
    let bytes = encode_message("{\"id\":1}")
        .iter()
        .chain(encode_message("{\"id\":2}").iter())
        .copied()
        .collect::<Vec<u8>>();
    let messages = decoder.push(&bytes).unwrap();
    assert_eq!(
        messages,
        vec!["{\"id\":1}".to_string(), "{\"id\":2}".to_string()]
    );
    assert_eq!(decoder.buffered_len(), 0);
}

#[test]
fn decode_message_split_across_reads() {
    let mut decoder = MessageDecoder::new();
    // JSON 內的 NUL 會以 \u0000 跳脫；位元組流中唯一的 \0 是訊息結尾。
    let bytes = encode_message("{\"method\":\"Page.enable\",\"params\":{\"x\":\"a\\u0000b\"}}");
    let total = bytes.chunks(3).count();
    for (idx, chunk) in bytes.chunks(3).enumerate() {
        let messages = decoder.push(chunk).unwrap();
        if idx + 1 == total {
            assert_eq!(
                messages,
                vec!["{\"method\":\"Page.enable\",\"params\":{\"x\":\"a\\u0000b\"}}".to_string()]
            );
        } else {
            assert!(messages.is_empty(), "第 {idx} 批不應產生訊息");
        }
    }
    assert_eq!(decoder.buffered_len(), 0);
}

#[test]
fn decode_partial_message_stays_buffered() {
    let mut decoder = MessageDecoder::new();
    let first = encode_message("{\"id\":1}");
    let second = encode_message("{\"id\":2}");
    let mut bytes = first;
    let split_at = second.len() - 1;
    bytes.extend_from_slice(&second[..split_at]); // 第二則只有前半（缺結尾 \0）

    let messages = decoder.push(&bytes).unwrap();
    assert_eq!(messages, vec!["{\"id\":1}".to_string()]);
    assert_eq!(decoder.buffered_len(), split_at);

    // 補上剩下的位元組（含結尾 \0）才完成第二則。
    let messages = decoder.push(&second[split_at..]).unwrap();
    assert_eq!(messages, vec!["{\"id\":2}".to_string()]);
    assert_eq!(decoder.buffered_len(), 0);
}

#[test]
fn decode_rejects_message_over_size_limit() {
    let mut decoder = MessageDecoder::new();
    let chunk = vec![b'a'; 1024 * 1024];
    let mut pushed = 0usize;
    let result = loop {
        match decoder.push(&chunk) {
            Ok(_) => pushed += chunk.len(),
            Err(err) => break err,
        }
        assert!(
            pushed <= MAX_MESSAGE_BYTES + 2 * chunk.len(),
            "應在到達上限時回報錯誤"
        );
    };
    match result {
        DecodeError::MessageTooLarge { buffered, incoming } => {
            assert!(buffered + incoming > MAX_MESSAGE_BYTES - chunk.len());
        }
    }
}

#[test]
fn encode_appends_nul_terminator() {
    let bytes = encode_message("{\"id\":1}");
    assert_eq!(bytes, b"{\"id\":1}\0");
}

#[test]
fn decode_lossy_utf8_does_not_break_stream() {
    let mut decoder = MessageDecoder::new();
    let mut bytes = b"{\"x\":\"".to_vec();
    bytes.extend_from_slice(&[0xff, 0xfe]); // 無效 UTF-8
    bytes.push(b'"');
    bytes.push(0);
    let messages = decoder.push(&bytes).unwrap();
    assert_eq!(messages.len(), 1);
    assert!(
        messages[0].contains('\u{fffd}'),
        "無效位元組應以替換字元呈現"
    );
}
