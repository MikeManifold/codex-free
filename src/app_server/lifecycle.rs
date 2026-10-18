//! Caller-owned runtime leases. MCP provides no host-turn completion event.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use rmcp::model::{CallToolRequestParams, ServerPeerInfo, Tool as McpTool};
use serde_json::json;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{AppServerSession, config::Settings, confirmations::text_result, map_result};
use crate::types::{AppConfig, McpServerSpec, ToolResult};

pub(super) const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) struct AppServerBridge {
    settings: Settings,
    sessions: Mutex<HashMap<String, Arc<AppServerSession>>>,
    shutdown: CancellationToken,
}

pub(crate) struct AppServerGuard(Arc<AppServerBridge>);
impl Drop for AppServerGuard {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

struct CallLease(Arc<AppServerSession>);
impl Drop for CallLease {
    fn drop(&mut self) {
        let mut activity = self.0.activity.lock().unwrap();
        activity.calls -= 1;
        activity.last_finished = Instant::now();
    }
}

pub(super) struct Activity {
    calls: usize,
    pub(super) last_finished: Instant,
}
impl Default for Activity {
    fn default() -> Self {
        Self {
            calls: 0,
            last_finished: Instant::now(),
        }
    }
}

impl AppServerBridge {
    pub fn new(spec: &McpServerSpec, app: &AppConfig) -> Result<Arc<Self>, String> {
        let bridge = Arc::new(Self {
            settings: super::AppServerConfig::resolve(spec, app)?,
            sessions: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
        });
        let weak = Arc::downgrade(&bridge);
        let stop = bridge.shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
                let Some(bridge) = Weak::upgrade(&weak) else {
                    break;
                };
                bridge.reap_idle(Instant::now(), IDLE_TIMEOUT).await;
            }
        });
        Ok(bridge)
    }

    pub fn guard(self: &Arc<Self>) -> AppServerGuard {
        AppServerGuard(self.clone())
    }
    pub fn tools() -> Vec<McpTool> {
        AppServerSession::tools()
    }
    pub fn info() -> Arc<ServerPeerInfo> {
        AppServerSession::info()
    }

    #[cfg(test)]
    pub(super) fn test_session(&self, owner: &str) -> Option<Arc<AppServerSession>> {
        self.sessions.lock().unwrap().get(owner).cloned()
    }

    fn shutdown(&self) {
        self.shutdown.cancel();
        for session in self.sessions.lock().unwrap().values() {
            session.shutdown();
        }
    }

    pub async fn call(
        self: &Arc<Self>,
        params: CallToolRequestParams,
        cancellation: Option<&CancellationToken>,
    ) -> ToolResult {
        if self.shutdown.is_cancelled() {
            return ToolResult::error("App Server adapter is shutting down");
        }
        let owner = params
            .meta
            .as_ref()
            .and_then(|m| m.get("codex-free/call-owner"))
            .and_then(|v| v.as_str())
            .unwrap_or("stdio-client")
            .to_string();
        if owner.len() > 200 {
            return ToolResult::error("Invalid caller identity");
        }
        if params.name == "codex_ui_session_end" {
            let session = self.sessions.lock().unwrap().remove(&owner);
            let Some(session) = session else {
                return map_result(text_result(json!({"ended":true,"alreadyEnded":true})));
            };
            let result = session.close().await;
            return map_result(result);
        }
        let (session, lease) = {
            let mut sessions = self.sessions.lock().unwrap();
            if !sessions.contains_key(&owner) && sessions.len() >= 32 {
                return ToolResult::error(
                    "Too many active App Server sessions; end an idle session first",
                );
            }
            let session = sessions
                .entry(owner)
                .or_insert_with(|| AppServerSession::new(self.settings.clone()))
                .clone();
            session.activity.lock().unwrap().calls += 1;
            (session.clone(), CallLease(session))
        };
        let result = session.call(params, cancellation).await;
        drop(lease);
        result
    }

    pub(super) async fn reap_idle(&self, now: Instant, timeout: Duration) {
        let expired = {
            let mut sessions = self.sessions.lock().unwrap();
            let owners = sessions
                .iter()
                .filter_map(|(owner, s)| {
                    let a = s.activity.lock().unwrap();
                    (a.calls == 0
                        && !s.operations.has_pending()
                        && now.saturating_duration_since(a.last_finished) >= timeout)
                        .then(|| owner.clone())
                })
                .collect::<Vec<_>>();
            owners
                .into_iter()
                .filter_map(|owner| sessions.remove(&owner))
                .collect::<Vec<_>>()
        };
        // Sessions were removed while holding the same lock used to acquire a call lease.
        // No later call can enter an expiring runtime.
        for session in expired {
            let result = session.close().await;
            tracing::info!(
                cleanup_failed = result["isError"] == true,
                "idle App Server session released"
            );
        }
    }
}
