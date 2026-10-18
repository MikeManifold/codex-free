//! An isolated native capture service, owned by one App Server connection.
//! Never attach cleanup to the desktop application's shared service.
use std::{path::PathBuf, process::Stdio, time::Duration};

use serde_json::{Value, json};
use tokio::{process::Command, sync::Mutex};

use super::{client::RpcError, config::Settings};

pub(super) struct NativeUi {
    directory: tempfile::TempDir,
    executable: PathBuf,
    configured_repls: Vec<String>,
    repl_overrides: Vec<(String, String)>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    child: Option<tokio::process::Child>,
    closed: bool,
}

impl NativeUi {
    pub fn prepare(settings: &Settings) -> Result<Option<std::sync::Arc<Self>>, RpcError> {
        if !cfg!(target_os = "macos") {
            return Ok(None);
        }
        let config = match std::fs::read_to_string(settings.codex_home.join("config.toml")) {
            Ok(text) => toml::from_str::<toml::Value>(&text)
                .map_err(|_| RpcError::message("Invalid native UI runtime configuration"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                toml::Value::Table(Default::default())
            }
            Err(_) => {
                return Err(RpcError::message(
                    "Native UI runtime configuration is unavailable",
                ));
            }
        };
        let servers = config.get("mcp_servers").and_then(toml::Value::as_table);
        // A virtual cua_repl can ignore MCP env overrides. Select only the
        // configured official Node runtime, and reject that virtual shortcut.
        let configured_repls = ["node_repl"]
            .into_iter()
            .filter(|name| servers.is_some_and(|s| s.contains_key(*name)))
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut repl_overrides = Vec::new();
        for name in &configured_repls {
            let env = servers.and_then(|s| s.get(name)).and_then(|s| s.get("env"));
            let value = |key: &str| {
                env.and_then(|e| e.get(key))
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| settings.env.get(key).cloned())
                    .or_else(|| std::env::var(key).ok())
            };
            // Use the configured, isolated REPL rather than the virtual CUA
            // server, which can attach to the desktop's shared native service.
            // This is the same bootstrap as the installed official CUA launcher.
            let banner = format!(
                "{}\nawait import(\"@oai/cua/tinyskyAlt\");\n",
                value("NODE_REPL_JS_BANNER").unwrap_or_default()
            );
            let mut allowlist = value("NODE_REPL_UNTRUSTED_ENV_ALLOWLIST")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            for key in ["CUA_REPL_ENABLED_SURFACES", "CUA_REPL_BROWSER_ENV"] {
                if !allowlist.iter().any(|v| v == key) {
                    allowlist.push(key.into());
                }
            }
            for (key, value) in [
                ("NODE_REPL_JS_BANNER", banner),
                ("NODE_REPL_UNTRUSTED_ENV_ALLOWLIST", allowlist.join(",")),
                (
                    "CUA_REPL_ENABLED_SURFACES",
                    value("CUA_REPL_ENABLED_SURFACES").unwrap_or_else(|| "browser,computer".into()),
                ),
            ] {
                repl_overrides.push((format!("mcp_servers.{name}.env.{key}"), value));
            }
        }
        let app = settings
            .env
            .get("SKY_CUA_SERVICE_PATH")
            .map(PathBuf::from)
            .or_else(|| {
                configured_repls.iter().find_map(|name| {
                    servers?
                        .get(name)?
                        .get("env")?
                        .get("SKY_CUA_SERVICE_PATH")?
                        .as_str()
                        .map(PathBuf::from)
                })
            })
            .unwrap_or_else(|| {
                settings
                    .codex_home
                    .join("computer-use/Codex Computer Use.app")
            });
        // Keep below sockaddr_un's path limit, even when macOS TMPDIR is long.
        let directory = tempfile::Builder::new()
            .prefix("codex-free-ui-")
            .tempdir_in("/tmp")
            .map_err(|_| RpcError::message("Could not create a private native UI endpoint"))?;
        Ok(Some(std::sync::Arc::new(Self {
            directory,
            executable: app.join("Contents/MacOS/SkyComputerUseService"),
            configured_repls,
            repl_overrides,
            state: Mutex::new(State::default()),
        })))
    }

    fn socket(&self) -> PathBuf {
        self.directory.path().join("cu.sock")
    }

    pub fn repl<'a>(&'a self, available: &[&str]) -> Option<&'a str> {
        self.configured_repls
            .iter()
            .map(String::as_str)
            .find(|name| available.contains(name))
    }

    pub fn owns_repl(&self, name: &str) -> bool {
        self.configured_repls.iter().any(|repl| repl == name)
    }

    pub fn configure(&self, command: &mut Command) {
        let endpoint = self.socket().to_string_lossy().into_owned();
        // Built-in cua_repl inherits the App Server environment. Configured
        // stdio REPLs instead receive an explicit MCP environment allowlist.
        command.env("SKY_CUA_SERVICE_NATIVE_PIPE_PATH", &endpoint);
        for name in &self.configured_repls {
            command.arg("-c").arg(format!(
                "mcp_servers.{name}.env.SKY_CUA_SERVICE_NATIVE_PIPE_PATH={}",
                serde_json::to_string(&endpoint).unwrap()
            ));
        }
        for (key, value) in &self.repl_overrides {
            command
                .arg("-c")
                .arg(format!("{key}={}", serde_json::to_string(value).unwrap()));
        }
    }

    pub async fn ensure_started(
        &self,
        settings: &Settings,
        runtime_pid: u32,
    ) -> Result<(), RpcError> {
        let mut state = self.state.lock().await;
        if state.closed {
            return Err(RpcError::message(
                "Native UI session has ended; the operation was not run",
            ));
        }
        if let Some(child) = &mut state.child {
            return match child.try_wait() {
                Ok(None) => Ok(()),
                _ => Err(RpcError::message(
                    "Owned native UI service exited; inspect external state before retrying",
                )),
            };
        }
        if !self.executable.is_file() {
            return Err(RpcError::message(
                "Official native UI service is unavailable; no UI operation was run",
            ));
        }
        // The official application also hosts a global login socket. Protect
        // the desktop's endpoints from unlink/rebind while preserving the
        // helper's normal auth/policy checks and their existing state files.
        let home = settings
            .env
            .get("HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .ok_or_else(|| RpcError::message("Native UI requires a home directory"))?;
        let protected = [
            PathBuf::from("/private/tmp/com.openai.sky.CUAService"),
            PathBuf::from("/tmp/com.openai.sky.CUAService"),
            home.join("Library/Group Containers/2DC432GLL2.com.openai.sky.CUAService/IPC"),
        ];
        let mut profile = "(version 1)(allow default)".to_string();
        for path in protected {
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            for path in [path, canonical] {
                profile.push_str(&format!(
                    "(deny file-write* (subpath {}))",
                    serde_json::to_string(&path).unwrap()
                ));
            }
        }
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command.args(["-p", &profile]).arg(&self.executable);
        command
            .envs(&settings.env)
            .env("CODEX_HOME", &settings.codex_home)
            .env("SKY_CUA_SERVICE_NATIVE_PIPE_PATH", self.socket())
            .env("TMPDIR", self.directory.path())
            .current_dir(&settings.spawn_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for name in &settings.removed_env {
            command.env_remove(name);
        }
        #[cfg(unix)]
        command.process_group(runtime_pid as i32);
        let child = command
            .spawn()
            .map_err(|_| RpcError::message("Could not start the owned native UI service"))?;
        state.child = Some(child);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        loop {
            let child = state.child.as_mut().unwrap();
            if !matches!(child.try_wait(), Ok(None)) {
                return Err(RpcError::message(
                    "Owned native UI service exited before becoming ready",
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                if std::fs::symlink_metadata(self.socket()).is_ok_and(|m| m.file_type().is_socket())
                {
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.start_kill();
                return Err(RpcError::message(
                    "Owned native UI service did not become ready; no UI operation was run",
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn stop(&self) -> bool {
        let mut state = self.state.lock().await;
        state.closed = true;
        let Some(child) = &mut state.child else {
            return true;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            state.child = None;
            return true;
        }
        // This is our Child handle, never a PID found by process-name search.
        #[cfg(unix)]
        if let Some(pid) = child.id()
            && !terminate_application(pid)
        {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        if !matches!(
            tokio::time::timeout(Duration::from_secs(3), child.wait()).await,
            Ok(Ok(_))
        ) {
            let _ = child.start_kill();
            if !matches!(
                tokio::time::timeout(Duration::from_secs(2), child.wait()).await,
                Ok(Ok(_))
            ) {
                return false;
            }
        }
        state.child = None;
        true
    }

    pub async fn stopped(&self) -> bool {
        let mut state = self.state.lock().await;
        state
            .child
            .as_mut()
            .is_none_or(|child| matches!(child.try_wait(), Ok(Some(_))))
    }

    pub async fn diagnostics(&self) -> Value {
        let mut state = self.state.lock().await;
        let pid = state.child.as_mut().and_then(|child| {
            matches!(child.try_wait(), Ok(None))
                .then(|| child.id())
                .flatten()
        });
        json!({"isolation":"caller-owned-native-service","pid":pid,"endpoint":self.socket(),"closed":state.closed})
    }
}

// Ask only our exact application process to run its termination handlers.
// The caller still verifies exit; acceptance also checks actual stream release.
#[cfg(target_os = "macos")]
fn terminate_application(pid: u32) -> bool {
    use std::ffi::{c_char, c_void};
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}
    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *const c_void;
        fn objc_msgSend();
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let find: unsafe extern "C" fn(*mut c_void, *const c_void, i32) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let terminate: unsafe extern "C" fn(*mut c_void, *const c_void) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        let process_id: unsafe extern "C" fn(*mut c_void, *const c_void) -> i32 =
            std::mem::transmute(objc_msgSend as *const ());
        let app = find(
            objc_getClass(c"NSRunningApplication".as_ptr()),
            sel_registerName(c"runningApplicationWithProcessIdentifier:".as_ptr()),
            pid as i32,
        );
        let accepted = !app.is_null()
            && process_id(app, sel_registerName(c"processIdentifier".as_ptr())) == pid as i32
            && terminate(app, sel_registerName(c"terminate".as_ptr()));
        objc_autoreleasePoolPop(pool);
        accepted
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn terminate_application(_: u32) -> bool {
    false
}
