use super::*;
use codex_protocol::protocol::GranularApprovalConfig;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

fn sample_request(codex_exe: Option<PathBuf>) -> ApplyPatchRequest {
    let path = std::env::temp_dir().join("guardian-apply-patch-test.txt");
    let action = ApplyPatchAction::new_add_for_test(&path, "hello".to_string());
    ApplyPatchRequest {
        action,
        file_paths: vec![
            AbsolutePathBuf::from_absolute_path(&path).expect("temp path should be absolute"),
        ],
        changes: HashMap::from([(
            path,
            FileChange::Add {
                content: "hello".to_string(),
            },
        )]),
        exec_approval_requirement: ExecApprovalRequirement::NeedsApproval {
            reason: None,
            proposed_execpolicy_amendment: None,
        },
        sandbox_permissions: SandboxPermissions::UseDefault,
        additional_permissions: None,
        permissions_preapproved: false,
        timeout_ms: None,
        codex_exe,
    }
}

#[test]
fn wants_no_sandbox_approval_granular_respects_sandbox_flag() {
    let runtime = ApplyPatchRuntime::new();
    assert!(runtime.wants_no_sandbox_approval(AskForApproval::OnRequest));
    assert!(
        !runtime.wants_no_sandbox_approval(AskForApproval::Granular(GranularApprovalConfig {
            sandbox_approval: false,
            rules: true,
            skill_approval: true,
            request_permissions: true,
            mcp_elicitations: true,
        }))
    );
    assert!(
        runtime.wants_no_sandbox_approval(AskForApproval::Granular(GranularApprovalConfig {
            sandbox_approval: true,
            rules: true,
            skill_approval: true,
            request_permissions: true,
            mcp_elicitations: true,
        }))
    );
}

#[test]
fn guardian_review_request_includes_patch_context() {
    let request = sample_request(None);
    let expected_cwd = request.action.cwd.clone();
    let expected_patch = request.action.patch.clone();

    let guardian_request = ApplyPatchRuntime::build_guardian_review_request(&request, "call-1");

    assert_eq!(
        guardian_request,
        GuardianApprovalRequest::ApplyPatch {
            id: "call-1".to_string(),
            cwd: expected_cwd,
            files: request.file_paths,
            change_count: 1usize,
            patch: expected_patch,
        }
    );
}

#[test]
fn build_command_spec_prefers_explicit_codex_exe() {
    let explicit_exe = std::env::temp_dir().join("codex-apply-patch-explicit");
    let request = sample_request(Some(explicit_exe.clone()));

    let launch = ApplyPatchRuntime::build_command_spec(&request, Path::new("/unused"))
        .expect("explicit exe should build a command spec");

    assert_eq!(launch.executable, explicit_exe);
    assert_eq!(
        launch.launch_mode,
        ApplyPatchLaunchMode::ConfiguredCodexLinuxSandboxExe
    );
    assert_eq!(launch.spec.program, launch.executable.to_string_lossy());
    assert_eq!(launch.spec.cwd, request.action.cwd);
}

#[test]
fn launch_diagnostics_report_launch_mode_and_path_existence() {
    let request = sample_request(None);
    let missing_exe = request.action.cwd.join("missing-codex-apply-patch-helper");

    let diagnostics = ApplyPatchRuntime::launch_diagnostics(
        &request,
        &missing_exe,
        ApplyPatchLaunchMode::CurrentExeFallback,
    );

    assert!(diagnostics.contains("launch mode: current_exe fallback"));
    assert!(diagnostics.contains(&format!("executable: {}", missing_exe.display())));
    assert!(diagnostics.contains("executable_exists: false"));
    assert!(diagnostics.contains(&format!("cwd: {}", request.action.cwd.display())));
    assert!(diagnostics.contains("cwd_exists: true"));
    assert!(diagnostics.contains("files: 1"));
}

#[cfg(target_os = "linux")]
#[test]
fn recover_deleted_current_exe_path_strips_linux_deleted_suffix() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let live_exe = temp.path().join("codex");
    std::fs::write(&live_exe, "stub").expect("write stub exe");

    let mut raw = live_exe.as_os_str().as_encoded_bytes().to_vec();
    raw.extend_from_slice(b" (deleted)");
    let deleted_path = PathBuf::from(OsString::from_vec(raw));

    let recovered = ApplyPatchRuntime::recover_deleted_current_exe_path(&deleted_path);

    assert_eq!(recovered, Some(live_exe));
}
