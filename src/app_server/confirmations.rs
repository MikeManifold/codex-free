//! Caller-scoped deferred confirmations. An operation is resumed, never replayed.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::client::ServerRequest;

pub(super) fn random_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS randomness unavailable");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) fn text_result(data: Value) -> Value {
    json!({"content":[{"type":"text","text":data.to_string()}],"structuredContent":data})
}

pub(super) fn error_result(message: impl Into<String>) -> Value {
    json!({"content":[{"type":"text","text":message.into()}],"isError":true})
}

struct Confirmation {
    id: String,
    request: ServerRequest,
    expires: Instant,
    expires_ms: i64,
}

struct State {
    status: &'static str,
    result: Option<Value>,
    confirmation: Option<Confirmation>,
    exposed: bool,
    finished: Option<Instant>,
}

pub(super) struct Operation {
    pub id: String,
    owner: String,
    tool: String,
    state: Mutex<State>,
    notify: Notify,
    pub cancelled: CancellationToken,
}

impl Operation {
    pub fn running(&self) {
        self.state.lock().unwrap().status = "running";
    }
    pub fn complete(&self, result: Value) {
        let mut state = self.state.lock().unwrap();
        if let Some(form) = state.confirmation.take() {
            form.request.respond(json!({"action":"cancel"}));
        }
        state.status = "completed";
        state.result = Some(result);
        state.finished = Some(Instant::now());
        drop(state);
        self.notify.notify_waiters();
    }
    fn cancel_form(&self, id: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        if state
            .confirmation
            .as_ref()
            .is_some_and(|f| id.is_none_or(|id| id == f.id))
        {
            let form = state.confirmation.take().unwrap();
            state.status = "running";
            form.request.respond(json!({"action":"cancel"}));
        }
        drop(state);
        self.notify.notify_waiters();
    }
    pub fn cancel(&self) {
        self.cancelled.cancel();
        self.cancel_form(None);
    }
}

pub(super) struct Operations {
    operations: Mutex<HashMap<String, Arc<Operation>>>,
    active: Mutex<Option<Weak<Operation>>>,
    timeout: Duration,
}

impl Operations {
    pub fn new(timeout: Duration) -> Self {
        Self {
            operations: Mutex::new(HashMap::new()),
            active: Mutex::new(None),
            timeout,
        }
    }
    pub fn create(&self, owner: &str, tool: &str) -> Result<Arc<Operation>, String> {
        self.sweep();
        let mut operations = self.operations.lock().unwrap();
        if operations.len() >= 64 {
            return Err(
                "Too many bridge operations; retrieve or cancel pending operations first".into(),
            );
        }
        let op = Arc::new(Operation {
            id: random_id(),
            owner: owner.into(),
            tool: tool.into(),
            state: Mutex::new(State {
                status: "queued",
                result: None,
                confirmation: None,
                exposed: false,
                finished: None,
            }),
            notify: Notify::new(),
            cancelled: CancellationToken::new(),
        });
        operations.insert(op.id.clone(), op.clone());
        Ok(op)
    }
    pub fn set_active(&self, op: Option<&Arc<Operation>>) {
        *self.active.lock().unwrap() = op.map(Arc::downgrade);
    }
    pub fn request(&self, request: ServerRequest) -> bool {
        let op = self.active.lock().unwrap().as_ref().and_then(Weak::upgrade);
        let Some(op) = op else {
            request.respond(json!({"action":"cancel"}));
            return false;
        };
        let mode = request.params["mode"].as_str().unwrap_or("");
        let mut state = op.state.lock().unwrap();
        if state.confirmation.is_some()
            || state.status != "running"
            || op.cancelled.is_cancelled()
            || ![
                "form",
                "openai/form",
                "openaiForm",
                "url",
                "openai/userVerification",
            ]
            .contains(&mode)
        {
            request.respond(json!({"action":"cancel"}));
            return false;
        }
        let id = random_id();
        state.confirmation = Some(Confirmation {
            id: id.clone(),
            request,
            expires: Instant::now() + self.timeout,
            expires_ms: chrono::Utc::now().timestamp_millis() + self.timeout.as_millis() as i64,
        });
        state.status = "awaiting_confirmation";
        drop(state);
        op.notify.notify_waiters();
        let weak = Arc::downgrade(&op);
        let timeout = self.timeout;
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            if let Some(op) = weak.upgrade() {
                op.cancel_form(Some(&id));
            }
        });
        true
    }
    fn owned(&self, owner: &str, id: &str) -> Result<Arc<Operation>, String> {
        self.operations
            .lock()
            .unwrap()
            .get(id)
            .filter(|op| op.owner == owner)
            .cloned()
            .ok_or_else(|| "Unknown operation for this caller".into())
    }
    pub fn list(&self, owner: &str) -> Value {
        self.sweep();
        let operations = self
            .operations
            .lock()
            .unwrap()
            .values()
            .filter(|op| op.owner == owner)
            .filter_map(|op| {
                let state = op.state.lock().unwrap();
                if state.status == "completed" {
                    return None;
                }
                let mut value = json!({"operationId":op.id,"tool":op.tool,"status":state.status});
                if let Some(form) = &state.confirmation {
                    value["confirmation"] = public_form(form);
                }
                Some(value)
            })
            .collect::<Vec<_>>();
        text_result(json!({"operations":operations}))
    }
    pub async fn observe(&self, op: &Arc<Operation>, wait: Option<Duration>) -> Value {
        let deadline = wait.map(|wait| tokio::time::Instant::now() + wait);
        loop {
            let notified = op.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let value = {
                let mut state = op.state.lock().unwrap();
                if state.status == "completed" {
                    Some((state.result.clone().unwrap(), !state.exposed))
                } else if state.status == "awaiting_confirmation"
                    || deadline.is_some_and(|d| tokio::time::Instant::now() >= d)
                {
                    state.exposed = true;
                    let mut value = json!({"status":state.status,"operationId":op.id,"tool":op.tool,
                        "instruction":"Do not repeat the original call. Display the actual confirmation to the human and reply once with codex_confirmation_respond. Retrieve completion with codex_operation_result."});
                    if let Some(form) = &state.confirmation {
                        value["confirmation"] = public_form(form);
                    }
                    Some((text_result(value), false))
                } else {
                    None
                }
            };
            if let Some((value, remove)) = value {
                if remove {
                    self.operations.lock().unwrap().remove(&op.id);
                }
                return value;
            }
            if let Some(at) = deadline {
                tokio::select! {_=notified=>{},_=tokio::time::sleep_until(at)=>{}}
            } else {
                notified.await;
            }
        }
    }
    pub async fn result(&self, owner: &str, id: &str, wait: Duration) -> Result<Value, String> {
        let op = self.owned(owner, id)?;
        Ok(self.observe(&op, Some(wait)).await)
    }
    pub async fn respond(&self, owner: &str, args: &Value) -> Result<Value, String> {
        let op = self.owned(owner, args["operationId"].as_str().unwrap_or(""))?;
        {
            let mut state = op.state.lock().unwrap();
            let form = state
                .confirmation
                .as_ref()
                .ok_or("This confirmation is expired or already answered")?;
            if Some(form.id.as_str()) != args["confirmationId"].as_str()
                || Instant::now() >= form.expires
            {
                return Err("This confirmation is expired or belongs to another operation".into());
            }
            let action = args["action"].as_str().unwrap_or("");
            if !["accept", "decline", "cancel"].contains(&action) {
                return Err("action must be accept, decline, or cancel".into());
            }
            if args["userConfirmed"] != true {
                return Err("An explicit human decision is required".into());
            }
            if action == "accept" {
                let mode = form.request.params["mode"].as_str().unwrap_or("");
                if mode == "openai/userVerification" {
                    return Err("Device verification must be completed in the supported Codex UI; text cannot replace device proof".into());
                }
                if mode != "url" {
                    let content = args
                        .get("content")
                        .ok_or("Accepted forms require actual user-provided content")?;
                    validate(content, &form.request.params["requestedSchema"])?;
                    if content.to_string().len() > 65536 {
                        return Err("Confirmation input is too large".into());
                    }
                }
            } else if args.get("content").is_some() {
                return Err("Decline/cancel must not contain form content".into());
            }
            let form = state.confirmation.take().unwrap();
            state.status = "running";
            let mut response = json!({"action":action});
            if action == "accept"
                && let Some(content) = args.get("content")
            {
                response["content"] = content.clone();
            }
            form.request.respond(response);
        }
        Ok(self.observe(&op, Some(Duration::from_secs(30))).await)
    }
    fn sweep(&self) {
        self.operations.lock().unwrap().retain(|_, op| {
            op.state
                .lock()
                .unwrap()
                .finished
                .is_none_or(|at| at.elapsed() <= self.timeout)
        });
    }
    pub fn shutdown(&self) {
        for op in self.operations.lock().unwrap().values() {
            op.cancel();
        }
    }
}

fn public_form(form: &Confirmation) -> Value {
    let mut value = form.request.params.clone();
    if let Some(map) = value.as_object_mut() {
        for key in ["threadId", "turnId", "challenge"] {
            map.remove(key);
        }
    }
    value["id"] = json!(form.id);
    value["expiresAt"] = json!(form.expires_ms);
    value["requiresHumanDecision"] = json!(true);
    value
}

/// Validate the same form-schema subset handled by the old bridge.
fn validate(value: &Value, schema: &Value) -> Result<(), String> {
    if let Some(items) = schema["enum"].as_array()
        && !items.contains(value)
    {
        return Err("Confirmation value is not an allowed option".into());
    }
    if let Some(item) = schema.get("const")
        && item != value
    {
        return Err("Confirmation value does not match required constant".into());
    }
    if let Some(choices) = schema
        .get("oneOf")
        .or_else(|| schema.get("anyOf"))
        .and_then(Value::as_array)
        && !choices.iter().any(|choice| validate(value, choice).is_ok())
    {
        return Err("Confirmation value does not match an allowed choice".into());
    }
    match schema["type"].as_str().unwrap_or("") {
        "object" => {
            let map = value
                .as_object()
                .ok_or("Confirmation value must be an object")?;
            if let Some(required) = schema["required"].as_array() {
                for key in required.iter().filter_map(Value::as_str) {
                    if !map.contains_key(key) {
                        return Err(format!("Missing confirmation field: {key}"));
                    }
                }
            }
            for (key, item) in map {
                if schema["additionalProperties"] == false
                    && schema["properties"].get(key).is_none()
                {
                    return Err(format!("Unknown confirmation field: {key}"));
                }
                validate(item, &schema["properties"][key])?;
            }
        }
        "array" => {
            let items = value
                .as_array()
                .ok_or("Confirmation value must be an array")?;
            if items.len() < (schema["minItems"].as_u64().unwrap_or(0) as usize)
                || schema["maxItems"]
                    .as_u64()
                    .is_some_and(|max| items.len() > max as usize)
            {
                return Err("Confirmation array length is invalid".into());
            }
            for item in items {
                validate(item, &schema["items"])?;
            }
        }
        "string" => {
            let text = value
                .as_str()
                .ok_or("Confirmation value must be a string")?;
            let len = text.encode_utf16().count() as u64;
            if len < schema["minLength"].as_u64().unwrap_or(0)
                || schema["maxLength"].as_u64().is_some_and(|max| len > max)
            {
                return Err("Confirmation string length is invalid".into());
            }
        }
        "boolean" if !value.is_boolean() => {
            return Err("Confirmation value must be a boolean".into());
        }
        "integer" | "number" => {
            let number = value
                .as_f64()
                .ok_or("Confirmation value must be a number")?;
            if (schema["type"] == "integer" && number.fract() != 0.0)
                || schema["minimum"].as_f64().is_some_and(|n| number < n)
                || schema["maximum"].as_f64().is_some_and(|n| number > n)
            {
                return Err("Confirmation number is invalid".into());
            }
        }
        _ => {}
    }
    Ok(())
}
