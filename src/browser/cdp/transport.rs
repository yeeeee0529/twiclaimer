//! CDP transport：與 headless shell 之間以 `--remote-debugging-pipe` 的兩條 pipe 通訊，
//! 每則訊息是一個 JSON 物件加 `\0`（§7.3.1）。
//!
//! 本檔只處理訊息框架（framing）；連線與 session 路由在 `connection.rs`，啟動在 `launcher.rs`。

/// 解碼器：把讀到的位元組流切成 NUL 分隔的 JSON 訊息。
///
/// CDP 的訊息可能被作業系統切成多段讀取，也可能一次讀到多則訊息；
/// 解碼器維護內部緩衝，呼叫端只需持續餵入讀到的位元組。
#[derive(Default)]
pub(crate) struct MessageDecoder {
    buf: Vec<u8>,
}

/// 單則訊息的位元組數上限（含 JSON 內容）。
///
/// 截圖等回應可能達數 MiB；上限取 64 MiB，避免瀏覽器行為異常時緩衝無限成長。
pub(crate) const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// 解碼錯誤。
///
/// 欄位只在測試中讀取（正式碼只把錯誤視為協定中斷）。
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum DecodeError {
    /// 累積的訊息超過 [`MAX_MESSAGE_BYTES`] 仍沒有出現 `\0`。
    MessageTooLarge {
        /// 收到本批位元組前的累積大小。
        buffered: usize,
        /// 本批位元組數。
        incoming: usize,
    },
}

impl MessageDecoder {
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// 餵入新讀到的位元組，回傳這批位元組內完整結尾的所有訊息（UTF-8 JSON 文字）。
    ///
    /// 沒有以 `\0` 結尾的殘餘位元組留在內部緩衝，等下次餵入。
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, DecodeError> {
        if self.buf.len() + bytes.len() > MAX_MESSAGE_BYTES
            && !bytes.contains(&b'\0')
            && !self.buf.contains(&b'\0')
        {
            return Err(DecodeError::MessageTooLarge {
                buffered: self.buf.len(),
                incoming: bytes.len(),
            });
        }
        self.buf.extend_from_slice(bytes);
        let mut messages = Vec::new();
        // 依 `\0` 切段；最後一段若沒有結尾則留在緩衝。
        let mut start = 0;
        for (idx, byte) in self.buf.iter().enumerate() {
            if *byte == b'\0' {
                // CDP 訊息必須是 UTF-8 JSON；無效位元組視為協定錯誤，以 lossy 解碼保留除錯資訊。
                let text = String::from_utf8_lossy(&self.buf[start..idx]).into_owned();
                messages.push(text);
                start = idx + 1;
            }
        }
        self.buf.drain(..start);
        Ok(messages)
    }

    /// 目前殘餘（尚未以 `\0` 結尾）的位元組數（測試用）。
    #[allow(dead_code)]
    pub(crate) fn buffered_len(&self) -> usize {
        self.buf.len()
    }
}

/// 編碼訊息為要寫入 pipe 的位元組（JSON 文字加 `\0`）。
pub(crate) fn encode_message(message: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 1);
    out.extend_from_slice(message.as_bytes());
    out.push(b'\0');
    out
}

#[cfg(test)]
mod tests;
