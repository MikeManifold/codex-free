use super::*;
use std::path::PathBuf;

#[test]
fn published_tools_keep_legacy_names_and_add_live_schema_selection() {
    let tools = AppServerBridge::tools();
    assert_eq!(tools.len(), 27);
    // Public compatibility contract: upstream MCP tools are discovered separately.
    let expected = [
        "codex_ui_session_end",
        "codex_app_status",
        "codex_exec",
        "codex_fs_read",
        "codex_fs_write",
        "codex_fs_list",
        "codex_mcp_status",
        "codex_mcp_call",
        "codex_mcp_oauth_login",
        "codex_cua_docs",
        "codex_cua_state",
        "codex_cua_js",
        "codex_skills_list",
        "codex_plugins_list",
        "codex_apps_list",
        "codex_apps_read",
        "codex_permissions_list",
        "codex_projects_list",
        "codex_hooks_list",
        "codex_models_list",
        "codex_account_read",
        "codex_remote_control_status",
        "codex_capabilities",
        "codex_rpc",
        "codex_confirmations_list",
        "codex_confirmation_respond",
        "codex_operation_result",
    ];
    let actual = tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(actual, expected.into_iter().collect());
    let status = tools.iter().find(|t| t.name == "codex_mcp_status").unwrap();
    assert!(status.input_schema["properties"].get("toolName").is_some());
}

#[cfg(unix)]
mod subprocess {
    use super::*;
    use crate::{config::default_config, exec_sessions::SessionState, tool::Tool};
    use base64::Engine;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        adapter: Arc<AppServerBridge>,
        _guard: AppServerGuard,
        directory: tempfile::TempDir,
        log: PathBuf,
        mode: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let cli = directory.path().join("fixture-cli");
            let log = directory.path().join("requests.jsonl");
            let mode = directory.path().join("mode.json");
            std::fs::write(&mode, "{}").unwrap();
            std::fs::write(&cli, FIXTURE).unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
            let app = default_config(directory.path().to_path_buf());
            let spec = McpServerSpec {
                transport: Some("codex-app-server".into()),
                env: std::collections::HashMap::from([
                    ("FIXTURE_LOG".into(), log.to_string_lossy().into_owned()),
                    ("FIXTURE_MODE".into(), mode.to_string_lossy().into_owned()),
                ]),
                app_server: Some(AppServerConfig {
                    version: Some(1),
                    codex_cli: Some(cli.to_string_lossy().into_owned()),
                    work_dir: Some(directory.path().to_string_lossy().into_owned()),
                    request_timeout_ms: Some(1000),
                    confirmation_timeout_ms: Some(1000),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let adapter = AppServerBridge::new(&spec, &app).unwrap();
            let guard = adapter.guard();
            Self {
                adapter,
                _guard: guard,
                directory,
                log,
                mode,
            }
        }
        async fn call(&self, name: &str, args: Value, owner: &str) -> ToolResult {
            let mut p = CallToolRequestParams::new(name.to_string())
                .with_arguments(args.as_object().unwrap().clone());
            p.meta = Some(serde_json::from_value(json!({"codex-free/call-owner":owner})).unwrap());
            self.adapter.call(p, None).await
        }
        fn requests(&self) -> Vec<Value> {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
        fn configure(&self, v: Value) {
            std::fs::write(&self.mode, v.to_string()).unwrap();
        }
    }

    #[tokio::test]
    async fn media_and_context_survive_without_a_node_adapter_or_model_turn() {
        let f = Fixture::new();
        for name in [
            "codex_mcp_call",
            "codex_cua_docs",
            "codex_cua_state",
            "codex_cua_js",
        ] {
            let args = match name {
                "codex_mcp_call" => json!({"server":"fixture","tool":"capture"}),
                "codex_cua_js" => json!({"code":"fixture"}),
                _ => json!({}),
            };
            let result = f.call(name, args, "caller-a").await;
            assert!(!result.is_error, "{}", result.joined_text());
            let image = result
                .content
                .iter()
                .find_map(|c| {
                    if let crate::types::ToolContent::Image { data, .. } = c {
                        Some(data)
                    } else {
                        None
                    }
                })
                .unwrap();
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(image)
                    .unwrap()
                    .len(),
                90000
            );
            assert_eq!(result.meta.as_ref().unwrap()["fixture"], true);
        }
        let requests = f.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "initialize")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "thread/start")
                .count(),
            1
        );
        assert!(!requests.iter().any(|r| r["method"] == "turn/start"));
        let init = requests
            .iter()
            .find(|r| r["method"] == "initialize")
            .unwrap();
        assert_eq!(
            init.pointer("/params/capabilities/mcpServerOpenaiFormElicitation"),
            Some(&json!(true))
        );
        let cua = requests
            .iter()
            .filter(|r| {
                r["method"] == "mcpServer/tool/call"
                    && r.pointer("/params/tool") == Some(&json!("js"))
            })
            .collect::<Vec<_>>();
        assert!(
            cua.iter()
                .all(|r| r.pointer("/params/_meta/x-codex-turn-metadata").is_some())
        );
    }

    #[tokio::test]
    async fn runtime_tool_metadata_and_calls_follow_upstream_changes() {
        let f = Fixture::new();
        f.configure(json!({"toolName":"first_tool"}));
        let first = f
            .call(
                "codex_mcp_status",
                json!({"serverName":"fixture","toolName":"first_tool"}),
                "a",
            )
            .await;
        assert!(!first.is_error, "{}", first.joined_text());
        assert_eq!(
            first.structured_content.unwrap().pointer("/tool/name"),
            Some(&json!("first_tool"))
        );
        f.configure(json!({"toolName":"new_tool_after_update"}));
        let second = f
            .call(
                "codex_mcp_status",
                json!({"serverName":"fixture","toolName":"new_tool_after_update"}),
                "a",
            )
            .await;
        assert!(!second.is_error, "{}", second.joined_text());
        assert_eq!(
            second
                .structured_content
                .unwrap()
                .pointer("/tool/inputSchema/properties/live/type"),
            Some(&json!("boolean"))
        );
        let called=f.call("codex_mcp_call",json!({"server":"fixture","tool":"new_tool_after_update","arguments":{"live":true}}),"a").await;
        assert!(!called.is_error);
        assert_eq!(called.structured_content.unwrap()["echo"]["live"], true);
    }

    #[tokio::test]
    async fn confirmations_are_scoped_resume_once_and_hide_private_proof() {
        let f = Fixture::new();
        let pending = f
            .call(
                "codex_mcp_call",
                json!({"server":"fixture","tool":"confirm"}),
                "owner",
            )
            .await;
        let receipt = pending.structured_content.unwrap();
        assert_eq!(receipt["status"], "awaiting_confirmation");
        assert!(receipt["confirmation"].get("challenge").is_none());
        assert!(receipt["confirmation"].get("threadId").is_none());
        let other = f
            .call("codex_confirmations_list", json!({}), "other-owner")
            .await;
        assert_eq!(other.structured_content.unwrap()["operations"], json!([]));
        assert!(
            f.call(
                "codex_operation_result",
                json!({"operationId":receipt["operationId"]}),
                "other-owner"
            )
            .await
            .is_error
        );
        let args = json!({"operationId":receipt["operationId"],"confirmationId":receipt["confirmation"]["id"],"action":"accept","userConfirmed":true,"content":{"allow":true}});
        let mut missing_human = args.clone();
        missing_human
            .as_object_mut()
            .unwrap()
            .remove("userConfirmed");
        assert!(
            f.call("codex_confirmation_respond", missing_human, "owner")
                .await
                .is_error
        );
        let mut invalid = args.clone();
        invalid["content"] = json!({});
        assert!(
            f.call("codex_confirmation_respond", invalid, "owner")
                .await
                .is_error
        );
        assert!(
            f.call("codex_confirmation_respond", args.clone(), "other-owner")
                .await
                .is_error
        );
        let accepted = f
            .call("codex_confirmation_respond", args.clone(), "owner")
            .await;
        assert!(!accepted.is_error, "{}", accepted.joined_text());
        assert_eq!(accepted.structured_content.unwrap()["accepted"], true);
        assert!(
            f.call("codex_confirmation_respond", args, "owner")
                .await
                .is_error
        );
        let calls = f
            .requests()
            .into_iter()
            .filter(|r| r.pointer("/params/tool") == Some(&json!("confirm")))
            .count();
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn decline_timeout_and_device_verification_do_not_become_approval() {
        let f = Fixture::new();
        for (tool, action) in [("confirm", "decline"), ("device", "cancel")] {
            let pending = f
                .call(
                    "codex_mcp_call",
                    json!({"server":"fixture","tool":tool}),
                    "owner",
                )
                .await;
            let v = pending.structured_content.unwrap();
            if tool == "device" {
                let wrong=f.call("codex_confirmation_respond",json!({"operationId":v["operationId"],"confirmationId":v["confirmation"]["id"],"action":"accept","userConfirmed":true,"content":{"allow":true}}),"owner").await;
                assert!(wrong.is_error);
                assert!(wrong.joined_text().contains("Device verification"));
            }
            let done=f.call("codex_confirmation_respond",json!({"operationId":v["operationId"],"confirmationId":v["confirmation"]["id"],"action":action,"userConfirmed":true}),"owner").await;
            assert_eq!(done.structured_content.unwrap()["accepted"], false);
        }
        let pending = f
            .call(
                "codex_mcp_call",
                json!({"server":"fixture","tool":"confirm"}),
                "owner",
            )
            .await;
        let v = pending.structured_content.unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let done = f
            .call(
                "codex_operation_result",
                json!({"operationId":v["operationId"],"waitMs":1000}),
                "owner",
            )
            .await;
        assert_eq!(done.structured_content.unwrap()["accepted"], false);
    }

    #[tokio::test]
    async fn exited_server_rebuilds_only_on_next_call_without_replaying_mutation() {
        let f = Fixture::new();
        let dead = f
            .call(
                "codex_mcp_call",
                json!({"server":"fixture","tool":"die"}),
                "owner",
            )
            .await;
        assert!(dead.is_error);
        let status = f.call("codex_mcp_status", json!({}), "owner").await;
        assert!(!status.is_error, "{}", status.joined_text());
        let requests = f.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "initialize")
                .count(),
            2
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.pointer("/params/tool") == Some(&json!("die")))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn rpc_timeout_is_not_replayed_and_independent_rescue_still_works() {
        let f = Fixture::new();
        let stalled = f
            .call(
                "codex_mcp_call",
                json!({"server":"fixture","tool":"stall"}),
                "owner",
            )
            .await;
        assert!(stalled.is_error);
        assert!(stalled.joined_text().contains("timed out"));
        let app = default_config(f.directory.path().to_path_buf());
        let rescue = crate::tools::exec_command::ExecCommand
            .call(
                json!({"cmd":"echo RESCUE_OK","yield_time_ms":1000}),
                &app,
                &SessionState::new(),
            )
            .await;
        assert!(!rescue.is_error, "{}", rescue.joined_text());
        assert!(
            rescue.structured_content.unwrap()["output"]
                .as_str()
                .unwrap()
                .contains("RESCUE_OK")
        );
        let requests = f.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.pointer("/params/tool") == Some(&json!("stall")))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn queued_cancellation_never_executes_after_another_call_is_confirmed() {
        let f = Fixture::new();
        let pending = f
            .call(
                "codex_mcp_call",
                json!({"server":"fixture","tool":"confirm"}),
                "owner",
            )
            .await
            .structured_content
            .unwrap();
        let cancellation = CancellationToken::new();
        let mut p = CallToolRequestParams::new("codex_mcp_call").with_arguments(
            serde_json::from_value(json!({"server":"fixture","tool":"must_not_execute"})).unwrap(),
        );
        p.meta = Some(serde_json::from_value(json!({"codex-free/call-owner":"owner"})).unwrap());
        let request = f.adapter.call(p, Some(&cancellation));
        tokio::pin!(request);
        // The second request has entered the serialized queue while the form is open.
        tokio::select! { _=&mut request=>panic!("queued request completed before cancellation"), _=tokio::time::sleep(Duration::from_millis(30))=>{} }
        cancellation.cancel();
        assert!(request.await.is_error);
        let done=f.call("codex_confirmation_respond",json!({"operationId":pending["operationId"],"confirmationId":pending["confirmation"]["id"],"action":"decline","userConfirmed":true}),"owner").await;
        assert_eq!(done.structured_content.unwrap()["accepted"], false);
        assert!(
            !f.call("codex_mcp_status", json!({}), "owner")
                .await
                .is_error
        );
        assert!(
            !f.requests()
                .iter()
                .any(|r| r.pointer("/params/tool") == Some(&json!("must_not_execute")))
        );
    }

    #[tokio::test]
    async fn ordinary_calls_release_capacity_and_validate_generic_arguments() {
        let f = Fixture::new();
        for _ in 0..70 {
            let result = f
                .call(
                    "codex_mcp_call",
                    json!({"server":"fixture","tool":"read","arguments":null}),
                    "owner",
                )
                .await;
            assert!(!result.is_error, "{}", result.joined_text());
            assert_eq!(result.structured_content.unwrap()["echo"], json!({}));
        }
        assert!(
            f.call(
                "codex_mcp_call",
                json!({"server":"fixture","tool":"invalid","arguments":[]}),
                "owner"
            )
            .await
            .is_error
        );
        assert!(
            !f.requests()
                .iter()
                .any(|r| r.pointer("/params/tool") == Some(&json!("invalid")))
        );
    }

    #[tokio::test]
    #[ignore = "requires the installed official CLI and an existing private configuration"]
    async fn installed_app_server_read_only_smoke() {
        let config = std::env::var("CODEX_APP_SERVER_TEST_CONFIG")
            .expect("set CODEX_APP_SERVER_TEST_CONFIG to an existing private configuration");
        let app = default_config(std::env::current_dir().unwrap());
        let spec = McpServerSpec {
            transport: Some("codex-app-server".into()),
            app_server: Some(AppServerConfig {
                config_file: Some(config),
                ..Default::default()
            }),
            ..Default::default()
        };
        let adapter = AppServerBridge::new(&spec, &app).unwrap();
        let guard = adapter.guard();
        let status = adapter
            .call(CallToolRequestParams::new("codex_app_status"), None)
            .await;
        assert!(!status.is_error, "App Server status failed");
        let status = status.structured_content.unwrap();
        assert_eq!(status["bridge"]["adapter"], "rust");
        assert_eq!(status["bridge"]["modelTurnsStartedByBridge"], 0);
        assert!(
            status["mcp"]["data"].is_array(),
            "MCP directory read failed"
        );
        let pid = status["bridge"]["appServerPid"].as_u64().unwrap() as i32;
        drop(guard);
        for _ in 0..60 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("owned App Server did not shut down");
    }

    #[tokio::test]
    async fn cancellation_kills_only_the_owned_fixture_and_can_reconnect() {
        let f = Fixture::new();
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let log = f.log.clone();
        tokio::spawn(async move {
            for _ in 0..100 {
                if std::fs::read_to_string(&log)
                    .unwrap_or_default()
                    .contains("\"tool\": \"stall\"")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            trigger.cancel();
        });
        let mut p = CallToolRequestParams::new("codex_mcp_call").with_arguments(
            serde_json::from_value(json!({"server":"fixture","tool":"stall"})).unwrap(),
        );
        p.meta = Some(serde_json::from_value(json!({"codex-free/call-owner":"owner"})).unwrap());
        let result = f.adapter.call(p, Some(&cancel)).await;
        assert!(result.is_error);
        let status = f.call("codex_mcp_status", json!({}), "owner").await;
        assert!(!status.is_error, "{}", status.joined_text());
        assert_eq!(
            f.requests()
                .iter()
                .filter(|r| r["method"] == "initialize")
                .count(),
            2
        );
    }

    #[tokio::test]
    #[ignore = "requires the installed official CLI and an existing private configuration"]
    async fn installed_runtime_isolates_callers_and_preserves_other_session_on_end() {
        let config =
            std::env::var("CODEX_APP_SERVER_TEST_CONFIG").expect("private configuration required");
        let app = default_config(std::env::current_dir().unwrap());
        let spec = McpServerSpec {
            app_server: Some(AppServerConfig {
                config_file: Some(config),
                ..Default::default()
            }),
            ..Default::default()
        };
        let adapter = AppServerBridge::new(&spec, &app).unwrap();
        let _guard = adapter.guard();
        async fn owned(
            adapter: &Arc<AppServerBridge>,
            owner: &str,
            tool: &str,
            args: Value,
        ) -> ToolResult {
            let mut params = CallToolRequestParams::new(tool.to_string())
                .with_arguments(args.as_object().unwrap().clone());
            params.meta =
                Some(serde_json::from_value(json!({"codex-free/call-owner":owner})).unwrap());
            adapter.call(params, None).await
        }
        let a=owned(&adapter,"acceptance-a","codex_mcp_call",json!({"server":"node_repl","tool":"js","arguments":{"code":"var freeOwnerProbe = 'A'; nodeRepl.write({stored:true});"}})).await;
        assert!(!a.is_error, "{}", a.joined_text());
        let b=owned(&adapter,"acceptance-b","codex_mcp_call",json!({"server":"node_repl","tool":"js","arguments":{"code":"nodeRepl.write({isolated: typeof freeOwnerProbe === 'undefined'});"}})).await;
        assert!(!b.is_error, "{}", b.joined_text());
        assert!(b.joined_text().contains("isolated: true"));
        let a=owned(&adapter,"acceptance-a","codex_mcp_call",json!({"server":"node_repl","tool":"js","arguments":{"code":"nodeRepl.write({sameOwner: freeOwnerProbe === 'A'});"}})).await;
        assert!(!a.is_error, "{}", a.joined_text());
        assert!(a.joined_text().contains("sameOwner: true"));
        let ended = owned(&adapter, "acceptance-a", "codex_ui_session_end", json!({})).await;
        assert!(!ended.is_error, "{}", ended.joined_text());
        assert_eq!(ended.structured_content.unwrap()["warnings"], json!([]));
        let b=owned(&adapter,"acceptance-b","codex_mcp_call",json!({"server":"node_repl","tool":"js","arguments":{"code":"nodeRepl.write({stillAlive:true});"}})).await;
        assert!(!b.is_error, "{}", b.joined_text());
        assert!(b.joined_text().contains("stillAlive: true"));
        let a=owned(&adapter,"acceptance-a","codex_mcp_call",json!({"server":"node_repl","tool":"js","arguments":{"code":"nodeRepl.write({freshState: typeof freeOwnerProbe === 'undefined'});"}})).await;
        assert!(!a.is_error, "{}", a.joined_text());
        assert!(a.joined_text().contains("freshState: true"));
        for owner in ["acceptance-a", "acceptance-b"] {
            assert!(
                !owned(&adapter, owner, "codex_ui_session_end", json!({}))
                    .await
                    .is_error
            );
        }
    }

    #[tokio::test]
    async fn directory_fallback_preserves_access_denials_and_human_forms() {
        let f = Fixture::new();
        let fallback = f
            .call("codex_apps_list", json!({"source":"directory"}), "a")
            .await;
        assert_eq!(fallback.structured_content.unwrap()["fallback"], true);
        f.configure(json!({"directoryDenied":true}));
        let denied = f
            .call("codex_apps_list", json!({"source":"directory"}), "a")
            .await;
        assert!(denied.is_error);
        assert!(denied.joined_text().contains("access denied"));
        f.configure(json!({"directoryConfirm":true}));
        let pending = f
            .call("codex_apps_list", json!({"source":"directory"}), "a")
            .await;
        let v = pending.structured_content.unwrap();
        assert_eq!(v["status"], "awaiting_confirmation");
        let done=f.call("codex_confirmation_respond",json!({"operationId":v["operationId"],"confirmationId":v["confirmation"]["id"],"action":"accept","userConfirmed":true,"content":{"allow":true}}),"a").await;
        assert!(!done.is_error);
        assert_eq!(done.structured_content.unwrap()["data"], json!([]));
    }

    #[tokio::test]
    async fn native_catalog_is_lazy_filters_tools_and_keeps_rescue_discoverable() {
        let f = Fixture::new();
        let mut app = default_config(f.directory.path().to_path_buf());
        app.mcp_servers.insert(
            "codex_app_server".into(),
            McpServerSpec {
                transport: Some("codex-app-server".into()),
                mode: Some(crate::types::McpToolExposure::Catalog),
                tools: Some(vec!["codex_app_status".into()]),
                ..Default::default()
            },
        );
        for name in ["cua_repl", "node_repl"] {
            app.mcp_servers.insert(
                name.into(),
                McpServerSpec {
                    command: Some("duplicate-runtime-must-not-start".into()),
                    ..Default::default()
                },
            );
        }
        let bridged = crate::bridge::connect_upstreams(&app).await;
        assert!(bridged.services.is_empty());
        assert_eq!(
            bridged
                .report
                .iter()
                .filter(|line| line.contains("duplicate direct runtime not started"))
                .count(),
            2
        );
        assert_eq!(bridged.app_servers.len(), 1);
        let sources = bridged
            .tools
            .iter()
            .find(|t| t.name() == "mcp_list_sources")
            .unwrap()
            .call(json!({}), &app, &SessionState::new())
            .await;
        assert_eq!(
            sources.structured_content.unwrap()["sources"][0]["transport"],
            "app-server-rpc"
        );
        let rescue = bridged
            .tools
            .iter()
            .find(|t| t.name() == "mcp_get_tool")
            .unwrap()
            .call(
                json!({"source":"local_shell","tool":"exec"}),
                &app,
                &SessionState::new(),
            )
            .await;
        assert!(!rescue.is_error);
        assert!(!f.log.exists());
    }

    #[tokio::test]
    async fn multi_call_contexts_are_isolated_and_explicit_end_affects_only_owner() {
        let f = Fixture::new();
        let a = f.call("codex_cua_js", json!({"code":"a"}), "a").await;
        let b = f.call("codex_cua_js", json!({"code":"b"}), "b").await;
        assert!(!a.is_error && !b.is_error);
        let before = f.requests();
        let js = before
            .iter()
            .filter(|r| r["params"]["tool"] == "js")
            .collect::<Vec<_>>();
        assert_ne!(js[0]["params"]["threadId"], js[1]["params"]["threadId"]);
        assert_ne!(js[0]["params"]["_meta"], js[1]["params"]["_meta"]);
        let ended = f.call("codex_ui_session_end", json!({}), "a").await;
        assert_eq!(ended.structured_content.unwrap()["ended"], true);
        let twice = f.call("codex_ui_session_end", json!({}), "a").await;
        assert_eq!(twice.structured_content.unwrap()["alreadyEnded"], true);
        assert!(
            !f.call("codex_cua_js", json!({"code":"b again"}), "b")
                .await
                .is_error
        );
        let requests = f.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "initialize")
                .count(),
            2
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "thread/start")
                .count(),
            2
        );
        let end = requests
            .iter()
            .find(|r| r["params"]["tool"] == "turn_ended")
            .unwrap();
        assert_eq!(end["params"]["threadId"], js[0]["params"]["threadId"]);
        assert_eq!(end["params"]["_meta"], js[0]["params"]["_meta"]);
        assert!(
            !f.call("codex_cua_js", json!({"code":"new a"}), "a")
                .await
                .is_error
        );
        assert_eq!(
            f.requests()
                .iter()
                .filter(|r| r["method"] == "initialize")
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn idle_deadline_is_five_minutes_after_completion_and_is_renewed() {
        let f = Fixture::new();
        let started = tokio::time::Instant::now();
        assert!(
            !f.call("codex_cua_js", json!({"code":"first"}), "a")
                .await
                .is_error
        );
        f.adapter
            .reap_idle(started + lifecycle::IDLE_TIMEOUT, lifecycle::IDLE_TIMEOUT)
            .await;
        assert!(
            !f.requests()
                .iter()
                .any(|r| r["params"]["tool"] == "turn_ended")
        );
        assert!(
            !f.call("codex_cua_js", json!({"code":"second"}), "a")
                .await
                .is_error
        );
        assert_eq!(
            f.requests()
                .iter()
                .filter(|r| r["method"] == "thread/start")
                .count(),
            1
        );
        f.adapter
            .reap_idle(
                tokio::time::Instant::now() + Duration::from_secs(301),
                lifecycle::IDLE_TIMEOUT,
            )
            .await;
        assert_eq!(
            f.requests()
                .iter()
                .filter(|r| r["params"]["tool"] == "turn_ended")
                .count(),
            1
        );
        assert_eq!(
            f.requests()
                .iter()
                .filter(|r| r["method"] == "thread/unsubscribe")
                .count(),
            1
        );
        assert!(
            !f.call("codex_cua_js", json!({"code":"fresh"}), "a")
                .await
                .is_error
        );
        assert_eq!(
            f.requests()
                .iter()
                .filter(|r| r["method"] == "thread/start")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn pending_confirmation_prevents_idle_reclaim_and_end_cancels_without_replay() {
        let f = Fixture::new();
        let pending = f
            .call(
                "codex_mcp_call",
                json!({"server":"fixture","tool":"confirm"}),
                "a",
            )
            .await;
        let receipt = pending.structured_content.unwrap();
        f.adapter
            .reap_idle(
                tokio::time::Instant::now() + Duration::from_secs(301),
                lifecycle::IDLE_TIMEOUT,
            )
            .await;
        assert!(
            !f.requests()
                .iter()
                .any(|r| r["method"] == "thread/unsubscribe")
        );
        assert!(!f.call("codex_mcp_status", json!({}), "b").await.is_error);
        assert!(
            !f.call("codex_ui_session_end", json!({}), "a")
                .await
                .is_error
        );
        assert!(
            f.call(
                "codex_operation_result",
                json!({"operationId":receipt["operationId"]}),
                "a"
            )
            .await
            .is_error
        );
        assert_eq!(
            f.requests()
                .iter()
                .filter(|r| r["params"]["tool"] == "confirm")
                .count(),
            1
        );
        assert!(!f.call("codex_mcp_status", json!({}), "b").await.is_error);
        assert_eq!(
            f.requests()
                .iter()
                .filter(|r| r["method"] == "initialize")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn failed_cleanup_is_bounded_by_stopping_only_owned_runtime() {
        let f = Fixture::new();
        assert!(
            !f.call("codex_cua_js", json!({"code":"use"}), "a")
                .await
                .is_error
        );
        f.configure(json!({"cleanupError":true}));
        let ended = f.call("codex_ui_session_end", json!({}), "a").await;
        assert!(!ended.is_error);
        assert_eq!(
            ended.structured_content.unwrap()["warnings"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            !f.call("codex_cua_js", json!({"code":"fresh"}), "a")
                .await
                .is_error
        );
        assert_eq!(
            f.requests()
                .iter()
                .filter(|r| r["method"] == "initialize")
                .count(),
            2
        );
    }

    const FIXTURE: &str = r##"#!/usr/bin/env python3
import sys,json,os,time,base64
log=os.environ['FIXTURE_LOG'];modefile=os.environ['FIXTURE_MODE'];pending={};formid=10000;threads=0
def send(v):print(json.dumps(v),flush=True)
def reply(i,v):send({'id':i,'result':v})
def result(v):return {'content':[{'type':'text','text':json.dumps(v)}],'structuredContent':v}
def form(i,mode='form',directory=False):
 global formid
 formid+=1;pending[formid]=(i,directory)
 send({'id':formid,'method':'mcpServer/elicitation/request','params':{'mode':mode,'threadId':'private-thread','challenge':'private-proof','message':'Fixture confirmation','requestedSchema':{'type':'object','properties':{'allow':{'type':'boolean'}},'required':['allow'],'additionalProperties':False}}})
for line in sys.stdin:
 try:m=json.loads(line)
 except:continue
 with open(log,'a') as f:f.write(json.dumps(m)+'\n')
 i=m.get('id');method=m.get('method');p=m.get('params') or {};mode=json.load(open(modefile))
 if method is None:
  original,directory=pending.pop(i);approved=m.get('result',{}).get('action')=='accept'
  reply(original,{'data':[]} if directory else result({'accepted':approved}));continue
 if i is None:continue
 if method=='initialize':reply(i,{})
 elif method=='thread/start':
  threads+=1;reply(i,{'thread':{'id':f'fixture-{os.getpid()}-{threads}','sessionId':f'session-{os.getpid()}'}})
 elif method=='thread/unsubscribe':reply(i,{'status':'unsubscribed'})
 elif method=='mcpServerStatus/list':
  name=mode.get('toolName','first_tool')
  reply(i,{'data':[{'name':'cua_repl','tools':{'js':{'name':'js'}}},{'name':'fixture','tools':{name:{'name':name,'description':'Runtime-discovered fixture tool','inputSchema':{'type':'object','properties':{'live':{'type':'boolean'}}}}}},{'name':'codex_apps','tools':{'demo':{'name':'demo','_meta':{'connector_id':'fixture-app','connector_name':'Fixture App'}}}}]})
 elif method=='mcpServer/tool/call':
  tool=p.get('tool')
  if tool=='die':os._exit(17)
  elif tool=='turn_ended' and mode.get('cleanupError'):send({'id':i,'error':{'code':-32000,'message':'Fixture cleanup failed'}})
  elif tool=='stall':time.sleep(20)
  elif tool in ['confirm','device']:form(i,'openai/userVerification' if tool=='device' else 'form')
  elif tool in ['capture','js']:reply(i,{'content':[{'type':'image','mimeType':'image/png','data':base64.b64encode(b'x'*90000).decode()}],'_meta':{'fixture':True},'isError':False})
  else:reply(i,result({'echo':p.get('arguments',{})}))
 elif method in ['app/list','app/read']:
  if mode.get('directoryDenied'):send({'id':i,'error':{'code':-32000,'message':'App access denied'}})
  elif mode.get('directoryConfirm'):form(i,directory=True)
  else:send({'id':i,'error':{'code':-32601,'message':'Unknown method'}})
 else:send({'id':i,'error':{'code':-32601,'message':'Unexpected or model-turn RPC: '+method}})
"##;
}
