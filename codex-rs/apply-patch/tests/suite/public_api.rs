use codex_apply_patch::UpdateFileChunk;
use pretty_assertions::assert_eq;

#[test]
fn update_file_chunk_remains_constructible_from_an_external_crate() {
    let chunk = UpdateFileChunk {
        change_context: Some("fn example()".to_string()),
        old_lines: vec!["old".to_string()],
        new_lines: vec!["new".to_string()],
        is_end_of_file: false,
    };

    assert_eq!(chunk.change_context.as_deref(), Some("fn example()"));
    assert_eq!(chunk.old_lines, ["old"]);
    assert_eq!(chunk.new_lines, ["new"]);
    assert!(!chunk.is_end_of_file);
}
