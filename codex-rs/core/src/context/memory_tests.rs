use super::*;
use codex_protocol::models::ContentItem;
use pretty_assertions::assert_eq;

#[test]
fn extraction_chunks_preserve_unicode_evidence_with_bounded_messages() {
    let evidence = "User correction: 🐈\n".repeat(2_000);
    let mut reconstructed = String::new();
    for message in MemoryContextFragment::extraction_messages(&evidence) {
        let ResponseItem::Message { role, content, .. } = message else {
            panic!("message")
        };
        assert_eq!(role, "user");
        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("text")
        };
        assert!(text.len() < 9_000);
        reconstructed.push_str(text);
    }
    assert_eq!(reconstructed, evidence);
}
