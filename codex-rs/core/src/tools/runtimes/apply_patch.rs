//! Apply Patch runtime: executes verified patches under the orchestrator.
//!
//! Assumes `apply_patch` verification/approval happened upstream. Reuses that
//! decision to avoid re-prompting, builds the self-invocation command for
//! `codex --codex-run-as-apply-patch`, and runs under the current
//! `SandboxAttempt` with a minimal environment.
use crate::error::CodexErr;
use crate::exec::ExecCapturePolicy;
use crate::exec::ExecToolCallOutput;
use crate::guardian::GuardianApprovalRequest;
use crate::guardian::review_approval_request;
use crate::guardian::routes_approval_to_guardian;
use crate::sandboxing::CommandSpec;
use crate::sandboxing::SandboxPermissions;
use crate::sandboxing::execute_env;
use crate::tools::sandboxing::Approvable;
use crate::tools::sandboxing::ApprovalCtx;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::Sandboxable;
use crate::tools::sandboxing::SandboxablePreference;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::tools::sandboxing::with_cached_approval;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::CODEX_CORE_APPLY_PATCH_ARG1;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::ReviewDecision;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug)]
pub struct ApplyPatchRequest {
    pub action: ApplyPatchAction,
    pub file_paths: Vec<AbsolutePathBuf>,
    pub changes: std::collections::HashMap<PathBuf, FileChange>,
    pub exec_approval_requirement: ExecApprovalRequirement,
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<PermissionProfile>,
    pub permissions_preapproved: bool,
    pub timeout_ms: Option<u64>,
    pub codex_exe: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApplyPatchLaunchMode {
    ConfiguredCodexLinuxSandboxExe,
    ConfiguredCodexLinuxSandboxExeViaApplyPatchAlias,
    CurrentExeFallback,
    CurrentExeFallbackRecoveredDeletedPath,
}

impl ApplyPatchLaunchMode {
    fn label(self) -> &'static str {
        match self {
            Self::ConfiguredCodexLinuxSandboxExe => "configured codex_linux_sandbox_exe",
            Self::ConfiguredCodexLinuxSandboxExeViaApplyPatchAlias => {
                "configured codex_linux_sandbox_exe (via apply_patch alias)"
            }
            Self::CurrentExeFallback => "current_exe fallback",
            Self::CurrentExeFallbackRecoveredDeletedPath => {
                "current_exe fallback (recovered deleted-path target)"
            }
        }
    }
}

#[derive(Debug)]
struct ApplyPatchLaunchSpec {
    spec: CommandSpec,
    executable: PathBuf,
    launch_mode: ApplyPatchLaunchMode,
}

#[derive(Default)]
pub struct ApplyPatchRuntime;

impl ApplyPatchRuntime {
    pub fn new() -> Self {
        Self
    }

    fn build_guardian_review_request(
        req: &ApplyPatchRequest,
        call_id: &str,
    ) -> GuardianApprovalRequest {
        GuardianApprovalRequest::ApplyPatch {
            id: call_id.to_string(),
            cwd: req.action.cwd.clone(),
            files: req.file_paths.clone(),
            change_count: req.changes.len(),
            patch: req.action.patch.clone(),
        }
    }

    fn is_linux_sandbox_helper(path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name == "codex-linux-sandbox"
                    || name == "codex-linux-sandbox.exe"
                    || name.starts_with("codex-linux-sandbox.")
            })
    }

    #[cfg(target_os = "linux")]
    fn recover_deleted_current_exe_path(path: &Path) -> Option<PathBuf> {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::ffi::OsStringExt;

        const DELETED_SUFFIX: &[u8] = b" (deleted)";

        if path.exists() {
            return None;
        }

        let bytes = path.as_os_str().as_bytes();
        let stripped = bytes.strip_suffix(DELETED_SUFFIX)?;
        let recovered = PathBuf::from(OsString::from_vec(stripped.to_vec()));
        recovered.exists().then_some(recovered)
    }

    fn resolve_codex_exe(
        req: &ApplyPatchRequest,
        codex_home: &Path,
    ) -> Result<(PathBuf, ApplyPatchLaunchMode), ToolError> {
        if let Some(path) = &req.codex_exe {
            return Ok((
                path.clone(),
                ApplyPatchLaunchMode::ConfiguredCodexLinuxSandboxExe,
            ));
        }

        #[cfg(not(target_os = "windows"))]
        let _ = codex_home;

        #[cfg(target_os = "windows")]
        {
            Ok((
                codex_windows_sandbox::resolve_current_exe_for_launch(codex_home, "codex.exe"),
                ApplyPatchLaunchMode::CurrentExeFallback,
            ))
        }
        #[cfg(not(target_os = "windows"))]
        {
            let exe = std::env::current_exe().map_err(|e| {
                ToolError::Rejected(format!(
                    "apply_patch failed to determine fallback current_exe for helper launch: {e}"
                ))
            })?;

            #[cfg(target_os = "linux")]
            if let Some(recovered) = Self::recover_deleted_current_exe_path(&exe) {
                return Ok((
                    recovered,
                    ApplyPatchLaunchMode::CurrentExeFallbackRecoveredDeletedPath,
                ));
            }

            Ok((exe, ApplyPatchLaunchMode::CurrentExeFallback))
        }
    }

    fn build_command_spec(
        req: &ApplyPatchRequest,
        codex_home: &Path,
    ) -> Result<ApplyPatchLaunchSpec, ToolError> {
        let (executable, launch_mode) = Self::resolve_codex_exe(req, codex_home)?;
        let (executable, program, args, launch_mode) = if req
            .codex_exe
            .as_deref()
            .is_some_and(Self::is_linux_sandbox_helper)
        {
            let apply_patch_exe = which::which("apply_patch").map_err(|err| {
                ToolError::Rejected(format!(
                    "apply_patch failed to locate apply_patch alias while codex_linux_sandbox_exe is configured: {err}"
                ))
            })?;
            (
                apply_patch_exe.clone(),
                apply_patch_exe.to_string_lossy().to_string(),
                vec![req.action.patch.clone()],
                ApplyPatchLaunchMode::ConfiguredCodexLinuxSandboxExeViaApplyPatchAlias,
            )
        } else {
            (
                executable.clone(),
                executable.to_string_lossy().to_string(),
                vec![
                    CODEX_CORE_APPLY_PATCH_ARG1.to_string(),
                    req.action.patch.clone(),
                ],
                launch_mode,
            )
        };
        Ok(ApplyPatchLaunchSpec {
            spec: CommandSpec {
                program,
                args,
                cwd: req.action.cwd.clone(),
                expiration: req.timeout_ms.into(),
                capture_policy: ExecCapturePolicy::ShellTool,
                // Run apply_patch with a minimal environment for determinism and to avoid leaks.
                env: HashMap::new(),
                sandbox_permissions: req.sandbox_permissions,
                additional_permissions: req.additional_permissions.clone(),
                justification: None,
            },
            executable,
            launch_mode,
        })
    }

    fn launch_diagnostics(
        req: &ApplyPatchRequest,
        executable: &Path,
        launch_mode: ApplyPatchLaunchMode,
    ) -> String {
        format!(
            "launch mode: {}, executable: {}, executable_exists: {}, cwd: {}, cwd_exists: {}, files: {}",
            launch_mode.label(),
            executable.display(),
            executable.exists(),
            req.action.cwd.display(),
            req.action.cwd.exists(),
            req.file_paths.len(),
        )
    }

    fn stdout_stream(ctx: &ToolCtx) -> Option<crate::exec::StdoutStream> {
        Some(crate::exec::StdoutStream {
            sub_id: ctx.turn.sub_id.clone(),
            call_id: ctx.call_id.clone(),
            tx_event: ctx.session.get_tx_event(),
        })
    }
}

impl Sandboxable for ApplyPatchRuntime {
    fn sandbox_preference(&self) -> SandboxablePreference {
        SandboxablePreference::Auto
    }
    fn escalate_on_failure(&self) -> bool {
        true
    }
}

impl Approvable<ApplyPatchRequest> for ApplyPatchRuntime {
    type ApprovalKey = AbsolutePathBuf;

    fn approval_keys(&self, req: &ApplyPatchRequest) -> Vec<Self::ApprovalKey> {
        req.file_paths.clone()
    }

    fn start_approval_async<'a>(
        &'a mut self,
        req: &'a ApplyPatchRequest,
        ctx: ApprovalCtx<'a>,
    ) -> BoxFuture<'a, ReviewDecision> {
        let session = ctx.session;
        let turn = ctx.turn;
        let call_id = ctx.call_id.to_string();
        let retry_reason = ctx.retry_reason.clone();
        let approval_keys = self.approval_keys(req);
        let changes = req.changes.clone();
        Box::pin(async move {
            if routes_approval_to_guardian(turn) {
                let action = ApplyPatchRuntime::build_guardian_review_request(req, ctx.call_id);
                return review_approval_request(session, turn, action, retry_reason).await;
            }
            if req.permissions_preapproved && retry_reason.is_none() {
                return ReviewDecision::Approved;
            }
            if let Some(reason) = retry_reason {
                let rx_approve = session
                    .request_patch_approval(
                        turn,
                        call_id,
                        changes.clone(),
                        Some(reason),
                        /*grant_root*/ None,
                    )
                    .await;
                return rx_approve.await.unwrap_or_default();
            }

            with_cached_approval(
                &session.services,
                "apply_patch",
                approval_keys,
                || async move {
                    let rx_approve = session
                        .request_patch_approval(
                            turn, call_id, changes, /*reason*/ None, /*grant_root*/ None,
                        )
                        .await;
                    rx_approve.await.unwrap_or_default()
                },
            )
            .await
        })
    }

    fn wants_no_sandbox_approval(&self, policy: AskForApproval) -> bool {
        match policy {
            AskForApproval::Never => false,
            AskForApproval::Granular(granular_config) => granular_config.allows_sandbox_approval(),
            AskForApproval::OnFailure => true,
            AskForApproval::OnRequest => true,
            AskForApproval::UnlessTrusted => true,
        }
    }

    // apply_patch approvals are decided upstream by assess_patch_safety.
    //
    // This override ensures the orchestrator runs the patch approval flow when required instead
    // of falling back to the global exec approval policy.
    fn exec_approval_requirement(
        &self,
        req: &ApplyPatchRequest,
    ) -> Option<ExecApprovalRequirement> {
        Some(req.exec_approval_requirement.clone())
    }
}

impl ToolRuntime<ApplyPatchRequest, ExecToolCallOutput> for ApplyPatchRuntime {
    async fn run(
        &mut self,
        req: &ApplyPatchRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecToolCallOutput, ToolError> {
        let launch = Self::build_command_spec(req, &ctx.turn.config.codex_home)?;
        let launch_diagnostics =
            Self::launch_diagnostics(req, &launch.executable, launch.launch_mode);
        let env = attempt.env_for(launch.spec, /*network*/ None).map_err(|err| {
            ToolError::Rejected(format!(
                "apply_patch failed to prepare helper launch ({launch_diagnostics}): {err}"
            ))
        })?;
        let out = execute_env(env, Self::stdout_stream(ctx))
            .await
            .map_err(|err| match err {
                CodexErr::Io(io_err) => ToolError::Rejected(format!(
                    "apply_patch failed to spawn helper ({launch_diagnostics}): {io_err}"
                )),
                other => ToolError::Codex(other),
            })?;
        Ok(out)
    }
}

#[cfg(test)]
#[path = "apply_patch_tests.rs"]
mod tests;
