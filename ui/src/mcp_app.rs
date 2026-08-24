use crate::bindings::{mount_mcp_app, park_mcp_app};
use crate::text::unique_dom_id;
use leptos::*;

pub(crate) fn mcp_app_title(payload: &serde_json::Value) -> String {
    payload
        .pointer("/tool/title")
        .or_else(|| payload.pointer("/tool/annotations/title"))
        .or_else(|| payload.pointer("/tool/name"))
        .and_then(serde_json::Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("MCP App")
        .to_string()
}

pub(crate) fn mcp_app_instance_id(
    frame_id: &str,
    presentation_id: &str,
    payload: &serde_json::Value,
) -> String {
    let identity = (!presentation_id.is_empty())
        .then_some(presentation_id)
        .or_else(|| {
            payload
                .pointer("/resource/uri")
                .or_else(|| payload.pointer("/tool/name"))
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("app");
    format!("mcp-app:{frame_id}:{identity}")
}

#[component]
pub(crate) fn McpAppPreview(instance_id: String, payload_json: String) -> impl IntoView {
    let dom_id = unique_dom_id("center-mcp-app");
    {
        let mount_id = instance_id.clone();
        let mount_dom_id = dom_id.clone();
        let mount_payload = payload_json.clone();
        create_effect(move |_| {
            let _ = mount_mcp_app(&mount_id, &mount_dom_id, &mount_payload);
        });
    }
    {
        let parked_id = instance_id.clone();
        on_cleanup(move || park_mcp_app(&parked_id));
    }
    view! {
        <div class="center-mcp-app" id=dom_id data-mcp-app-id=instance_id></div>
    }
}
