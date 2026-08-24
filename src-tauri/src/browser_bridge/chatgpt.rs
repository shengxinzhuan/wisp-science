use serde_json::{json, Value};

pub fn send_script(prompt: &str) -> String {
    json!({
        "cmd": "chatgpt",
        "method": "fill",
        "prompt": prompt
    })
    .to_string()
}

pub fn click_send_script() -> String {
    json!({ "cmd": "chatgpt", "method": "send" }).to_string()
}

pub fn read_script() -> String {
    json!({ "cmd": "chatgpt", "method": "read" }).to_string()
}

pub fn ready_script() -> String {
    json!({ "cmd": "chatgpt", "method": "ready" }).to_string()
}

pub fn wait_spec() -> Value {
    json!({
        "until": "stable",
        "selector": "[data-message-author-role=\"assistant\"]",
        "text_not_includes": "Working",
        "settle_ms": 1200
    })
}

pub fn parse_read(value: &Value) -> Value {
    json!({
        "answer_text": value.get("answer_text").cloned().unwrap_or(json!("")),
        "citations": value.get("citations").cloned().unwrap_or(json!([])),
        "status": if value.get("sending").and_then(Value::as_bool).unwrap_or(false) { "streaming" } else { "complete" },
        "blocked": value.get("blocked").cloned().unwrap_or(Value::Null),
        "url": value.get("url").cloned().unwrap_or(json!(""))
    })
}
