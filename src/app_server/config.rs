use std::{collections::HashMap, path::PathBuf, time::Duration};

use serde::Deserialize;

use crate::types::{AppConfig, McpServerSpec};

/// Same host settings as the former Node adapter; secrets remain external.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppServerConfig {
    pub config_file: Option<String>,
    pub version: Option<u32>,
    pub codex_cli: Option<String>,
    pub work_dir: Option<String>,
    pub codex_home: Option<String>,
    pub request_timeout_ms: Option<u64>,
    pub confirmation_timeout_ms: Option<u64>,
    pub allow_raw_rpc: Option<bool>,
}

#[derive(Clone)]
pub(super) struct Settings {
    pub cli: PathBuf,
    pub work_dir: PathBuf,
    pub codex_home: PathBuf,
    pub spawn_dir: PathBuf,
    pub request_timeout: Duration,
    pub confirmation_timeout: Duration,
    pub allow_raw_rpc: bool,
    pub env: HashMap<String, String>,
    pub removed_env: Vec<String>,
}

impl AppServerConfig {
    pub(super) fn resolve(spec: &McpServerSpec, app: &AppConfig) -> Result<Settings, String> {
        let inline = spec.app_server.clone().unwrap_or_default();
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("App Server requires a home directory")?;
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let spawn_dir = match &spec.cwd {
            Some(path) => expand(path, &cwd, &home)?,
            None => cwd.clone(),
        };
        let lookup = |name: &str| {
            spec.env
                .get(name)
                .cloned()
                .or_else(|| std::env::var(name).ok())
        };
        let selected = lookup("CODEX_APP_BRIDGE_CONFIG").or_else(|| inline.config_file.clone());
        let (mut merged, base) = if let Some(path) = selected {
            let path = expand(&path, &spawn_dir, &home)?;
            let data =
                std::fs::read(&path).map_err(|_| "App Server configuration file is unavailable")?;
            let config: Self =
                serde_json::from_slice(&data).map_err(|_| "Invalid App Server configuration")?;
            if config.version != Some(1) || config.config_file.is_some() {
                return Err(
                    "App Server configuration must have version=1 and no nested configFile".into(),
                );
            }
            (config, path.parent().unwrap_or(&spawn_dir).to_path_buf())
        } else {
            (Self::default(), spawn_dir.clone())
        };
        macro_rules! overlay { ($($field:ident),*) => { $(if inline.$field.is_some() { merged.$field = inline.$field; })* }; }
        overlay!(
            version,
            codex_cli,
            work_dir,
            codex_home,
            request_timeout_ms,
            confirmation_timeout_ms,
            allow_raw_rpc
        );
        if merged.version.is_some_and(|v| v != 1) {
            return Err("Unsupported App Server configuration version".into());
        }
        let timeout = |value: Option<u64>, default| {
            let value = value.unwrap_or(default);
            if !(1000..=600_000).contains(&value) {
                return Err("App Server timeouts must be 1000..600000 milliseconds".to_string());
            }
            Ok(Duration::from_millis(value))
        };
        let request_ms = lookup("CODEX_APP_BRIDGE_TIMEOUT_MS")
            .map(|s| s.parse::<u64>())
            .transpose()
            .map_err(|_| "Invalid App Server timeout")?
            .or(merged.request_timeout_ms);
        let cli = lookup("CODEX_APP_CLI")
            .or(merged.codex_cli)
            .unwrap_or_else(|| {
                "/Applications/Codex.app/Contents/Resources/codex-cli/bin/codex".into()
            });
        let work_dir = lookup("CODEX_APP_BRIDGE_WORK_DIR")
            .or(merged.work_dir)
            .unwrap_or_else(|| spawn_dir.to_string_lossy().into_owned());
        let codex_home = lookup("CODEX_HOME")
            .or(merged.codex_home)
            .unwrap_or_else(|| "~/.codex".into());
        let mut removed_env = vec![
            crate::process_env::CHILD_CONTROL_PLANE_API_KEY_ENV.into(),
            crate::process_env::CHILD_MCP_AUTHORIZATION_ENV.into(),
        ];
        if let Some(name) = app
            .openai_tunnel
            .as_ref()
            .and_then(|t| t.api_key_ref.strip_prefix("env:"))
        {
            removed_env.push(name.to_string());
        }
        Ok(Settings {
            cli: expand(&cli, &base, &home)?,
            work_dir: expand(&work_dir, &base, &home)?,
            codex_home: expand(&codex_home, &base, &home)?,
            spawn_dir,
            request_timeout: timeout(request_ms, 120_000)?,
            confirmation_timeout: timeout(merged.confirmation_timeout_ms, 300_000)?,
            allow_raw_rpc: lookup("CODEX_APP_BRIDGE_ALLOW_RAW_RPC")
                .map(|s| s == "1")
                .unwrap_or(merged.allow_raw_rpc.unwrap_or(false)),
            env: spec.env.clone(),
            removed_env,
        })
    }
}

fn expand(value: &str, base: &std::path::Path, home: &std::path::Path) -> Result<PathBuf, String> {
    if value.trim().is_empty() || value.contains('\0') {
        return Err("App Server paths must be nonempty".into());
    }
    let path = if value == "~" {
        home.to_path_buf()
    } else if let Some(tail) = value.strip_prefix("~/") {
        home.join(tail)
    } else {
        PathBuf::from(value)
    };
    Ok(if path.is_absolute() {
        path
    } else {
        base.join(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_config_reuses_private_file_and_resolves_relative_paths() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("bridge.local.json");
        std::fs::write(&file,r#"{"version":1,"codexCli":"bin/codex","workDir":"workspace","codexHome":"state","requestTimeoutMs":2345,"allowRawRpc":false}"#).unwrap();
        let app = crate::config::default_config(temp.path().to_path_buf());
        let spec = McpServerSpec {
            app_server: Some(AppServerConfig {
                config_file: Some(file.to_string_lossy().into_owned()),
                ..Default::default()
            }),
            env: HashMap::from([(
                "CODEX_HOME".into(),
                temp.path()
                    .join("override-state")
                    .to_string_lossy()
                    .into_owned(),
            )]),
            ..Default::default()
        };
        let resolved = AppServerConfig::resolve(&spec, &app).unwrap();
        assert_eq!(resolved.cli, temp.path().join("bin/codex"));
        assert_eq!(resolved.work_dir, temp.path().join("workspace"));
        assert_eq!(resolved.codex_home, temp.path().join("override-state"));
        assert_eq!(resolved.request_timeout, Duration::from_millis(2345));
        assert!(!resolved.allow_raw_rpc);
    }

    #[test]
    fn config_rejects_invalid_fields_versions_and_timeouts() {
        assert!(
            serde_json::from_str::<AppServerConfig>(r#"{"version":1,"unknown":true}"#).is_err()
        );
        let app = crate::config::default_config(PathBuf::from("/tmp"));
        for settings in [
            AppServerConfig {
                version: Some(2),
                ..Default::default()
            },
            AppServerConfig {
                request_timeout_ms: Some(0),
                ..Default::default()
            },
            AppServerConfig {
                confirmation_timeout_ms: Some(600001),
                ..Default::default()
            },
        ] {
            let spec = McpServerSpec {
                app_server: Some(settings),
                ..Default::default()
            };
            assert!(AppServerConfig::resolve(&spec, &app).is_err());
        }
    }

    #[test]
    fn owned_server_excludes_tunnel_credentials_from_child_environment() {
        let mut app = crate::config::default_config(PathBuf::from("/tmp"));
        app.openai_tunnel = Some(crate::types::OpenAiTunnelConfig {
            tunnel_id: "fixture".into(),
            api_key_ref: "env:FIXTURE_TUNNEL_SECRET".into(),
            organization_id: None,
            client_path: None,
        });
        let settings = AppServerConfig::resolve(&McpServerSpec::default(), &app).unwrap();
        assert!(
            settings
                .removed_env
                .contains(&"FIXTURE_TUNNEL_SECRET".to_string())
        );
        assert!(
            settings
                .removed_env
                .contains(&crate::process_env::CHILD_MCP_AUTHORIZATION_ENV.to_string())
        );
    }
}
