//! Native adapter for the official App Server. Local rescue tools stay separate.
mod client;
mod config;
mod confirmations;
mod lifecycle;
#[cfg(test)]
mod tests;
mod tools;

pub use config::AppServerConfig;
pub(crate) use lifecycle::{AppServerBridge, AppServerGuard};

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ServerPeerInfo, Tool as McpTool};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use crate::types::McpServerSpec;
use crate::types::ToolResult;
use client::{Connection, RpcClient, RpcError};
use confirmations::{Operations, error_result};

struct PendingCall(Option<Arc<confirmations::Operation>>);
impl Drop for PendingCall {
    fn drop(&mut self) {
        if let Some(op) = &self.0 {
            op.cancel();
        }
    }
}

pub const VERSION: &str = "0.5.0";

struct UtilityContext {
    generation: u64,
    thread_id: String,
    session_id: String,
    turn_id: String,
    repl_servers: std::collections::HashSet<String>,
}

pub(super) struct AppServerSession {
    settings: config::Settings,
    rpc: RpcClient,
    operations: Arc<Operations>,
    serial: Mutex<()>,
    utility: Mutex<Option<UtilityContext>>,
    shutdown: CancellationToken,
    activity: std::sync::Mutex<lifecycle::Activity>,
}

impl AppServerSession {
    fn new(settings: config::Settings) -> Arc<Self> {
        let operations = Arc::new(Operations::new(settings.confirmation_timeout));
        let handler_operations = operations.clone();
        let rpc = RpcClient::new(
            settings.clone(),
            Arc::new(move |request| handler_operations.request(request)),
        );
        Arc::new(Self {
            settings,
            rpc,
            operations,
            serial: Mutex::new(()),
            utility: Mutex::new(None),
            shutdown: CancellationToken::new(),
            activity: std::sync::Mutex::new(Default::default()),
        })
    }
    fn shutdown(&self) {
        self.shutdown.cancel();
        self.operations.shutdown();
        self.rpc.shutdown();
    }
    pub fn tools() -> Vec<McpTool> {
        let mut tools: Vec<McpTool> = serde_json::from_str(include_str!("tools.json"))
            .expect("checked-in App Server schemas");
        for tool in &mut tools {
            if tool.name == "codex_mcp_status" {
                let schema = Arc::make_mut(&mut tool.input_schema);
                schema.get_mut("properties").and_then(Value::as_object_mut).unwrap().insert("toolName".into(),json!({"type":"string","description":"With serverName, return this live tool's exact metadata and schema. Tool names are discovered dynamically from App Server."}));
            }
        }
        tools
    }
    pub fn info() -> Arc<ServerPeerInfo> {
        Arc::new(serde_json::from_value(json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},
            "serverInfo":{"name":"codex-app-server-bridge","version":VERSION},
            "instructions":"Native Rust App Server adapter. Each trusted caller owns an isolated official CLI runtime. UI calls must use codex_cua_* or codex_mcp_call through this adapter, not a separate direct REPL. Consecutive calls reuse the caller's runtime; five idle minutes release it automatically, excluding running calls and pending confirmations. codex_ui_session_end releases it immediately. Never replay a write after expiration, cancellation, or errors. Independent local_shell/exec remains available for recovery."})).unwrap())
    }
    pub async fn call(
        self: &Arc<Self>,
        params: CallToolRequestParams,
        cancellation: Option<&CancellationToken>,
    ) -> ToolResult {
        let owner = params
            .meta
            .as_ref()
            .and_then(|m| m.get("codex-free/call-owner"))
            .and_then(Value::as_str)
            .unwrap_or("stdio-client")
            .to_string();
        if owner.len() > 200 {
            return ToolResult::error("Invalid caller identity");
        }
        if self.shutdown.is_cancelled() {
            return ToolResult::error(
                "This App Server session has ended; start a new call instead of replaying a write",
            );
        }
        let args = Value::Object(params.arguments.unwrap_or_default());
        let name = params.name.as_ref();
        let controlled = match name {
            "codex_confirmations_list" => Some(Ok(self.operations.list(&owner))),
            "codex_confirmation_respond" => Some(self.operations.respond(&owner, &args).await),
            "codex_operation_result" => {
                let wait = args.get("waitMs").and_then(Value::as_u64).unwrap_or(1000);
                if wait > 30000 {
                    Some(Err("waitMs must be 0..30000".into()))
                } else {
                    Some(
                        self.operations
                            .result(
                                &owner,
                                args["operationId"].as_str().unwrap_or(""),
                                Duration::from_millis(wait),
                            )
                            .await,
                    )
                }
            }
            _ => None,
        };
        if let Some(result) = controlled {
            return map_result(result.unwrap_or_else(error_result));
        }
        if !Self::tools().iter().any(|t| t.name.as_ref() == name) {
            return ToolResult::error(format!("Unknown tool: {name}"));
        }
        let operation = match self.operations.create(&owner, name) {
            Ok(op) => op,
            Err(error) => return ToolResult::error(error),
        };
        let mut pending = PendingCall(Some(operation.clone()));
        let bridge = self.clone();
        let op = operation.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let permit = tokio::select! {
                permit=bridge.serial.lock()=>permit,
                _=op.cancelled.cancelled()=>{op.complete(error_result("Operation cancelled before execution"));return;},
                _=bridge.shutdown.cancelled()=>{op.complete(error_result("Adapter is shutting down"));return;}
            };
            if op.cancelled.is_cancelled() || bridge.shutdown.is_cancelled() {
                op.complete(error_result("Operation cancelled before execution"));
                return;
            }
            op.running();
            bridge.operations.set_active(Some(&op));
            let result = tokio::select! {
                value=bridge.execute(&name,&args)=>value.unwrap_or_else(|error|error_result(error.display())),
                _=op.cancelled.cancelled()=>{
                    bridge.rpc.stop_owned().await;
                    error_result("Operation cancelled; inspect external state before retrying")
                },
                _=bridge.shutdown.cancelled()=>error_result("Adapter is shutting down; operation was not retried"),
            };
            if result["isError"] == true {
                // A timed-out RPC may still be executing upstream. End only this
                // caller's runtime rather than reusing uncertain UI state.
                let _ = bridge.cleanup_runtime().await;
            }
            bridge.activity.lock().unwrap().last_finished = tokio::time::Instant::now();
            op.complete(result);
            bridge.operations.set_active(None);
            drop(permit);
        });
        let value = if let Some(cancel) = cancellation {
            tokio::select! {
                value=self.operations.observe(&operation,None)=>value,
                _=cancel.cancelled()=>{operation.cancel();error_result("Cancelled by downstream client; operation was not retried")}
            }
        } else {
            self.operations.observe(&operation, None).await
        };
        pending.0 = None;
        map_result(value)
    }
    async fn utility(&self, conn: &Connection) -> Result<Value, RpcError> {
        let mut slot = self.utility.lock().await;
        if slot
            .as_ref()
            .is_none_or(|u| u.generation != conn.generation)
        {
            let response=self.request(conn,"thread/start",json!({"cwd":self.settings.work_dir,"approvalPolicy":"never","sandbox":"danger-full-access","ephemeral":true})).await?;
            let id = response
                .pointer("/thread/id")
                .or_else(|| response.get("threadId"))
                .or_else(|| response.get("id"))
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::message("thread/start did not return a thread id"))?
                .to_string();
            let session = response
                .pointer("/thread/sessionId")
                .or_else(|| response.get("sessionId"))
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string();
            let raw = confirmations::random_id();
            let turn_id = format!(
                "{}-{}-4{}-a{}-{}",
                &raw[..8],
                &raw[8..12],
                &raw[13..16],
                &raw[17..20],
                &raw[20..]
            );
            *slot = Some(UtilityContext {
                generation: conn.generation,
                thread_id: id,
                session_id: session,
                turn_id,
                repl_servers: Default::default(),
            });
        }
        let u = slot.as_ref().unwrap();
        Ok(
            json!({"threadId":u.thread_id,"metadata":json!({"session_id":u.session_id,"thread_id":u.thread_id,"turn_id":u.turn_id,"thread_source":"user"}).to_string()}),
        )
    }
    async fn cleanup_runtime(&self) -> Value {
        let utility = self.utility.lock().await.take();
        let mut warnings = Vec::new();
        if let Some(conn) = self.rpc.current().await
            && let Some(u) = utility.filter(|u| u.generation == conn.generation)
        {
            let metadata = json!({"session_id":u.session_id,"thread_id":u.thread_id,"turn_id":u.turn_id,"thread_source":"user"}).to_string();
            let mut servers = u.repl_servers.into_iter().collect::<Vec<_>>();
            servers.sort();
            for server in servers {
                let result = conn.request("mcpServer/tool/call", json!({
                        "threadId":u.thread_id,"server":server,"tool":"turn_ended",
                        "arguments":{"hook_event_name":"Stop","session_id":u.session_id,"turn_id":u.turn_id},
                        "_meta":{"x-codex-turn-metadata":metadata}
                    }), Duration::from_secs(5)).await;
                if !matches!(result, Ok(ref value) if value["isError"] != true) {
                    warnings.push(format!(
                        "{server} did not acknowledge turn cleanup; its owned process was stopped"
                    ));
                }
            }
            if conn
                .request(
                    "thread/unsubscribe",
                    json!({"threadId":u.thread_id}),
                    Duration::from_secs(5),
                )
                .await
                .is_err()
            {
                warnings.push(
                    "Thread unsubscribe was not acknowledged; its owned process was stopped".into(),
                );
            }
        }
        // Stopping this caller's owned child also bounds cleanup if a hook fails.
        // Other callers and the desktop App have independent processes.
        if !self.rpc.stop_owned().await {
            return error_result(
                "Owned runtime did not acknowledge shutdown; inspect it with native rescue tools before retrying",
            );
        }
        confirmations::text_result(
            json!({"ended":true,"alreadyEnded":false,"cleanup":"owned_runtime_stopped","warnings":warnings}),
        )
    }
    async fn close(&self) -> Value {
        self.shutdown.cancel();
        self.operations.shutdown();
        let permit = tokio::time::timeout(Duration::from_secs(6), self.serial.lock()).await;
        let result = self.cleanup_runtime().await;
        drop(permit);
        result
    }
    async fn request(
        &self,
        conn: &Connection,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        conn.request(method, params, self.settings.request_timeout)
            .await
    }
}

fn map_result(value: Value) -> ToolResult {
    match serde_json::from_value(value) {
        Ok(result) => crate::bridge::map_call_result(result),
        Err(_) => ToolResult::error("App Server did not return a valid MCP tool result"),
    }
}
