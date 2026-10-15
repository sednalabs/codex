use super::*;
use crate::legacy_core::config::ConfigBuilder;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[tokio::test]
async fn dismiss_version_creates_cache_file_when_missing() {
    let codex_home = tempdir().expect("temp codex home");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("load config");
    let version_file = version_filepath(&config);

    dismiss_version(&config, "999.0.0")
        .await
        .expect("dismiss version");

    let info = read_version_info(&version_file).expect("read version info");
    assert_eq!(info.last_checked_at, DateTime::<Utc>::UNIX_EPOCH);
    assert_eq!(
        (
            info.latest_version.as_str(),
            info.dismissed_version.as_deref()
        ),
        ("999.0.0", Some("999.0.0"))
    );
}

#[tokio::test]
async fn old_openai_cache_is_not_used_or_overwritten_as_sedna_cache() {
    let codex_home = tempdir().expect("temp codex home");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("load config");
    let old_openai_cache = codex_home.path().join("version.json");
    std::fs::write(
        &old_openai_cache,
        r#"{"latest_version":"999.0.0","last_checked_at":"2026-01-01T00:00:00Z"}"#,
    )
    .expect("write old cache");

    let sedna_cache = version_filepath(&config);
    assert_ne!(sedna_cache, old_openai_cache);
    assert!(read_version_info(&sedna_cache).is_err());

    dismiss_version(&config, "0.163.0")
        .await
        .expect("write Sedna dismissal");

    assert!(read_version_info(&old_openai_cache)
        .expect("old cache remains untouched")
        .dismissed_version
        .is_none());
    assert_eq!(
        read_version_info(&sedna_cache)
            .expect("read Sedna cache")
            .dismissed_version
            .as_deref(),
        Some("0.163.0")
    );
}
