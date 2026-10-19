// 來源：$REF/dist/browser/ChatDisabler.js（CHAT_WEBSOCKET_BLOCK_SCRIPT（buildChatBlockInitScript 的輸出））
// 由 S1.2 逐字複製；修改前先寫 doc/decisions.md。
(() => {
  const BLOCKED_HOSTNAME = 'irc-ws.chat.twitch.tv';
  const RealWebSocket = window.WebSocket;
  if (!RealWebSocket) {
    return;
  }

  class FakeChatSocket {
    constructor(url, protocols) {
      this.url = url;
      this.protocol = Array.isArray(protocols)
        ? (protocols[0] ?? '')
        : (protocols ?? '');
      this.readyState = 0;
      this.bufferedAmount = 0;
      this.extensions = '';
      this.binaryType = 'blob';
      this.onopen = null;
      this.onmessage = null;
      this.onclose = null;
      this.onerror = null;
      this._listeners = new Map();
      queueMicrotask(() => {
        if (this.readyState !== 0) {
          return;
        }
        this.readyState = 1;
        const event = { type: 'open', target: this };
        if (typeof this.onopen === 'function') {
          this.onopen(event);
        }
        this._dispatch('open', event);
      });
    }

    send() {}

    close() {
      if (this.readyState === 2 || this.readyState === 3) {
        return;
      }
      this.readyState = 2;
      queueMicrotask(() => {
        this.readyState = 3;
        const event = {
          type: 'close',
          code: 1000,
          reason: '',
          wasClean: true,
          target: this,
        };
        if (typeof this.onclose === 'function') {
          this.onclose(event);
        }
        this._dispatch('close', event);
      });
    }

    addEventListener(type, listener) {
      if (typeof listener !== 'function') {
        return;
      }
      const list = this._listeners.get(type) ?? new Set();
      list.add(listener);
      this._listeners.set(type, list);
    }

    removeEventListener(type, listener) {
      this._listeners.get(type)?.delete(listener);
    }

    dispatchEvent(event) {
      this._dispatch(event.type, event);
      return true;
    }

    _dispatch(type, event) {
      for (const listener of this._listeners.get(type) ?? []) {
        try {
          listener(event);
        } catch {
          // Page listener errors must not break the shim.
        }
      }
    }
  }

  function PatchedWebSocket(url, protocols) {
    let hostname = '';
    try {
      hostname = new URL(String(url)).hostname;
    } catch {
      hostname = '';
    }
    if (hostname === BLOCKED_HOSTNAME) {
      return new FakeChatSocket(url, protocols);
    }
    return new RealWebSocket(url, protocols);
  }

  PatchedWebSocket.prototype = RealWebSocket.prototype;
  PatchedWebSocket.CONNECTING = 0;
  PatchedWebSocket.OPEN = 1;
  PatchedWebSocket.CLOSING = 2;
  PatchedWebSocket.CLOSED = 3;
  window.WebSocket = PatchedWebSocket;
})();
