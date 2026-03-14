use super::*;
use crate::sandboxing::SandboxPermissions;
use codex_apply_patch::MaybeApplyPatchVerified;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn approval_keys_include_move_destination() {
    let tmp = TempDir::new().expect("tmp");
    let cwd = tmp.path();
    std::fs::create_dir_all(cwd.join("old")).expect("create old dir");
    std::fs::create_dir_all(cwd.join("renamed/dir")).expect("create dest dir");
    std::fs::write(cwd.join("old/name.txt"), "old content\n").expect("write old file");
    let patch = r#"*** Begin Patch
*** Update File: old/name.txt
*** Move to: renamed/dir/name.txt
@@
-old content
+new content
*** End Patch"#;
    let argv = vec!["apply_patch".to_string(), patch.to_string()];
    let action = match codex_apply_patch::maybe_parse_apply_patch_verified(&argv, cwd) {
        MaybeApplyPatchVerified::Body(action) => action,
        other => panic!("expected patch body, got: {other:?}"),
    };

    let keys = file_paths_for_action(&action);
    assert_eq!(keys.len(), 2);
}

#[test]
fn build_apply_patch_request_preserves_launch_exe() {
    let tmp = TempDir::new().expect("tmp");
    let path = tmp.path().join("file.txt");
    let action = ApplyPatchAction::new_add_for_test(&path, "hello".to_string());
    let file_path = AbsolutePathBuf::from_absolute_path(&path).expect("absolute path");
    let codex_exe = Some(PathBuf::from("/tmp/codex-launch"));

    let request = build_apply_patch_request(
        action,
        vec![file_path],
        HashMap::from([(
            path,
            FileChange::Add {
                content: "hello".to_string(),
            },
        )]),
        ExecApprovalRequirement::NeedsApproval {
            reason: None,
            proposed_execpolicy_amendment: None,
        },
        crate::tools::handlers::EffectiveAdditionalPermissions {
            sandbox_permissions: SandboxPermissions::UseDefault,
            additional_permissions: None,
            permissions_preapproved: false,
        },
        Some(250),
        codex_exe.clone(),
    );

    assert_eq!(request.timeout_ms, Some(250));
    assert_eq!(request.codex_exe, codex_exe);
}
