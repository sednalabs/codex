use super::{ToolCallSource, ToolCallSourceKind};

#[test]
fn source_kind_preserves_explicit_requester_provenance() {
    let direct = ToolCallSource::Direct;
    let code_mode = ToolCallSource::CodeMode {
        cell_id: "child-agent".to_string(),
        runtime_tool_call_id: "runtime-call-1".to_string(),
    };
    let host_check = ToolCallSource::HostContinuityCheck;

    assert_eq!(direct.kind(), ToolCallSourceKind::Direct);
    assert_eq!(code_mode.kind(), ToolCallSourceKind::CodeMode);
    assert_eq!(host_check.kind(), ToolCallSourceKind::HostContinuityCheck);
}

#[test]
fn host_continuity_check_is_never_inferred_from_direct_or_child_identity() {
    let direct = ToolCallSource::Direct;
    let ordinary_v2_child = ToolCallSource::CodeMode {
        cell_id: "child-agent".to_string(),
        runtime_tool_call_id: "runtime-call-1".to_string(),
    };

    assert!(!direct.is_host_continuity_check());
    assert!(!ordinary_v2_child.is_host_continuity_check());
    assert!(ToolCallSource::HostContinuityCheck.is_host_continuity_check());
}
