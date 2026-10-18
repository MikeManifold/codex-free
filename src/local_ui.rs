use rmcp::model::{MetaObject, Resource, ResourceContents};
use serde_json::json;

pub const LOCAL_UI_URI: &str = "ui://codex-free/local-ui/v2/mcp-app.html";
const LEGACY_LOCAL_UI_URI: &str = "ui://codex-free/local-ui/v1/mcp-app.html";
pub const LOCAL_UI_MIME_TYPE: &str = "text/html;profile=mcp-app";
pub const LOCAL_UI_RESULT_META_KEY: &str = "io.github.hypnguyen1209/local-ui";

pub fn tool_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui": {
            "resourceUri": LOCAL_UI_URI,
            "visibility": ["model", "app"]
        },
        "ui/resourceUri": LOCAL_UI_URI,
        "openai/outputTemplate": LOCAL_UI_URI,
        "openai/widgetAccessible": true,
        "openai/toolInvocation/invoking": "Preparing interactive view",
        "openai/toolInvocation/invoked": "Interactive view ready"
    }))
    .expect("static local UI tool metadata must be an object")
}

pub fn resource_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui": { "prefersBorder": false }
    }))
    .expect("static local UI resource metadata must be an object")
}

pub fn result_meta(title: &str, source_path: &str, html: &str, height: u64) -> MetaObject {
    let mut meta = MetaObject::new();
    meta.0.insert(
        LOCAL_UI_RESULT_META_KEY.to_string(),
        json!({
            "version": 1,
            "kind": "html",
            "title": title,
            "sourcePath": source_path,
            "height": height,
            "html": html
        }),
    );
    meta
}

pub fn resource() -> Resource {
    Resource::new(LOCAL_UI_URI, "codex-free-local-ui")
        .with_title("Interactive project UI")
        .with_description(
            "Explicit project preview: HTML, images/SVG, data tables and UTF-8 source",
        )
        .with_mime_type(LOCAL_UI_MIME_TYPE)
        .with_size(LOCAL_UI_HTML.len() as u64)
        .with_meta(resource_meta())
}

pub fn contents() -> ResourceContents {
    ResourceContents::text(LOCAL_UI_HTML, LOCAL_UI_URI)
        .with_mime_type(LOCAL_UI_MIME_TYPE)
        .with_meta(resource_meta())
}

pub fn contents_for_uri(uri: &str) -> Option<ResourceContents> {
    match uri {
        LOCAL_UI_URI => Some(contents()),
        LEGACY_LOCAL_UI_URI => Some(
            ResourceContents::text(LOCAL_UI_HTML, LEGACY_LOCAL_UI_URI)
                .with_mime_type(LOCAL_UI_MIME_TYPE)
                .with_meta(resource_meta()),
        ),
        _ => None,
    }
}

pub const LOCAL_UI_HTML: &str = include_str!("local_ui.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_uses_mcp_app_mime_type() {
        let resource = resource();
        let value = serde_json::to_value(resource).unwrap();
        assert_eq!(value["uri"], LOCAL_UI_URI);
        assert_eq!(value["mimeType"], LOCAL_UI_MIME_TYPE);
    }

    #[test]
    fn tool_meta_links_resource_and_compat_alias() {
        let meta = serde_json::to_value(tool_meta()).unwrap();
        assert_eq!(meta["ui"]["resourceUri"], LOCAL_UI_URI);
        assert_eq!(meta["openai/outputTemplate"], LOCAL_UI_URI);
    }

    #[test]
    fn result_html_is_widget_only_metadata() {
        let meta = result_meta("demo", "viewer.html", "<b>ok</b>", 480);
        let payload = meta.get(LOCAL_UI_RESULT_META_KEY).unwrap();
        assert_eq!(payload["title"], "demo");
        assert_eq!(payload["sourcePath"], "viewer.html");
        assert_eq!(payload["height"], 480);
        assert_eq!(payload["html"], "<b>ok</b>");
    }
}
