use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rmcp::model::{MetaObject, ToolAnnotations};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::exec_sessions::SessionState;
use crate::local_ui;
use crate::safe_path::resolve_safe_path;
use crate::tool::{Tool, arg_str};
use crate::types::{AppConfig, ToolResult};

const MAX_UI_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_UI_PAYLOAD_BYTES: usize = 3 * 1024 * 1024;
const DEFAULT_HEIGHT: u64 = 540;
const MIN_HEIGHT: u64 = 260;
const MAX_HEIGHT: u64 = 1000;

pub struct RenderUi;

impl RenderUi {
    pub const NAME: &'static str = "render_ui";

    // Keep the programmatic catalog alias; only the registered render_ui tool
    // attaches a widget. Generic MCP dispatch must remain presentation-neutral.
    pub async fn render_from_args(args: Value, config: &AppConfig) -> ToolResult {
        Self::run(args, config).await
    }

    fn normalized_height(args: &Value) -> Result<u64, String> {
        match args.get("height") {
            None => Ok(DEFAULT_HEIGHT),
            Some(value) => {
                let number = value
                    .as_f64()
                    .filter(|number| number.is_finite())
                    .ok_or_else(|| "height must be a finite number".to_string())?;
                Ok(number.round().clamp(MIN_HEIGHT as f64, MAX_HEIGHT as f64) as u64)
            }
        }
    }

    fn default_title(path: &std::path::Path) -> String {
        path.file_stem()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("Interactive view")
            .replace(['_', '-'], " ")
    }

    fn preview_body(extension: &str, bytes: &[u8], format: &str) -> Result<Value, String> {
        if bytes.len() > MAX_UI_FILE_BYTES {
            return Err("Preview source exceeds the 2 MiB limit".into());
        }
        if !matches!(format, "auto" | "text") {
            return Err("format must be auto or text".into());
        }
        let text = || {
            std::str::from_utf8(bytes).map_err(|_| "Text previews require valid UTF-8".to_string())
        };
        if format == "text" {
            return Ok(json!({ "kind": "text", "text": text()? }));
        }
        let mime = match extension {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            "bmp" => Some("image/bmp"),
            "svg" => {
                text()?;
                Some("image/svg+xml")
            }
            _ => None,
        };
        if let Some(mime) = mime {
            return Ok(json!({
                "kind": "image", "mimeType": mime, "data": STANDARD.encode(bytes)
            }));
        }
        match extension {
            "html" | "htm" => Ok(json!({ "kind": "html", "html": text()? })),
            "json" => {
                let source = text()?.trim_start_matches('\u{feff}');
                // Validate without round-tripping the user's numeric values.
                serde_json::from_str::<Value>(source)
                    .map_err(|error| format!("Invalid JSON: {error}"))?;
                Ok(json!({ "kind": "json", "text": source }))
            }
            "csv" | "tsv" => Ok(json!({ "kind": extension, "text": text()? })),
            "txt" | "md" | "markdown" | "log" | "rs" | "js" | "ts" |
            "jsx" | "tsx" | "py" | "css" | "sh" | "toml" | "yaml" | "yml" |
            "ini" | "conf" | "xml" => Ok(json!({ "kind": "text", "text": text()? })),
            _ => Err("Unsupported preview format. Use HTML, an image/SVG, JSON, CSV/TSV, or UTF-8 text. Markdown/code are source previews. Binary 3D, PDF, office and media files need a dedicated viewer; format=text is only for UTF-8 source.".into()),
        }
    }
}

#[async_trait]
impl Tool for RenderUi {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn title(&self) -> Option<String> {
        Some("Render project preview".to_string())
    }

    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(
            ToolAnnotations::new()
                .read_only(true)
                .destructive(false)
                .idempotent(true)
                .open_world(false),
        )
    }

    fn meta(&self) -> Option<MetaObject> {
        Some(local_ui::tool_meta())
    }

    fn description(&self) -> String {
        "Explicitly display one project-local preview in ChatGPT. Supports trusted self-contained HTML (including existing 3D viewers/charts/dashboards), PNG/JPEG/GIF/WebP/BMP/SVG images, JSON source, CSV/TSV tables, and UTF-8 text/code/Markdown source. Markdown is not typeset. File size <= 2 MiB; serialized preview payload <= 3 MiB. Use this standalone tool only when the user needs a visible preview; ordinary computer/browser tool results and screenshots must remain ordinary MCP content and do not need this tool. Binary 3D, PDF, office documents and audio/video are not native inputs. Does not modify the source file or fetch external URLs. Payload is sent only in widget metadata; a prepared result is not a browser-render acknowledgement.".to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Project-relative path to the supported local file to display" },
                "title": { "type": "string", "maxLength": 200, "description": "Optional preview title" },
                "height": { "type": "number", "minimum": MIN_HEIGHT, "maximum": MAX_HEIGHT, "description": "Preferred viewport height in pixels; default 540" },
                "format": { "type": "string", "enum": ["auto", "text"], "default": "auto", "description": "Detect from extension, or force a UTF-8 source preview (never execute as HTML)" }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        Self::run(args, config).await
    }
}

impl RenderUi {
    async fn run(args: Value, config: &AppConfig) -> ToolResult {
        let Some(path) = arg_str(&args, "path").filter(|path| !path.trim().is_empty()) else {
            return ToolResult::error("path must be a non-empty string");
        };
        let height = match Self::normalized_height(&args) {
            Ok(height) => height,
            Err(error) => return ToolResult::error(error),
        };
        let format = match args.get("format") {
            None => "auto",
            Some(Value::String(value)) if matches!(value.as_str(), "auto" | "text") => {
                value.as_str()
            }
            _ => return ToolResult::error("format must be auto or text"),
        };
        let title_arg = match args.get("title") {
            None => None,
            Some(Value::String(value)) if value.chars().count() <= 200 => Some(value.trim()),
            _ => return ToolResult::error("title must be a string of at most 200 characters"),
        };
        let file_path = match resolve_safe_path(path, &config.work_dir, false) {
            Ok(path) => path,
            Err(error) => return ToolResult::error(error),
        };
        if !file_path.is_file() {
            return ToolResult::error(format!("Preview file not found: {path}"));
        }
        let extension = file_path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let file = match tokio::fs::File::open(&file_path).await {
            Ok(file) => file,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        let mut bytes = Vec::new();
        // Bound the read itself, not just a metadata check before reading.
        if let Err(error) = file
            .take((MAX_UI_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await
        {
            return ToolResult::error(error.to_string());
        }
        let mut payload = match Self::preview_body(&extension, &bytes, format) {
            Ok(payload) => payload,
            Err(error) => return ToolResult::error(error),
        };
        let title = title_arg
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| Self::default_title(&file_path));
        payload["version"] = json!(2);
        payload["title"] = json!(title);
        payload["sourcePath"] = json!(path);
        payload["height"] = json!(height);
        let encoded_size = match serde_json::to_vec(&payload) {
            Ok(encoded) => encoded.len(),
            Err(error) => return ToolResult::error(error.to_string()),
        };
        if encoded_size > MAX_UI_PAYLOAD_BYTES {
            return ToolResult::error(
                "Encoded preview exceeds the 3 MiB payload limit; use a smaller file",
            );
        }
        let kind = payload["kind"].as_str().unwrap_or("preview");
        let mut output = ToolResult::text(format!(
            "Preview prepared: {path} ({kind}; {} source bytes). Client display is not yet verified.",
            bytes.len()
        ));
        let mut meta = MetaObject::new();
        meta.0
            .insert(local_ui::LOCAL_UI_RESULT_META_KEY.to_string(), payload);
        output.meta = Some(meta);
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn height_defaults_clamps_and_rejects_wrong_types() {
        assert_eq!(RenderUi::normalized_height(&json!({})).unwrap(), 540);
        assert_eq!(
            RenderUi::normalized_height(&json!({"height":100})).unwrap(),
            260
        );
        assert_eq!(
            RenderUi::normalized_height(&json!({"height":2000})).unwrap(),
            1000
        );
        assert!(RenderUi::normalized_height(&json!({"height":"540"})).is_err());
    }
    #[test]
    fn default_title_is_humanized() {
        assert_eq!(
            RenderUi::default_title(std::path::Path::new("preview-review.html")),
            "preview review"
        );
    }
    #[test]
    fn html_and_forced_source_remain_distinct() {
        let bytes = b"<script>test()</script>";
        assert_eq!(
            RenderUi::preview_body("html", bytes, "auto").unwrap()["html"],
            "<script>test()</script>"
        );
        let source = RenderUi::preview_body("html", bytes, "text").unwrap();
        assert_eq!(source["kind"], "text");
        assert!(source.get("html").is_none());
    }
    #[test]
    fn images_have_whitelisted_mime_and_base64() {
        for (ext, mime) in [
            ("png", "image/png"),
            ("jpeg", "image/jpeg"),
            ("svg", "image/svg+xml"),
        ] {
            let result = RenderUi::preview_body(ext, b"test", "auto").unwrap();
            assert_eq!(result["mimeType"], mime);
            assert_eq!(result["data"], "dGVzdA==");
        }
    }
    #[test]
    fn json_is_validated_without_losing_large_integers() {
        let text = r#"{"id":9007199254740993}"#;
        assert_eq!(
            RenderUi::preview_body("json", text.as_bytes(), "auto").unwrap()["text"],
            text
        );
        assert!(RenderUi::preview_body("json", b"bad", "auto").is_err());
    }
    #[test]
    fn limits_unknown_formats_and_invalid_utf8_fail_explicitly() {
        assert!(RenderUi::preview_body("txt", &[255], "auto").is_err());
        assert!(RenderUi::preview_body("pdf", b"%PDF", "auto").is_err());
        assert!(RenderUi::preview_body("txt", &vec![b'a'; MAX_UI_FILE_BYTES + 1], "auto").is_err());
        assert!(RenderUi::preview_body("txt", b"ok", "unexpected").is_err());
    }
    #[test]
    fn markdown_is_explicitly_source_not_html() {
        let body = RenderUi::preview_body("md", b"# Title", "auto").unwrap();
        assert_eq!(body["kind"], "text");
        assert!(body.get("html").is_none());
    }
}
