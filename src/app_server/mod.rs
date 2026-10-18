//! Native adapter for the official App Server. Local rescue tools stay separate.
mod client;
mod config;
mod confirmations;
#[cfg(test)]
mod tests;
mod tools;

pub use config::AppServerConfig;

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ServerPeerInfo, Tool as McpTool};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::types::{AppConfig, McpServerSpec, ToolResult};
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

pub const VERSION: &str = "0.4.0";

struct UtilityContext {
    generation: u64,
    thread_id: String,
    session_id: String,
    turn_id: String,
}

pub(crate) struct AppServerBridge {
    settings: config::Settings,
    rpc: RpcClient,
    operations: Arc<Operations>,
    serial: Mutex<()>,
    utility: Mutex<Option<UtilityContext>>,
    shutdown: CancellationToken,
}

/// Service lifetime is owned by Free, not by an in-flight task or a GUI process.
pub(crate) struct AppServerGuard(Arc<AppServerBridge>);
impl Drop for AppServerGuard {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

impl AppServerBridge {
    pub fn new(spec: &McpServerSpec, app: &AppConfig) -> Result<Arc<Self>, String> {
        let settings = AppServerConfig::resolve(spec, app)?;
        let operations = Arc::new(Operations::new(settings.confirmation_timeout));
        let handler_operations = operations.clone();
        let rpc = RpcClient::new(
            settings.clone(),
            Arc::new(move |request| handler_operations.request(request)),
        );
        Ok(Arc::new(Self {
            settings,
            rpc,
            operations,
            serial: Mutex::new(()),
            utility: Mutex::new(None),
            shutdown: CancellationToken::new(),
        }))
    }
    pub fn guard(self: &Arc<Self>) -> AppServerGuard {
        AppServerGuard(self.clone())
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
            "instructions":"Native Rust App Server adapter. MCP calls use the installed official CLI without starting model turns. Discover live server/tool metadata through codex_mcp_status, and use codex_cua_state before choosing browser backends. Independent local_shell/exec remains available for recovery."})).unwrap())
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
            });
        }
        let u = slot.as_ref().unwrap();
        Ok(
            json!({"threadId":u.thread_id,"metadata":json!({"session_id":u.session_id,"thread_id":u.thread_id,"turn_id":u.turn_id,"thread_source":"user"}).to_string()}),
        )
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
