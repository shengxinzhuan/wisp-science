// Heuristic ChatGPT composer / reply detection. Avoid brittle hashed class names.

function chatgptDomFns() {
  var composer = function () {
    return document.querySelector('#prompt-textarea') ||
      document.querySelector('[data-testid="prompt-textarea"]') ||
      document.querySelector('div[contenteditable="true"]#prompt-textarea') ||
      document.querySelector('form textarea') ||
      document.querySelector('[contenteditable="true"][role="textbox"]') ||
      document.querySelector('div[contenteditable="true"]');
  };
  var sendButton = function () {
    return document.querySelector('[data-testid="send-button"]') ||
      document.querySelector('button[aria-label*="Send" i]') ||
      document.querySelector('button[data-testid="composer-send-button"]');
  };
  var stopButton = function () {
    return document.querySelector('button[aria-label*="Stop" i]') ||
      document.querySelector('[data-testid="stop-button"]');
  };
  var assistantTurns = function () {
    var nodes = document.querySelectorAll('[data-message-author-role="assistant"]');
    return Array.prototype.slice.call(nodes);
  };
  var lastAssistant = function () {
    var turns = assistantTurns();
    var last = turns[turns.length - 1];
    if (!last) return { text: "", citations: [] };
    var links = Array.prototype.slice.call(last.querySelectorAll("a[href]")).map(function (a) {
      return { text: (a.innerText || "").trim().slice(0, 200), href: a.href };
    }).filter(function (x) { return x.href; });
    return { text: (last.innerText || "").trim(), citations: links.slice(0, 30) };
  };
  var blocked = function () {
    var text = ((document.body && document.body.innerText) || "").toLowerCase();
    if (text.indexOf("are you a robot") >= 0 && (text.indexOf("confirm you are a human") >= 0 || text.indexOf("captcha") >= 0)) {
      return "captcha";
    }
    if (text.indexOf("log in") >= 0 && !composer()) return "login";
    return null;
  };
  return {
    ready: function () {
      return {
        url: location.href,
        has_composer: !!composer(),
        sending: !!stopButton(),
        blocked: blocked(),
        last: lastAssistant()
      };
    },
    fill: function (prompt) {
      var el = composer();
      if (!el) throw new Error("ChatGPT composer not found");
      el.focus();
      if ("value" in el) {
        var proto = el.tagName === "TEXTAREA" ? window.HTMLTextAreaElement.prototype : window.HTMLInputElement.prototype;
        var setter = Object.getOwnPropertyDescriptor(proto, "value");
        if (setter && setter.set) setter.set.call(el, prompt);
        else el.value = prompt;
        el.dispatchEvent(new Event("input", { bubbles: true }));
        el.dispatchEvent(new Event("change", { bubbles: true }));
      } else {
        document.execCommand("selectAll", false, null);
        document.execCommand("insertText", false, prompt);
      }
      return { ok: true, chars: String(prompt).length };
    },
    send: function () {
      var btn = sendButton();
      if (btn && !btn.disabled) {
        btn.click();
        return { ok: true, method: "button" };
      }
      var el = composer();
      if (!el) throw new Error("ChatGPT send control not found");
      el.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", code: "Enter", bubbles: true }));
      return { ok: true, method: "enter" };
    },
    read: function () {
      var last = lastAssistant();
      return {
        url: location.href,
        title: document.title,
        blocked: blocked(),
        sending: !!stopButton(),
        answer_text: last.text,
        citations: last.citations
      };
    }
  };
}

if (typeof self !== "undefined") {
  self.chatgptDomFns = chatgptDomFns;
}