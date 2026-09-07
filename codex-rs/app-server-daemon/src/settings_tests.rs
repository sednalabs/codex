use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::DaemonSettings;

#[tokio::test]
async fn remote_control_save_preserves_updater_settings() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("settings.json");
    tokio::fs::write(&path, r#"{"remoteControlEnabled":true}"#)
        .await
        .expect("write legacy settings");

    let settings = DaemonSettings::load(&path).await.expect("load settings");
    assert_eq!(
        settings,
        DaemonSettings {
            remote_control_enabled: true,
            ..DaemonSettings::default()
        }
    );

    tokio::fs::write(
        &path,
        r#"{"remoteControlEnabled":true,"updater":{"autoUpdateEnabled":false,"updateIntervalMinutes":17},"futureSetting":42}"#,
    )
    .await
    .expect("write settings");
    let updated = DaemonSettings {
        remote_control_enabled: false,
        ..DaemonSettings::load(&path)
            .await
            .expect("load updater settings")
    };
    updated.save(&path).await.expect("save remote control");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &tokio::fs::read(&path).await.expect("read settings")
        )
        .expect("parse settings"),
        serde_json::json!({
            "remoteControlEnabled": false,
            "updater": {"autoUpdateEnabled": false, "updateIntervalMinutes": 17},
            "futureSetting": 42,
        })
    );
}

#[tokio::test]
async fn update_interval_accepts_long_values_and_rejects_zero() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("settings.json");
    for minutes in [u32::MAX, 0] {
        tokio::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "updater": {"updateIntervalMinutes": minutes},
            }))
            .expect("serialize invalid settings"),
        )
        .await
        .expect("write invalid settings");
        let loaded = DaemonSettings::load(&path).await;
        if minutes == 0 {
            assert!(loaded.is_err());
        } else {
            assert_eq!(
                loaded.expect("load long interval").update_interval_minutes,
                minutes
            );
        }
    }
}
