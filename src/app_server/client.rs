//! Owned App Server subprocess and newline-delimited JSON-RPC. Never replay calls.
use std::{
    collections::{HashMap, VecDeque},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{mpsc, oneshot, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use super::{VERSION, config::Settings};

#[derive(Debug, Clone)]
pub(super) struct RpcError {
    pub message: String,
    pub rpc: Option<Value>,
}

impl RpcError {
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            rpc: None,
        }
    }
    pub fn unsupported(&self) -> bool {
        self.rpc.as_ref().and_then(|v| v["code"].as_i64()) == Some(-32601)
            || [
                "unknown method",
                "unsupported method",
                "unknown variant",
                "unsupported variant",
            ]
            .iter()
            .any(|prefix| self.message.to_lowercase().starts_with(prefix))
    }
    pub fn display(&self) -> String {
        match &self.rpc {
            Some(rpc) => format!("{}\n{rpc}", self.message),
            None => self.message.clone(),
        }
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

pub(super) struct ServerRequest {
    pub id: Value,
    pub method: String,
    pub params: Value,
    sender: mpsc::UnboundedSender<Value>,
}

impl ServerRequest {
    pub fn respond(&self, result: Value) {
        let _ = self.sender.send(json!({"id":self.id,"result":result}));
    }
    fn unsupported(&self) {
        let _ = self.sender.send(json!({"id":self.id,"error":{"code":-32001,"message":format!("Unsupported server request: {}", self.method)}}));
    }
}

pub(super) type RequestHandler = Arc<dyn Fn(ServerRequest) -> bool + Send + Sync>;

pub(super) struct Connection {
    pub generation: u64,
    pub pid: u32,
    pub alive: Arc<AtomicBool>,
    sender: mpsc::UnboundedSender<Value>,
    pending: Pending,
    next_id: AtomicU64,
    stop: CancellationToken,
    closed: CancellationToken,
    pub init: Value,
    pub stderr: Arc<Mutex<VecDeque<String>>>,
    extension: watch::Receiver<Option<Instant>>,
}

impl Connection {
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst) && !self.stop.is_cancelled()
    }
    pub fn stop(&self) {
        self.stop.cancel();
    }
    pub fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.sender
            .send(json!({"method":method,"params":params}))
            .map_err(|_| RpcError::message("App Server is not running"))
    }
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(RpcError::message("App Server exited; call was not retried"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut extension = self.extension.clone();
        extension.borrow_and_update();
        let sent = self
            .sender
            .send(json!({"id":id,"method":method,"params":params}));
        if sent.is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(RpcError::message(
                "App Server is not running; call was not retried",
            ));
        }
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let result = loop {
            tokio::select! {
                result = &mut rx => break result.unwrap_or_else(|_| Err(RpcError::message("App Server response channel closed"))),
                _ = &mut deadline => break Err(RpcError::message(format!("App Server RPC timed out: {method}; check the original operation before retrying"))),
                _ = self.stop.cancelled() => break Err(RpcError::message("App Server stopped; check the original operation before retrying")),
                changed = extension.changed() => {
                    if changed.is_ok() && let Some(at) = *extension.borrow_and_update() { deadline.as_mut().reset(at); }
                }
            }
        };
        self.pending.lock().unwrap().remove(&id);
        result
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub(super) struct RpcClient {
    settings: Settings,
    connection: tokio::sync::Mutex<Option<Arc<Connection>>>,
    generation: AtomicU64,
    shutdown: CancellationToken,
    handler: RequestHandler,
}

impl RpcClient {
    pub fn new(settings: Settings, handler: RequestHandler) -> Self {
        Self {
            settings,
            handler,
            connection: tokio::sync::Mutex::new(None),
            generation: AtomicU64::new(0),
            shutdown: CancellationToken::new(),
        }
    }
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }
    pub async fn ensure_ready(&self) -> Result<Arc<Connection>, RpcError> {
        let mut slot = self.connection.lock().await;
        if self.shutdown.is_cancelled() {
            return Err(RpcError::message("App Server adapter is shutting down"));
        }
        if let Some(conn) = slot.as_ref().filter(|c| c.is_alive()) {
            return Ok(conn.clone());
        }
        if let Some(conn) = slot.take() {
            conn.stop();
            let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed.cancelled()).await;
        }
        let conn = self.start().await?;
        *slot = Some(conn.clone());
        Ok(conn)
    }
    pub async fn stop_owned(&self) {
        if let Some(conn) = self.connection.lock().await.as_ref() {
            conn.stop();
        }
    }
    async fn start(&self) -> Result<Arc<Connection>, RpcError> {
        let mut command = Command::new(&self.settings.cli);
        command
            .args(["app-server", "--listen", "stdio://"])
            .current_dir(&self.settings.spawn_dir)
            .envs(&self.settings.env)
            .env("CODEX_HOME", &self.settings.codex_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for name in &self.settings.removed_env {
            command.env_remove(name);
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|e| RpcError::message(format!("Could not start owned App Server: {e}")))?;
        let pid = child
            .id()
            .ok_or_else(|| RpcError::message("App Server has no process id"))?;
        let mut input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let errors = child.stderr.take().unwrap();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let stop = self.shutdown.child_token();
        let closed = CancellationToken::new();
        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        let (sender, mut receiver) = mpsc::unbounded_channel::<Value>();
        let (extend, extension) = watch::channel(None);
        let conn = Connection {
            generation: self.generation.fetch_add(1, Ordering::SeqCst) + 1,
            pid,
            alive: alive.clone(),
            sender: sender.clone(),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            stop: stop.clone(),
            closed: closed.clone(),
            init: Value::Null,
            stderr: stderr.clone(),
            extension,
        };
        let writer_stop = stop.clone();
        tokio::spawn(async move {
            loop {
                let value = tokio::select! { v = receiver.recv() => match v {Some(v)=>v,None=>break}, _=writer_stop.cancelled()=>break };
                let mut bytes = serde_json::to_vec(&value).unwrap();
                bytes.push(b'\n');
                if input.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            writer_stop.cancel();
        });
        let reader_stop = stop.clone();
        let reader_pending = pending.clone();
        let handler = self.handler.clone();
        let extension_duration = self.settings.confirmation_timeout + self.settings.request_timeout;
        tokio::spawn(async move {
            let mut reader = BufReader::new(output);
            let mut line = Vec::new();
            loop {
                line.clear();
                // The limit accommodates screenshots while bounding malformed output.
                let mut bounded = (&mut reader).take(32 * 1024 * 1024 + 1);
                let read = tokio::select! { r=bounded.read_until(b'\n', &mut line)=>r, _=reader_stop.cancelled()=>break };
                if !matches!(read, Ok(n) if n>0) || line.len() > 32 * 1024 * 1024 {
                    break;
                }
                let Ok(message) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                if message.get("id").is_none() {
                    continue;
                }
                if message.get("result").is_some()
                    || message.get("response").is_some()
                    || message.get("error").is_some()
                {
                    if let Some(id) = message["id"].as_u64()
                        && let Some(reply) = reader_pending.lock().unwrap().remove(&id)
                    {
                        let value = if let Some(error) = message.get("error") {
                            Err(RpcError {
                                message: error["message"]
                                    .as_str()
                                    .unwrap_or("App Server RPC error")
                                    .into(),
                                rpc: Some(error.clone()),
                            })
                        } else {
                            Ok(message
                                .get("result")
                                .or_else(|| message.get("response"))
                                .cloned()
                                .unwrap_or(Value::Null))
                        };
                        let _ = reply.send(value);
                    }
                } else if let Some(method) = message["method"].as_str() {
                    let request = ServerRequest {
                        id: message["id"].clone(),
                        method: method.into(),
                        params: message.get("params").cloned().unwrap_or(Value::Null),
                        sender: sender.clone(),
                    };
                    if method == "mcpServer/elicitation/request" {
                        if handler(request) {
                            let _ = extend.send(Some(Instant::now() + extension_duration));
                        }
                    } else {
                        request.unsupported();
                    }
                }
            }
            reader_stop.cancel();
        });
        let stderr_stop = stop.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(errors);
            let mut bytes = Vec::new();
            loop {
                bytes.clear();
                let mut bounded = (&mut reader).take(65537);
                let read = tokio::select! {r=bounded.read_until(b'\n',&mut bytes)=>r,_=stderr_stop.cancelled()=>break};
                if !matches!(read,Ok(n) if n>0) {
                    break;
                }
                if bytes.len() > 65536 {
                    stderr_stop.cancel();
                    break;
                }
                let line = String::from_utf8_lossy(&bytes);
                let mut tail = stderr.lock().unwrap();
                tail.push_back(line.chars().take(2000).collect::<String>());
                while tail.len() > 80 {
                    tail.pop_front();
                }
            }
        });
        let manager_stop = stop.clone();
        tokio::spawn(async move {
            tokio::select! {
                _=child.wait()=>{},
                _=manager_stop.cancelled()=>{
                    #[cfg(unix)] unsafe { libc::kill(-(pid as i32), libc::SIGTERM); }
                    #[cfg(not(unix))] { let _=child.start_kill(); }
                    if tokio::time::timeout(Duration::from_secs(3),child.wait()).await.is_err() {
                        #[cfg(unix)] unsafe { libc::kill(-(pid as i32),libc::SIGKILL); }
                        let _=child.start_kill(); let _=child.wait().await;
                    }
                }
            }
            alive.store(false, Ordering::SeqCst);
            manager_stop.cancel();
            for (_, reply) in pending.lock().unwrap().drain() {
                let _ = reply.send(Err(RpcError::message(
                    "App Server exited; the operation was not retried",
                )));
            }
            closed.cancel();
        });
        let mut conn = conn;
        let initialized = conn.request("initialize",json!({
            "clientInfo":{"name":"chatgpt_local_mcp_bridge","title":"ChatGPT Local Codex App Bridge","version":VERSION},
            "capabilities":{"experimentalApi":true,"explicitGatewayOauth":true,"extensions":{"openai/form":{}},"mcpServerOpenaiFormElicitation":true,
                "optOutNotificationMethods":["thread/started","item/agentMessage/delta","item/reasoning/delta","command/exec/outputDelta"]}
        }),Duration::from_secs(20)).await;
        conn.init = match initialized {
            Ok(value) => value,
            Err(error) => {
                conn.stop();
                let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed.cancelled()).await;
                return Err(error);
            }
        };
        conn.notify("initialized", json!({}))?;
        Ok(Arc::new(conn))
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}
