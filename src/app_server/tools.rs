use std::{collections::BTreeMap, time::Duration};

use base64::Engine;
use serde_json::{Map, Value, json};

use super::{
    AppServerBridge, VERSION,
    client::{Connection, RpcError},
    confirmations::text_result,
};

const SAFE_RPC: &[&str] = &[
    "config/read",
    "permissionProfile/list",
    "model/list",
    "experimentalFeature/list",
    "account/read",
    "account/usage/read",
    "mcpServerStatus/list",
    "mcpServer/resource/read",
    "skills/list",
    "hooks/list",
    "plugin/list",
    "plugin/read",
    "plugin/installed",
    "app/list",
    "app/read",
    "app/installed",
    "environment/info",
    "environment/status",
    "remoteControl/status/read",
    "project/list",
    "project/read",
    "fs/readFile",
    "fs/getMetadata",
    "fs/readDirectory",
];

impl AppServerBridge {
    pub(super) async fn execute(&self, name: &str, args: &Value) -> Result<Value, RpcError> {
        let conn = self.rpc.ensure_ready().await?;
        match name {
            "codex_app_status"=>{
                let status=match self.mcp_status(&conn,&json!({"detail":"toolsAndAuthOnly"})).await {
                    Ok(status)=>summarize_mcp(&status),Err(error)=>json!({"error":error.message}),
                };
                Ok(text_result(json!({"version":VERSION,"initialize":conn.init,"bridge":self.diagnostics(&conn).await,"mcp":status})))
            }
            "codex_exec"=>{
                let mut p=pick(args,&["command","env","timeoutMs"]);
                p["cwd"]=args.get("cwd").cloned().unwrap_or_else(||json!(self.settings.work_dir));
                p["tty"]=json!(false);p["streamStdin"]=json!(false);p["streamStdoutStderr"]=json!(false);
                let timeout=args["timeoutMs"].as_u64().map(|ms|Duration::from_millis(ms.saturating_add(10000).min(610000))).unwrap_or(self.settings.request_timeout);
                let response=conn.request("command/exec",p,timeout).await?;
                let mut parts=vec![format!("exitCode: {}",response.get("exitCode").map(Value::to_string).unwrap_or_else(||"unknown".into()))];
                for key in ["stdout","stderr"] {if let Some(value)=response[key].as_str().filter(|s|!s.is_empty()){parts.push(format!("{key}:\n{value}"));}}
                Ok(json!({"content":[{"type":"text","text":parts.join("\n")}],"structuredContent":response}))
            }
            "codex_fs_read"=>{
                let mut p=pick(args,&["followSymlinks"]);p["path"]=absolute_path(args)?;
                let response=self.request(&conn,"fs/readFile",p).await?;
                let Some(data)=response.get("dataBase64").or_else(||response.get("data_base64")).and_then(Value::as_str) else {return Ok(text_result(response));};
                let mode=args["encoding"].as_str().unwrap_or("utf8");
                let text=if mode=="base64" {data.to_string()} else {
                    let bytes=base64::engine::general_purpose::STANDARD.decode(data).map_err(|_|RpcError::message("App Server returned invalid file encoding"))?;
                    String::from_utf8_lossy(&bytes).into_owned()
                };
                let mut structured=response;structured["decodedEncoding"]=json!(mode);
                Ok(json!({"content":[{"type":"text","text":text}],"structuredContent":structured}))
            }
            "codex_fs_write"=>{
                let path=absolute_path(args)?;let content=args["content"].as_str().ok_or_else(||RpcError::message("content is required"))?;
                let data=if args["encoding"]=="base64" {content.to_string()} else {base64::engine::general_purpose::STANDARD.encode(content)};
                let mut p=pick(args,&["followSymlinks"]);p["path"]=path.clone();p["dataBase64"]=json!(data);
                let response=self.request(&conn,"fs/writeFile",p).await?;
                Ok(text_result(json!({"ok":true,"path":path,"response":response})))
            }
            "codex_fs_list"=>Ok(text_result(self.request(&conn,"fs/readDirectory",json!({"path":absolute_path(args)?})).await?)),
            "codex_mcp_status"=>{
                let response=self.mcp_status(&conn,args).await?;
                if let Some(tool)=args["toolName"].as_str() {
                    let server=args["serverName"].as_str().ok_or_else(||RpcError::message("serverName is required with toolName"))?;
                    let source=data(&response).iter().find(|item|item["name"]==server).ok_or_else(||RpcError::message("Unknown live MCP server"))?;
                    let definition=source["tools"].get(tool).or_else(||source["tools"].as_object().and_then(|m|m.values().find(|t|t["name"]==tool)))
                        .ok_or_else(||RpcError::message("Unknown live MCP tool; refresh codex_mcp_status"))?;
                    Ok(text_result(json!({"server":server,"tool":definition})))
                } else {Ok(text_result(if args["detail"]=="full" {response} else {summarize_mcp(&response)}))}
            }
            "codex_mcp_call"=>{
                let server=args["server"].as_str().ok_or_else(||RpcError::message("server is required"))?;
                let tool=args["tool"].as_str().ok_or_else(||RpcError::message("tool is required"))?;
                let arguments=match args.get("arguments") {
                    None|Some(Value::Null)=>json!({}),
                    Some(Value::Object(value))=>Value::Object(value.clone()),
                    _=>return Err(RpcError::message("arguments must be an object")),
                };
                self.call_mcp(&conn,server,tool,arguments,false).await
            }
            "codex_mcp_oauth_login"=>Ok(text_result(self.request(&conn,"mcpServer/oauth/login",pick(args,&["name"])).await?)),
            "codex_cua_docs"|"codex_cua_state"|"codex_cua_js"=>{
                let status=self.mcp_status(&conn,&json!({"detail":"toolsAndAuthOnly"})).await?;
                let servers=data(&status);let server=["cua_repl","node_repl"].into_iter().find(|name|servers.iter().any(|s|s["name"]==*name))
                    .ok_or_else(||RpcError::message("No trusted Computer Use runtime is available"))?;
                let params=match name {
                    "codex_cua_docs"=>json!({"code":"nodeRepl.write(await cua.rewriteDocumentation())","title":"Read live Computer Use documentation"}),
                    "codex_cua_state"=>json!({"code":"let s = await cua.getState({emit:false}); nodeRepl.write(JSON.stringify(s))","title":"Read Computer Use state"}),
                    _=>pick(args,&["code","title","timeout_ms"]),
                };
                let mut response=self.call_mcp(&conn,server,"js",params,true).await?;
                if name!="codex_cua_docs" && let Some(content)=response["content"].as_array_mut() && content.len()>1 {
                    content.retain(|b|!(b["type"]=="text" && b["text"].as_str().is_some_and(|s|s.starts_with("## Computer Use"))));
                }
                Ok(response)
            }
            "codex_skills_list"=>Ok(text_result(self.request(&conn,"skills/list",json!({"cwds":[self.settings.work_dir],"forceReload":args["forceReload"].as_bool().unwrap_or(false)})).await?)),
            "codex_plugins_list"=>Ok(text_result(self.request(&conn,"plugin/list",json!({"cwds":null,"marketplaceKinds":null,"forceRefetch":args["forceRefetch"].as_bool().unwrap_or(false)})).await?)),
            "codex_apps_list"|"codex_apps_read"=>Ok(text_result(self.apps(&conn,name,args).await?)),
            "codex_permissions_list"=>{
                let mut p=pick(args,&["limit","cursor"]);p["cwd"]=json!(self.settings.work_dir);
                Ok(text_result(self.request(&conn,"permissionProfile/list",p).await?))
            }
            "codex_projects_list"=>Ok(text_result(self.request(&conn,"project/list",pick(args,&["limit","cursor","sortKey","sortDirection"])).await?)),
            "codex_hooks_list"=>Ok(text_result(self.request(&conn,"hooks/list",json!({"cwds":[self.settings.work_dir]})).await?)),
            "codex_models_list"=>Ok(text_result(self.request(&conn,"model/list",pick(args,&["includeHidden","limit","cursor"])).await?)),
            "codex_account_read"=>{
                let v=self.request(&conn,"account/read",json!({})).await?;
                Ok(text_result(json!({"accountType":v.pointer("/account/type"),"planType":v.pointer("/account/planType"),"requiresOpenaiAuth":v.get("requiresOpenaiAuth"),"backendOrigin":v.pointer("/workspaceRouting/backendOrigin"),"accountRoutingOverride":v.pointer("/workspaceRouting/accountRoutingOverride")})))
            }
            "codex_remote_control_status"=>{
                let v=self.request(&conn,"remoteControl/status/read",json!({})).await?;
                Ok(text_result(json!({"status":v.get("status"),"serverName":v.get("serverName"),"environmentAttached":v.get("environmentId").is_some_and(|v|!v.is_null())})))
            }
            "codex_capabilities"=>{
                let mut output=Map::new();
                for (key,method,params) in [
                    ("mcp","mcpServerStatus/list",json!({"detail":"toolsAndAuthOnly"})),
                    ("skills","skills/list",json!({"cwds":[self.settings.work_dir],"forceReload":false})),
                    ("plugins","plugin/list",json!({"cwds":null,"marketplaceKinds":null,"forceRefetch":false})),
                    ("permissions","permissionProfile/list",json!({"cwd":self.settings.work_dir})),
                    ("projects","project/list",json!({"limit":200})),("hooks","hooks/list",json!({"cwds":[self.settings.work_dir]})),
                    ("models","model/list",json!({"limit":200})),("remoteControl","remoteControl/status/read",json!({})),
                ] {
                    let value=match self.request(&conn,method,params).await {Ok(v)=>if key=="mcp"{summarize_mcp(&v)}else{summarize_collection(&v)},Err(e)=>json!({"error":e.message})};
                    output.insert(key.into(),value);
                }
                let apps=match self.apps(&conn,"codex_apps_list",&json!({"source":"runtime","limit":200})).await {Ok(v)=>summarize_collection(&v),Err(e)=>json!({"error":e.message})};
                output.insert("apps".into(),apps);output.insert("bridge".into(),self.diagnostics(&conn).await);
                Ok(text_result(Value::Object(output)))
            }
            "codex_rpc"=>{
                let method=args["method"].as_str().ok_or_else(||RpcError::message("method is required"))?;
                if !self.settings.allow_raw_rpc && !SAFE_RPC.contains(&method){return Err(RpcError::message(format!("RPC method is not allowlisted: {method}")));}
                Ok(text_result(self.request(&conn,method,args.get("params").cloned().unwrap_or_else(||json!({}))).await?))
            }
            _=>Err(RpcError::message(format!("Unknown tool: {name}"))),
        }
    }
    async fn call_mcp(
        &self,
        conn: &Connection,
        server: &str,
        tool: &str,
        args: Value,
        metadata: bool,
    ) -> Result<Value, RpcError> {
        let scope = self.utility(conn).await?;
        let mut p =
            json!({"threadId":scope["threadId"],"server":server,"tool":tool,"arguments":args});
        if metadata {
            p["_meta"] = json!({"x-codex-turn-metadata":scope["metadata"]});
        }
        let value = self.request(conn, "mcpServer/tool/call", p).await?;
        if !value["content"].is_array() {
            return Err(RpcError::message(
                "Upstream did not return a valid MCP tool result",
            ));
        }
        Ok(value)
    }
    async fn mcp_status(&self, conn: &Connection, args: &Value) -> Result<Value, RpcError> {
        let mut p = pick(args, &["serverName", "detail"]);
        if p.get("detail").is_none() {
            p["detail"] = json!("toolsAndAuthOnly");
        }
        match self.request(conn, "mcpServerStatus/list", p).await {
            Err(e)
                if e.unsupported()
                    || e.rpc.as_ref().and_then(|v| v["code"].as_i64()) == Some(-32602) =>
            {
                self.request(conn, "mcpServerStatus/list", pick(args, &["serverName"]))
                    .await
            }
            value => value,
        }
    }
    async fn diagnostics(&self, conn: &Connection) -> Value {
        let utility = self.utility.lock().await;
        let context = utility.as_ref().filter(|u| u.generation == conn.generation);
        json!({"adapter":"rust","cli":self.settings.cli,"workDir":self.settings.work_dir,"appServerRunning":conn.is_alive(),"appServerPid":conn.pid,
            "utilityThreadId":context.map(|u|&u.thread_id),"utilitySessionId":context.map(|u|&u.session_id),"syntheticTurnId":context.map(|u|&u.turn_id),
            "modelTurnsStartedByBridge":0,"stderrTail":conn.stderr.lock().unwrap().iter().rev().take(20).collect::<Vec<_>>()})
    }
    async fn apps(&self, conn: &Connection, name: &str, args: &Value) -> Result<Value, RpcError> {
        let read = name == "codex_apps_read";
        let mut fallback_error = None;
        if args["source"] == "directory" {
            let (method, p) = if read {
                ("app/read", pick(args, &["appIds", "includeTools"]))
            } else {
                ("app/list", pick(args, &["forceRefetch", "limit", "cursor"]))
            };
            match self.request(conn, method, p).await {
                Ok(v) => return Ok(v),
                Err(e) if e.unsupported() => fallback_error = Some(e.message),
                Err(e) => return Err(e),
            }
        }
        let status = self
            .mcp_status(conn, &json!({"detail":"toolsAndAuthOnly"}))
            .await?;
        let server = data(&status)
            .iter()
            .find(|s| s["name"] == "codex_apps")
            .ok_or_else(|| RpcError::message("codex_apps MCP server is not available"))?;
        let mut groups: BTreeMap<String, Value> = BTreeMap::new();
        if let Some(tools) = server["tools"].as_object() {
            for tool in tools.values() {
                let meta = &tool["_meta"];
                let id = meta["connector_id"]
                    .as_str()
                    .or_else(|| {
                        meta.pointer("/_codex_apps/connector_id")
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("unknown");
                let entry=groups.entry(id.into()).or_insert_with(||json!({"id":id,"name":meta.get("connector_name").unwrap_or(&json!(id)),"description":meta.get("connector_description"),"source":"codex_apps_mcp_fallback","tools":[]}));
                entry["tools"].as_array_mut().unwrap().push(json!({"name":tool.get("name"),"title":tool.get("title"),"description":tool.get("description"),"annotations":tool.get("annotations")}));
            }
        }
        let mut items = groups.into_values().collect::<Vec<_>>();
        items.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        for item in &mut items {
            item["toolCount"] = json!(item["tools"].as_array().unwrap().len());
        }
        if read {
            items.retain(|item| {
                args["appIds"]
                    .as_array()
                    .is_some_and(|ids| ids.contains(&item["id"]))
            });
        } else {
            items.truncate(args["limit"].as_u64().unwrap_or(items.len() as u64) as usize);
        }
        if !read || args["includeTools"] != true {
            for item in &mut items {
                item.as_object_mut().unwrap().remove("tools");
            }
        }
        let mut value = json!({"data":items,"source":"runtime"});
        if !read {
            value["nextCursor"] = Value::Null;
        }
        if let Some(error) = fallback_error {
            value["fallback"] = json!(true);
            value["fallbackReason"] = json!(error.chars().take(500).collect::<String>());
        } else if !read {
            value["fallback"] = json!(false);
        }
        Ok(value)
    }
}

fn pick(args: &Value, keys: &[&str]) -> Value {
    Value::Object(
        keys.iter()
            .filter_map(|key| args.get(*key).map(|v| ((*key).to_string(), v.clone())))
            .collect(),
    )
}
fn absolute_path(args: &Value) -> Result<Value, RpcError> {
    let path = args["path"]
        .as_str()
        .filter(|s| s.starts_with('/'))
        .ok_or_else(|| RpcError::message("path must be an absolute macOS path"))?;
    Ok(json!(path))
}
fn data(value: &Value) -> &[Value] {
    value["data"].as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn summarize_mcp(value: &Value) -> Value {
    json!({"data":data(value).iter().map(|s|{
        let tools=s["tools"].as_object().map(|tools|tools.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
        json!({"name":s.get("name"),"authStatus":s.get("authStatus"),"runtimeStatus":s.get("runtimeStatus"),"pluginId":s.get("pluginId"),"httpOrigin":s.get("httpOrigin"),"serverInfo":s.get("serverInfo"),"toolCount":tools.len(),"toolNames":tools.iter().take(40).collect::<Vec<_>>(),"toolsTruncated":tools.len()>40,"toolsError":s.get("toolsError"),"resourceCount":s["resources"].as_array().map_or(0,Vec::len),"resourceTemplateCount":s["resourceTemplates"].as_array().map_or(0,Vec::len)})
    }).collect::<Vec<_>>(),"nextCursor":value.get("nextCursor")})
}
fn summarize_collection(value: &Value) -> Value {
    let arr = if let Some(arr) = value.as_array() {
        Some(arr)
    } else {
        [
            "data", "items", "plugins", "skills", "apps", "projects", "profiles",
        ]
        .iter()
        .find_map(|key| value[*key].as_array())
    };
    match arr {
        Some(arr) => {
            json!({"count":arr.len(),"names":arr.iter().filter_map(|v|["name","id","title","displayName"].iter().find_map(|k|v.get(*k).filter(|v|!v.is_null()))).take(200).collect::<Vec<_>>(),"nextCursor":value.get("nextCursor").or_else(||value.get("next_cursor"))})
        }
        None => value.clone(),
    }
}
