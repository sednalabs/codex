use super::*;
use codex_keyring_store::tests::MockKeyringStore;
use keyring::Error as KeyringError;
use pretty_assertions::assert_eq;
use std::sync::Arc;

const SUBJECT: &str = "6d69c7fb-b1b5-43ef-8eb7-496dd0dc184d";

fn access_token(subject: &str, refresh: Option<&str>) -> OAuthAccessToken {
    let claims = serde_json::json!({ "sub": subject });
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    OAuthAccessToken::for_test(format!("e30.{payload}.signature"), refresh, 1_900_000_000)
}

fn stored_profile(refresh_token: &str) -> StoredApiRefreshProfile {
    StoredApiRefreshProfile::new(SUBJECT.to_string(), refresh_token.to_string())
}

fn refresh_result(
    access_token: OAuthAccessToken,
    rotated: Option<&str>,
) -> FixedOAuthRefreshResult {
    FixedOAuthRefreshResult {
        access_token: Ok(access_token),
        rotated_refresh_token: rotated.map(|token| Zeroizing::new(token.to_string())),
    }
}

#[test]
fn api_refresh_profile_is_separate_and_contains_no_access_token_or_extra_scope() {
    let store = MockKeyringStore::default();
    let access = access_token(SUBJECT, Some("rotated-refresh-token"));
    persist_new_profile(&store, access).expect("persist API refresh profile");

    let serialized = store
        .saved_value(KEYRING_ACCOUNT)
        .expect("fixed API keyring record");
    let parsed: StoredApiRefreshProfile =
        serde_json::from_str(&serialized).expect("parse fixed API profile");
    validate_profile(&parsed).expect("fixed profile contract");
    assert_eq!(parsed.subject, SUBJECT);
    assert_eq!(parsed.scope, "ops:write");
    assert!(!serialized.contains("offline_access"));
    assert!(!serialized.contains("signature"));
    assert_eq!(store.saved_value("MCP_OAUTH_ops"), None);
}

#[tokio::test]
async fn rotated_refresh_token_is_saved_before_the_access_token_is_returned() {
    let store = MockKeyringStore::default();
    let old_profile = stored_profile("old-refresh-token");
    let access = refresh_profile_with(&store, old_profile, |refresh| {
        Box::pin(async move {
            assert_eq!(refresh.as_str(), "old-refresh-token");
            Ok(refresh_result(
                access_token(SUBJECT, Some("rotated-refresh-token")),
                Some("rotated-refresh-token"),
            ))
        })
    })
    .await
    .expect("refresh and persist before returning");

    assert!(access.access_token().ends_with(".signature"));
    let serialized = store
        .saved_value(KEYRING_ACCOUNT)
        .expect("rotated API keyring record");
    let parsed: StoredApiRefreshProfile =
        serde_json::from_str(&serialized).expect("parse rotated API profile");
    assert_eq!(parsed.refresh_token, "rotated-refresh-token");
    assert_eq!(parsed.subject, SUBJECT);
}

#[tokio::test]
async fn keyring_rotation_failure_releases_no_access_token() {
    let store = Arc::new(MockKeyringStore::default());
    let old_profile = stored_profile("old-refresh-token");
    let callback_store = Arc::clone(&store);
    let result = refresh_profile_with(store.as_ref(), old_profile, move |_| {
        Box::pin(async move {
            callback_store.set_error(
                KEYRING_ACCOUNT,
                KeyringError::NoStorageAccess(Box::new(std::io::Error::other("test"))),
            );
            Ok(refresh_result(
                access_token(SUBJECT, Some("rotated-refresh-token")),
                Some("rotated-refresh-token"),
            ))
        })
    })
    .await;

    assert!(matches!(
        result,
        Err(error) if error.stage() == FixedOAuthLoginStage::CredentialStore
    ));
}

#[tokio::test]
async fn refresh_cannot_change_the_keyring_profile_subject() {
    let store = MockKeyringStore::default();
    let profile = stored_profile("old-refresh-token");
    let result = refresh_profile_with(&store, profile, |_| {
        Box::pin(async {
            Ok(refresh_result(
                access_token("7a5b79f0-5292-48f7-9aad-81ad2ef62d45", Some("rotated")),
                Some("rotated"),
            ))
        })
    })
    .await;

    assert!(matches!(
        result,
        Err(error) if error.stage() == FixedOAuthLoginStage::TokenValidation
    ));
    let serialized = store
        .saved_value(KEYRING_ACCOUNT)
        .expect("rotation is retained before rejecting a changed subject");
    let parsed: StoredApiRefreshProfile =
        serde_json::from_str(&serialized).expect("parse API profile");
    assert_eq!(parsed.refresh_token, "rotated");
    assert_eq!(parsed.subject, SUBJECT);
}

#[test]
fn refresh_profile_rejects_wrong_client_and_noncanonical_subject() {
    let mut wrong_client = stored_profile("refresh");
    wrong_client.client_id = "some-other-client".to_string();
    assert!(validate_profile(&wrong_client).is_err());

    let mut wrong_subject = stored_profile("refresh");
    wrong_subject.subject = "6D69C7FB-B1B5-43EF-8EB7-496DD0DC184D".to_string();
    assert!(validate_profile(&wrong_subject).is_err());
}

#[test]
fn api_profile_logout_does_not_delete_mcp_credentials() {
    let store = MockKeyringStore::default();
    store
        .save(KEYRING_SERVICE, KEYRING_ACCOUNT, "api-refresh")
        .expect("store API profile");
    store
        .save("Codex MCP Credentials", "ops-server-account", "mcp-refresh")
        .expect("store MCP profile");

    assert!(delete_api_profile_from(&store).expect("delete API profile"));
    assert_eq!(store.saved_value(KEYRING_ACCOUNT), None);
    assert_eq!(
        store.saved_value("ops-server-account").as_deref(),
        Some("mcp-refresh")
    );
}
