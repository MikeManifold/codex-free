use async_trait::async_trait;
use rmcp::model::ToolAnnotations;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolRequestContext, arg_str};
use crate::types::{AppConfig, ToolResult};

pub struct SwitchProjectRoot;

#[async_trait]
impl Tool for SwitchProjectRoot {
    fn name(&self) -> &'static str {
        "switch_project_root"
    }
    fn description(&self) -> String {
        "Explicitly switch this ChatGPT conversation to another source project only when the human requested that switch. Supply the current source root as expected_project_root and confirm=true. The default set_project_root remains immutable. Refuses while native resident commands are active, serializes against in-flight project calls, backs up the old binding, and leaves project files/worktrees/review checkpoints intact. Finish independent upstream operations first. After success call get_agent_brief for the new project. Also callable through mcp_call_tool with source=local_projects, tool=switch.".into()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(
            ToolAnnotations::new()
                .read_only(false)
                .destructive(true)
                .idempotent(true)
                .open_world(true),
        )
    }
    fn requires_project_root(&self) -> bool {
        false
    }
    fn uses_exec_session_state(&self) -> bool {
        true
    }
    fn input_schema(&self) -> Value {
        json!({
            "type":"object", "additionalProperties":false,
            "required":["path", "expected_project_root", "confirm"],
            "properties": {"path":{"type":"string"},"expected_project_root":{"type":"string","description":"Current source project, beneath the access root."},"confirm":{"type":"boolean","description":"True only after the user explicitly asks to switch this conversation's project."}}
        })
    }
    async fn call(&self, _args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        ToolResult::error("Project switching requires stable ChatGPT conversation metadata.")
    }
    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        let Some(identity) = context.conversation.as_ref() else {
            return self.call(args, config, session).await;
        };
        if args.get("confirm").and_then(Value::as_bool) != Some(true) {
            return ToolResult::error(
                "An explicit user-requested switch and confirm=true are required.",
            );
        }
        let (Some(path), Some(expected)) = (
            arg_str(&args, "path"),
            arg_str(&args, "expected_project_root"),
        ) else {
            return ToolResult::error("path and expected_project_root are required.");
        };
        let _guard = context
            .project_bindings
            .call_lock(identity)
            .write_owned()
            .await;
        if session.exec_session_ids().iter().any(|id| {
            session
                .exec_session(*id)
                .is_some_and(|process| process.exit_code().is_none())
        }) {
            return ToolResult::error(
                "Native commands are still running for this conversation. Finish or stop them before switching projects.",
            );
        }
        match context
            .project_bindings
            .switch_project_root(config, identity, path, expected)
            .await
        {
            Ok(selection) => {
                for id in session.exec_session_ids() {
                    session.remove_exec_session(id);
                }
                *session.plan.lock().unwrap() = None;
                let result = json!({"status":"switched","sourceProjectRoot":selection.source_project_root,"projectRoot":selection.project_root,"managedWorktree":selection.managed_worktree,"warnings":selection.warnings,"next":"Call get_agent_brief before using the new project."});
                ToolResult::text(result.to_string()).with_structured(result)
            }
            Err(error) => ToolResult::error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_bindings::{ConversationIdentity, ProjectBindingStore};
    use std::sync::Arc;

    fn context(
        store: Arc<ProjectBindingStore>,
        identity: ConversationIdentity,
    ) -> ToolRequestContext {
        ToolRequestContext {
            conversation: Some(identity),
            project_bindings: store,
            conversation_authorizations: Arc::new(
                crate::conversation_auth::ConversationAuthorizationStore::new(),
            ),
            review_checkpoints: Arc::new(crate::review::ReviewCheckpointManager::new()),
            artifact_egress: Arc::new(crate::artifact_egress::ArtifactEgressStore::new(
                Default::default(),
            )),
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn explicit_switch_preserves_old_files_other_conversations_and_binding_backup() {
        let temp = tempfile::tempdir().unwrap();
        let access = temp.path().join("access");
        let first = access.join("first");
        let second = access.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("keep.txt"), "original").unwrap();
        let mut config = crate::config::default_config(access);
        config.multi_project = true;
        config.worktrees.mode = crate::types::WorktreeMode::Never;
        let store = Arc::new(ProjectBindingStore::new(temp.path().join("bindings")));
        let a = ConversationIdentity::from_openai_session("fixture-a").unwrap();
        let b = ConversationIdentity::from_openai_session("fixture-b").unwrap();
        store
            .select_project_root(&config, &a, "first")
            .await
            .unwrap();
        store
            .select_project_root(&config, &b, "first")
            .await
            .unwrap();
        assert!(
            store
                .select_project_root(&config, &a, "second")
                .await
                .is_err()
        );
        let context = context(store.clone(), a.clone());
        let session = SessionState::new();
        let run = |args| SwitchProjectRoot.call_with_context(args, &config, &session, &context);
        assert!(
            run(json!({"path":"second","expected_project_root":"first","confirm":false}))
                .await
                .is_error
        );
        assert!(
            run(json!({"path":"second","expected_project_root":"second","confirm":true}))
                .await
                .is_error
        );
        assert!(
            run(json!({"path":"../escape","expected_project_root":"first","confirm":true}))
                .await
                .is_error
        );
        assert_eq!(
            store.effective_config(&config, &a).unwrap().work_dir,
            first.canonicalize().unwrap()
        );
        let result =
            run(json!({"path":"second","expected_project_root":"first","confirm":true})).await;
        assert!(!result.is_error, "{}", result.joined_text());
        assert_eq!(
            store.effective_config(&config, &a).unwrap().work_dir,
            second.canonicalize().unwrap()
        );
        assert_eq!(
            store.effective_config(&config, &b).unwrap().work_dir,
            first.canonicalize().unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(first.join("keep.txt")).unwrap(),
            "original"
        );
        let backups: Vec<_> = walkdir::WalkDir::new(store.base_dir())
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".previous."))
            .collect();
        assert_eq!(backups.len(), 1);
        let previous: Value =
            serde_json::from_slice(&std::fs::read(backups[0].path()).unwrap()).unwrap();
        assert_eq!(
            previous["sourceProjectRoot"],
            first.canonicalize().unwrap().to_string_lossy().as_ref()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn active_resident_commands_block_switching() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let mut config = crate::config::default_config(temp.path().to_path_buf());
        config.multi_project = true;
        config.exec.mode = crate::types::ExecMode::Unrestricted;
        let store = Arc::new(ProjectBindingStore::new(temp.path().join("bindings")));
        let identity = ConversationIdentity::from_openai_session("active-fixture").unwrap();
        store
            .select_project_root(&config, &identity, "first")
            .await
            .unwrap();
        let session = SessionState::new();
        let command = crate::tools::exec_command::ExecCommand
            .call(
                json!({"cmd":"sleep 10","yield_time_ms":250}),
                &config,
                &session,
            )
            .await;
        assert!(!command.is_error, "{}", command.joined_text());
        let result = SwitchProjectRoot
            .call_with_context(
                json!({"path":"second","expected_project_root":"first","confirm":true}),
                &config,
                &session,
                &context(store.clone(), identity.clone()),
            )
            .await;
        assert!(result.is_error);
        assert!(result.joined_text().contains("still running"));
        assert_eq!(
            store.effective_config(&config, &identity).unwrap().work_dir,
            first.canonicalize().unwrap()
        );
    }
}
