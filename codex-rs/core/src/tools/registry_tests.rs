use super::*;
use crate::session::step_context::StepContext;
use codex_hooks::Hooks;
use codex_hooks::HooksConfig;
use pretty_assertions::assert_eq;

struct TestHandler {
    tool_name: codex_tools::ToolName,
}

impl ToolExecutor<ToolInvocation> for TestHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        test_spec(&self.tool_name)
    }

    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async {
            Ok(
                Box::new(crate::tools::context::FunctionToolOutput::from_text(
                    "ok".to_string(),
                    Some(true),
                )) as Box<dyn crate::tools::context::ToolOutput>,
            )
        })
    }
}

impl CoreToolRuntime for TestHandler {}

#[derive(Clone)]
enum LifecycleTestResult {
    Ok { success: bool },
    Err,
}

struct LifecycleTestHandler {
    tool_name: codex_tools::ToolName,
    result: LifecycleTestResult,
}

struct ModelBoundedCodeModeOutput {
    model_output: crate::tools::context::FunctionToolOutput,
    code_mode_value: serde_json::Value,
}

impl crate::tools::context::ToolOutput for ModelBoundedCodeModeOutput {
    fn log_preview(&self) -> String {
        self.model_output.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        self.model_output.success_for_logging()
    }

    fn code_mode_execution_status(&self) -> codex_tools::ToolExecutionStatus {
        codex_tools::ToolExecutionStatus::Completed
    }

    fn to_response_item(
        &self,
        call_id: &str,
        payload: &ToolPayload,
    ) -> codex_protocol::models::ResponseInputItem {
        self.model_output.to_response_item(call_id, payload)
    }

    fn post_tool_use_response(
        &self,
        _call_id: &str,
        _payload: &ToolPayload,
    ) -> Option<serde_json::Value> {
        Some(serde_json::json!({ "projection": "bounded" }))
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> serde_json::Value {
        self.code_mode_value.clone()
    }
}

struct ModelBoundedCodeModeHandler {
    tool_name: codex_tools::ToolName,
}

impl ToolExecutor<ToolInvocation> for ModelBoundedCodeModeHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        test_spec(&self.tool_name)
    }

    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async {
            Ok(Box::new(ModelBoundedCodeModeOutput {
                model_output: crate::tools::context::FunctionToolOutput::from_text(
                    "{\"error\":\"bounded\"}".to_string(),
                    Some(false),
                ),
                code_mode_value: serde_json::json!({ "complete": "resource value" }),
            }) as Box<dyn crate::tools::context::ToolOutput>)
        })
    }
}

impl CoreToolRuntime for ModelBoundedCodeModeHandler {}

impl ToolExecutor<ToolInvocation> for LifecycleTestHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        test_spec(&self.tool_name)
    }

    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call())
    }
}

impl LifecycleTestHandler {
    async fn handle_call(
        &self,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        match self.result.clone() {
            LifecycleTestResult::Ok { success } => Ok(Box::new(
                crate::tools::context::FunctionToolOutput::from_text(
                    "ok".to_string(),
                    Some(success),
                ),
            )
                as Box<dyn crate::tools::context::ToolOutput>),
            LifecycleTestResult::Err => Err(FunctionCallError::RespondToModel(
                "handler failed".to_string(),
            )),
        }
    }
}

impl CoreToolRuntime for LifecycleTestHandler {}

fn test_spec(tool_name: &codex_tools::ToolName) -> codex_tools::ToolSpec {
    codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
        name: tool_name.name.clone(),
        description: "Test tool.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: codex_tools::JsonSchema::default(),
        output_schema: None,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum RecordedToolLifecycle {
    Start {
        call_id: String,
        tool_name: codex_tools::ToolName,
    },
    Finish {
        call_id: String,
        tool_name: codex_tools::ToolName,
        outcome: codex_extension_api::ToolCallOutcome,
    },
}

struct ToolLifecycleRecorder {
    records: Arc<std::sync::Mutex<Vec<RecordedToolLifecycle>>>,
}

impl codex_extension_api::ToolLifecycleContributor for ToolLifecycleRecorder {
    fn on_tool_start<'a>(
        &'a self,
        input: codex_extension_api::ToolStartInput<'a>,
    ) -> codex_extension_api::ToolLifecycleFuture<'a> {
        let records = Arc::clone(&self.records);
        let record = RecordedToolLifecycle::Start {
            call_id: input.call_id.to_string(),
            tool_name: input.tool_name.clone(),
        };
        Box::pin(async move {
            records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(record);
        })
    }

    fn on_tool_finish<'a>(
        &'a self,
        input: codex_extension_api::ToolFinishInput<'a>,
    ) -> codex_extension_api::ToolLifecycleFuture<'a> {
        let records = Arc::clone(&self.records);
        let record = RecordedToolLifecycle::Finish {
            call_id: input.call_id.to_string(),
            tool_name: input.tool_name.clone(),
            outcome: input.outcome,
        };
        Box::pin(async move {
            records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(record);
        })
    }
}

#[test]
fn handler_looks_up_namespaced_aliases_explicitly() {
    let namespace = "mcp__codex_apps__gmail";
    let tool_name = "gmail_get_recent_emails";
    let plain_name = codex_tools::ToolName::plain(tool_name);
    let namespaced_name = codex_tools::ToolName::namespaced(namespace, tool_name);
    let plain_handler = Arc::new(TestHandler {
        tool_name: plain_name.clone(),
    }) as Arc<dyn CoreToolRuntime>;
    let namespaced_handler = Arc::new(TestHandler {
        tool_name: namespaced_name.clone(),
    }) as Arc<dyn CoreToolRuntime>;
    let registry = ToolRegistry::new(HashMap::from([
        (plain_name.clone(), Arc::clone(&plain_handler)),
        (namespaced_name.clone(), Arc::clone(&namespaced_handler)),
    ]));

    let plain = registry.tool(&plain_name);
    let namespaced = registry.tool(&namespaced_name);
    let missing_namespaced = registry.tool(&codex_tools::ToolName::namespaced(
        "mcp__codex_apps__calendar",
        tool_name,
    ));

    assert_eq!(plain.is_some(), true);
    assert_eq!(namespaced.is_some(), true);
    assert_eq!(missing_namespaced.is_none(), true);
    assert!(
        plain
            .as_ref()
            .is_some_and(|handler| Arc::ptr_eq(handler, &plain_handler))
    );
    assert!(
        namespaced
            .as_ref()
            .is_some_and(|handler| Arc::ptr_eq(handler, &namespaced_handler))
    );
}

#[tokio::test]
async fn function_tools_expose_default_hook_payloads_and_rewrites() -> anyhow::Result<()> {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let tool_name = codex_tools::ToolName::namespaced("functions.", "echo");
    let handler = TestHandler {
        tool_name: tool_name.clone(),
    };
    let invocation = ToolInvocation {
        payload: ToolPayload::Function {
            arguments: serde_json::json!({ "message": "hello" }).to_string(),
        },
        ..test_invocation(Arc::new(session), Arc::new(turn), "call-1", tool_name)
    };
    let output =
        crate::tools::context::FunctionToolOutput::from_text("echoed".to_string(), Some(true));

    assert_eq!(
        handler.pre_tool_use_payload(&invocation),
        Some(PreToolUsePayload {
            tool_name: HookToolName::new("functions.echo"),
            tool_input: serde_json::json!({ "message": "hello" }),
        })
    );
    assert_eq!(
        handler.post_tool_use_payload(&invocation, &output),
        Some(PostToolUsePayload {
            tool_name: HookToolName::new("functions.echo"),
            tool_use_id: "call-1".to_string(),
            tool_input: serde_json::json!({ "message": "hello" }),
            tool_response: serde_json::json!("echoed"),
        })
    );

    let invocation = handler
        .with_updated_hook_input(invocation, serde_json::json!({ "message": "rewritten" }))?;
    let ToolPayload::Function { arguments } = invocation.payload else {
        panic!("generic rewritten function payload should remain function-shaped");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&arguments)?,
        serde_json::json!({ "message": "rewritten" })
    );

    Ok(())
}

#[tokio::test]
async fn function_hook_input_defaults_empty_arguments_to_object() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let tool_name = codex_tools::ToolName::plain("echo");
    let handler = TestHandler {
        tool_name: tool_name.clone(),
    };
    let invocation = ToolInvocation {
        payload: ToolPayload::Function {
            arguments: "  ".to_string(),
        },
        ..test_invocation(Arc::new(session), Arc::new(turn), "call-1", tool_name)
    };

    assert_eq!(
        handler.pre_tool_use_payload(&invocation),
        Some(PreToolUsePayload {
            tool_name: HookToolName::new("echo"),
            tool_input: serde_json::json!({}),
        })
    );
}

#[tokio::test]
async fn spawn_agent_function_tools_use_agent_matcher_alias() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let hook_payloads = [
        codex_tools::ToolName::plain("spawn_agent"),
        codex_tools::ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "spawn_agent"),
    ]
    .into_iter()
    .map(|tool_name| {
        let handler = TestHandler {
            tool_name: tool_name.clone(),
        };
        let invocation = ToolInvocation {
            payload: ToolPayload::Function {
                arguments: serde_json::json!({ "message": "inspect this repo" }).to_string(),
            },
            ..test_invocation(Arc::clone(&session), Arc::clone(&turn), "call-1", tool_name)
        };
        handler.pre_tool_use_payload(&invocation)
    })
    .collect::<Vec<_>>();

    assert_eq!(
        hook_payloads,
        vec![
            Some(PreToolUsePayload {
                tool_name: HookToolName::spawn_agent(),
                tool_input: serde_json::json!({ "message": "inspect this repo" }),
            }),
            Some(PreToolUsePayload {
                tool_name: HookToolName::spawn_agent(),
                tool_input: serde_json::json!({ "message": "inspect this repo" }),
            }),
        ]
    );
}

#[tokio::test]
async fn code_mode_wait_does_not_expose_default_hook_payloads() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let output = crate::tools::context::FunctionToolOutput::from_text("ok".to_string(), Some(true));

    let wait = crate::tools::handlers::CodeModeWaitHandler;
    let wait_invocation = test_invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait-call",
        wait.tool_name(),
    );
    assert_eq!(wait.pre_tool_use_payload(&wait_invocation), None);
    assert_eq!(wait.post_tool_use_payload(&wait_invocation, &output), None);
}

#[tokio::test]
async fn write_stdin_does_not_expose_default_pre_tool_use_payload() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;

    let write_stdin = crate::tools::handlers::WriteStdinHandler;
    let invocation = test_invocation(
        Arc::new(session),
        Arc::new(turn),
        "write-stdin-call",
        write_stdin.tool_name(),
    );

    assert_eq!(write_stdin.pre_tool_use_payload(&invocation), None);
}

#[test]
fn post_tool_use_feedback_output_keeps_code_mode_result_typed() {
    let result = AnyToolResult {
        call_id: "call-1".to_string(),
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
        result: Box::new(PostToolUseFeedbackOutput {
            original: Box::new(codex_tools::JsonToolOutput::new(
                serde_json::json!({ "typed": true }),
            )),
            model_visible: crate::tools::context::FunctionToolOutput::from_text(
                "hook feedback".to_string(),
                /*success*/ None,
            ),
        }),
        post_tool_use_payload: None,
    };

    assert_eq!(
        result.into_response(),
        ResponseInputItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                "hook feedback".to_string()
            ),
        }
    );

    let result = AnyToolResult {
        call_id: "call-1".to_string(),
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
        result: Box::new(PostToolUseFeedbackOutput {
            original: Box::new(codex_tools::JsonToolOutput::new(
                serde_json::json!({ "typed": true }),
            )),
            model_visible: crate::tools::context::FunctionToolOutput::from_text(
                "hook feedback".to_string(),
                /*success*/ None,
            ),
        }),
        post_tool_use_payload: None,
    };

    assert_eq!(
        result.code_mode_result(),
        serde_json::json!({ "typed": true })
    );
}

#[tokio::test]
async fn dispatch_notifies_tool_lifecycle_contributors() -> anyhow::Result<()> {
    let (mut session, turn) = crate::session::tests::make_session_and_context().await;
    let records = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    builder.tool_lifecycle_contributor(Arc::new(ToolLifecycleRecorder {
        records: Arc::clone(&records),
    }));
    session.services.extensions = Arc::new(builder.build());

    let ok_tool = codex_tools::ToolName::plain("ok_tool");
    let failing_tool = codex_tools::ToolName::plain("failing_tool");
    let ok_handler = Arc::new(LifecycleTestHandler {
        tool_name: ok_tool.clone(),
        result: LifecycleTestResult::Ok { success: false },
    }) as Arc<dyn CoreToolRuntime>;
    let failing_handler = Arc::new(LifecycleTestHandler {
        tool_name: failing_tool.clone(),
        result: LifecycleTestResult::Err,
    }) as Arc<dyn CoreToolRuntime>;
    let registry = ToolRegistry::new(HashMap::from([
        (ok_tool.clone(), ok_handler),
        (failing_tool.clone(), failing_handler),
    ]));
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    registry
        .dispatch_any_with_terminal_outcome(
            test_invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                "ok-call",
                ok_tool.clone(),
            ),
            /*terminal_outcome_reached*/ None,
        )
        .await?;
    let err = match registry
        .dispatch_any_with_terminal_outcome(
            test_invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                "failing-call",
                failing_tool.clone(),
            ),
            /*terminal_outcome_reached*/ None,
        )
        .await
    {
        Ok(_) => panic!("failing handler should return an error"),
        Err(err) => err,
    };
    assert_eq!(err.to_string(), "handler failed");

    let expected = vec![
        RecordedToolLifecycle::Start {
            call_id: "ok-call".to_string(),
            tool_name: ok_tool.clone(),
        },
        RecordedToolLifecycle::Finish {
            call_id: "ok-call".to_string(),
            tool_name: ok_tool,
            outcome: codex_extension_api::ToolCallOutcome::Completed { success: false },
        },
        RecordedToolLifecycle::Start {
            call_id: "failing-call".to_string(),
            tool_name: failing_tool.clone(),
        },
        RecordedToolLifecycle::Finish {
            call_id: "failing-call".to_string(),
            tool_name: failing_tool,
            outcome: codex_extension_api::ToolCallOutcome::Failed {
                handler_executed: true,
            },
        },
    ];
    let actual = records
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain(..)
        .collect::<Vec<_>>();
    assert_eq!(expected, actual);

    Ok(())
}

#[tokio::test]
async fn code_mode_execution_status_controls_hooks_and_lifecycle_without_changing_model_output()
-> anyhow::Result<()> {
    let (mut session, turn) = crate::session::tests::make_session_and_context().await;
    let records = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    builder.tool_lifecycle_contributor(Arc::new(ToolLifecycleRecorder {
        records: Arc::clone(&records),
    }));
    session.services.extensions = Arc::new(builder.build());

    std::fs::create_dir_all(&turn.config.codex_home)?;
    #[cfg(windows)]
    let hook_script = turn.config.codex_home.join("post_tool_use_hook.cmd");
    #[cfg(not(windows))]
    let hook_script = turn.config.codex_home.join("post_tool_use_hook.sh");
    let hook_marker = turn.config.codex_home.join("post_tool_use_hook_input.json");
    #[cfg(windows)]
    std::fs::write(
        &hook_script,
        format!(
            "@echo off\r\nmore > \"{}\"\r\necho {{\"continue\":true}}\r\n",
            hook_marker.display(),
        ),
    )?;
    #[cfg(not(windows))]
    std::fs::write(
        &hook_script,
        format!(
            "#!/bin/sh\ncat > {}\nprintf '%s\\n' '{{\"continue\":true}}'\n",
            shlex::try_quote(hook_marker.to_string_lossy().as_ref())?,
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(&hook_script)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook_script, permissions)?;
    }
    #[cfg(windows)]
    let hook_command = format!(r#""{}""#, hook_script.display());
    #[cfg(not(windows))]
    let hook_command = hook_script.display().to_string();
    std::fs::write(
        turn.config.codex_home.join("hooks.json"),
        serde_json::json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "^model_bounded_resource$",
                    "hooks": [{
                        "type": "command",
                        "command": hook_command,
                    }],
                }],
            },
        })
        .to_string(),
    )?;
    let hook_list = codex_hooks::list_hooks(HooksConfig {
        feature_enabled: true,
        config_layer_stack: Some(turn.config.config_layer_stack.clone()),
        ..HooksConfig::default()
    });
    assert_eq!(hook_list.hooks.len(), 1);
    let trusted_stack = turn.config.config_layer_stack.with_user_config(
        &turn.config.codex_home.join(codex_config::CONFIG_TOML_FILE),
        serde_json::from_value(serde_json::json!({
            "hooks": {
                "state": {
                    hook_list.hooks[0].key.clone(): {
                        "trusted_hash": hook_list.hooks[0].current_hash.clone(),
                    },
                },
            },
        }))?,
    )?;
    #[cfg(windows)]
    let hook_shell_program = None;
    #[cfg(windows)]
    let hook_shell_args = Vec::new();
    #[cfg(not(windows))]
    let mut hook_shell_argv = session
        .user_shell()
        .derive_exec_args("", /*use_login_shell*/ false);
    #[cfg(not(windows))]
    let hook_shell_program = Some(hook_shell_argv.remove(0));
    #[cfg(not(windows))]
    let _ = hook_shell_argv.pop();
    #[cfg(not(windows))]
    let hook_shell_args = hook_shell_argv;
    session
        .services
        .hooks
        .store(Arc::new(Hooks::new(HooksConfig {
            feature_enabled: true,
            config_layer_stack: Some(trusted_stack),
            shell_program: hook_shell_program,
            shell_args: hook_shell_args,
            ..HooksConfig::default()
        })));

    let tool_name = codex_tools::ToolName::plain("model_bounded_resource");
    let registry = ToolRegistry::with_handler_for_test(Arc::new(ModelBoundedCodeModeHandler {
        tool_name: tool_name.clone(),
    }));
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let direct = registry
        .dispatch_any_with_terminal_outcome(
            test_invocation_with_source(
                Arc::clone(&session),
                Arc::clone(&turn),
                "direct-model-projection",
                tool_name.clone(),
                crate::tools::context::ToolCallSource::Direct,
            ),
            /*terminal_outcome_reached*/ None,
        )
        .await?;
    assert!(
        !hook_marker.exists(),
        "the model-bounded direct projection must not invoke PostToolUse"
    );
    let code_mode = registry
        .dispatch_any_with_terminal_outcome(
            test_invocation_with_source(
                session,
                turn,
                "code-mode-resource-read",
                tool_name.clone(),
                crate::tools::context::ToolCallSource::CodeMode {
                    cell_id: "cell-1".to_string(),
                    runtime_tool_call_id: "tool-1".to_string(),
                },
            ),
            /*terminal_outcome_reached*/ None,
        )
        .await?;

    let codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } =
        direct.into_response()
    else {
        panic!("direct invocation should retain a function response");
    };
    assert_eq!(output.success, Some(false));
    assert_eq!(
        code_mode.code_mode_result(),
        serde_json::json!({ "complete": "resource value" })
    );
    let hook_input: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&hook_marker)?)?;
    assert_eq!(hook_input["tool_name"], "model_bounded_resource");
    assert_eq!(
        hook_input["tool_response"],
        serde_json::json!({ "projection": "bounded" })
    );

    let expected = vec![
        RecordedToolLifecycle::Start {
            call_id: "direct-model-projection".to_string(),
            tool_name: tool_name.clone(),
        },
        RecordedToolLifecycle::Finish {
            call_id: "direct-model-projection".to_string(),
            tool_name: tool_name.clone(),
            outcome: codex_extension_api::ToolCallOutcome::Completed { success: false },
        },
        RecordedToolLifecycle::Start {
            call_id: "code-mode-resource-read".to_string(),
            tool_name: tool_name.clone(),
        },
        RecordedToolLifecycle::Finish {
            call_id: "code-mode-resource-read".to_string(),
            tool_name,
            outcome: codex_extension_api::ToolCallOutcome::Completed { success: true },
        },
    ];
    let actual = records
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain(..)
        .collect::<Vec<_>>();
    assert_eq!(expected, actual);

    Ok(())
}

fn test_invocation(
    session: Arc<crate::session::session::Session>,
    turn: Arc<crate::session::turn_context::TurnContext>,
    call_id: &str,
    tool_name: codex_tools::ToolName,
) -> ToolInvocation {
    test_invocation_with_source(
        session,
        turn,
        call_id,
        tool_name,
        crate::tools::context::ToolCallSource::Direct,
    )
}

fn test_invocation_with_source(
    session: Arc<crate::session::session::Session>,
    turn: Arc<crate::session::turn_context::TurnContext>,
    call_id: &str,
    tool_name: codex_tools::ToolName,
    source: crate::tools::context::ToolCallSource,
) -> ToolInvocation {
    let step_context = StepContext::for_test(Arc::clone(&turn));
    ToolInvocation {
        session,
        step_context,
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: call_id.to_string(),
        tool_name,
        source,
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    }
}
