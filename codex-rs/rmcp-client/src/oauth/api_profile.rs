//! One fixed API claim refresh profile supplied by its protected caller.
//!
//! This is deliberately separate from MCP credential resolution: it never reads
//! Codex's fallback credential file and never adopts or deletes an MCP entry.

use anyhow::Result;
use anyhow::bail;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use zeroize::Zeroize as _;
use zeroize::Zeroizing;

use crate::FixedOAuthLoginError;
use crate::FixedOAuthLoginStage;
use crate::OAuthAccessToken;
use crate::perform_oauth_login::FIXED_API_CLAIM_CLIENT_ID;
use crate::perform_oauth_login::FIXED_API_CLAIM_ISSUER;
use crate::perform_oauth_login::FIXED_API_CLAIM_RESOURCE;
use crate::perform_oauth_login::FIXED_API_CLAIM_SCOPE;
use crate::perform_oauth_login::FixedOAuthRefreshResult;
use crate::perform_oauth_login::perform_fixed_ops_api_access_token_login;
use crate::perform_oauth_login::perform_fixed_ops_api_access_token_login_with_callback_input;
use crate::perform_oauth_login::perform_fixed_ops_api_access_token_refresh;

const REFRESH_PROFILE_VERSION: u8 = 1;
const MAX_REFRESH_TOKEN_BYTES: usize = 16 * 1024;
const MAX_PROFILE_BYTES: usize = 24 * 1024;
const MAX_JWT_PAYLOAD_BYTES: usize = 8 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredApiRefreshProfile {
    version: u8,
    issuer: String,
    client_id: String,
    resource: String,
    scope: String,
    subject: String,
    refresh_token: String,
}

impl Drop for StoredApiRefreshProfile {
    fn drop(&mut self) {
        self.refresh_token.zeroize();
    }
}

/// Exchanges the fixed API refresh token, persisting any rotation before the
/// access token can be returned to the root-owned launcher. If no profile is
/// present, performs the fixed PKCE authorization once and stores only its
/// refresh token through the supplied protected persistence callback.
pub async fn perform_fixed_ops_api_access_token_with_refresh(
    open_authorization_url: fn(&str) -> anyhow::Result<()>,
    read_callback_url: fn(&str) -> anyhow::Result<String>,
    use_manual_callback_input: bool,
    load_profile: impl FnOnce() -> Result<Option<String>>,
    save_profile: impl FnMut(&str) -> Result<()>,
) -> std::result::Result<OAuthAccessToken, FixedOAuthLoginError> {
    let serialized = load_profile()
        .map_err(|_| FixedOAuthLoginError::at(FixedOAuthLoginStage::CredentialStore))?;
    if let Some(serialized) = serialized {
        let serialized = Zeroizing::new(serialized);
        if serialized.len() > MAX_PROFILE_BYTES {
            return Err(FixedOAuthLoginError::at(
                FixedOAuthLoginStage::CredentialStore,
            ));
        }
        let profile: StoredApiRefreshProfile = serde_json::from_str(&serialized)
            .map_err(|_| FixedOAuthLoginError::at(FixedOAuthLoginStage::CredentialStore))?;
        validate_profile(&profile)
            .map_err(|_| FixedOAuthLoginError::at(FixedOAuthLoginStage::CredentialStore))?;

        return refresh_profile_with(profile, save_profile, |refresh_token| {
            Box::pin(
                async move { perform_fixed_ops_api_access_token_refresh(&refresh_token).await },
            )
        })
        .await;
    }

    let access_token = if use_manual_callback_input {
        perform_fixed_ops_api_access_token_login_with_callback_input(read_callback_url).await?
    } else {
        perform_fixed_ops_api_access_token_login(open_authorization_url).await?
    };
    persist_new_profile(access_token, save_profile)
}

async fn refresh_profile_with<F, Fut>(
    profile: StoredApiRefreshProfile,
    mut save_profile: impl FnMut(&str) -> Result<()>,
    refresh: F,
) -> std::result::Result<OAuthAccessToken, FixedOAuthLoginError>
where
    F: FnOnce(Zeroizing<String>) -> Fut,
    Fut: std::future::Future<
            Output = std::result::Result<FixedOAuthRefreshResult, FixedOAuthLoginError>,
        >,
{
    let refresh_result = refresh(Zeroizing::new(profile.refresh_token.clone())).await?;
    let rotated_refresh_token = refresh_result
        .rotated_refresh_token
        .as_deref()
        .map(String::as_str)
        .unwrap_or(&profile.refresh_token);
    let updated_profile =
        StoredApiRefreshProfile::new(profile.subject.clone(), rotated_refresh_token.to_string());
    save_profile_with(&mut save_profile, &updated_profile)
        .map_err(|_| FixedOAuthLoginError::at(FixedOAuthLoginStage::CredentialStore))?;
    let access_token = refresh_result.access_token?;
    let subject = subject_from_access_token(access_token.access_token())
        .map_err(|_| FixedOAuthLoginError::at(FixedOAuthLoginStage::TokenValidation))?;
    if subject != profile.subject {
        return Err(FixedOAuthLoginError::at(
            FixedOAuthLoginStage::TokenValidation,
        ));
    }
    Ok(access_token)
}

fn persist_new_profile(
    access_token: OAuthAccessToken,
    mut save_profile: impl FnMut(&str) -> Result<()>,
) -> std::result::Result<OAuthAccessToken, FixedOAuthLoginError> {
    let subject = subject_from_access_token(access_token.access_token())
        .map_err(|_| FixedOAuthLoginError::at(FixedOAuthLoginStage::TokenValidation))?;
    let refresh_token = access_token
        .refresh_token()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| FixedOAuthLoginError::at(FixedOAuthLoginStage::TokenValidation))?;
    let profile = StoredApiRefreshProfile::new(subject, refresh_token.to_string());
    save_profile_with(&mut save_profile, &profile)
        .map_err(|_| FixedOAuthLoginError::at(FixedOAuthLoginStage::CredentialStore))?;
    Ok(access_token)
}

impl StoredApiRefreshProfile {
    fn new(subject: String, refresh_token: String) -> Self {
        Self {
            version: REFRESH_PROFILE_VERSION,
            issuer: FIXED_API_CLAIM_ISSUER.to_string(),
            client_id: FIXED_API_CLAIM_CLIENT_ID.to_string(),
            resource: FIXED_API_CLAIM_RESOURCE.to_string(),
            scope: FIXED_API_CLAIM_SCOPE.to_string(),
            subject,
            refresh_token,
        }
    }
}

fn validate_profile(profile: &StoredApiRefreshProfile) -> Result<()> {
    if profile.version != REFRESH_PROFILE_VERSION
        || profile.issuer != FIXED_API_CLAIM_ISSUER
        || profile.client_id != FIXED_API_CLAIM_CLIENT_ID
        || profile.resource != FIXED_API_CLAIM_RESOURCE
        || profile.scope != FIXED_API_CLAIM_SCOPE
        || !is_canonical_uuid(&profile.subject)
        || profile.refresh_token.is_empty()
        || profile.refresh_token.len() > MAX_REFRESH_TOKEN_BYTES
        || profile.refresh_token.chars().any(char::is_control)
    {
        bail!("fixed API refresh profile does not match its source contract");
    }
    Ok(())
}

fn save_profile_with(
    save: &mut impl FnMut(&str) -> Result<()>,
    profile: &StoredApiRefreshProfile,
) -> Result<()> {
    validate_profile(profile)?;
    let serialized = Zeroizing::new(serde_json::to_string(profile)?);
    if serialized.len() > MAX_PROFILE_BYTES {
        bail!("fixed API refresh profile exceeds its size limit");
    }
    save(&serialized)
}

/// This reads an unverified subject claim only to keep the user's refresh
/// record attached to one identity. The root launcher independently validates
/// issuer, audience, scope, expiry, and policy subject before using the token.
fn subject_from_access_token(access_token: &str) -> Result<String> {
    let mut segments = access_token.split('.');
    let (Some(_header), Some(payload), Some(_signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        bail!("fixed API access token is not a compact JWT");
    };
    if payload.len() > MAX_JWT_PAYLOAD_BYTES * 2 {
        bail!("fixed API access token claims exceed their size limit");
    }
    let claims = URL_SAFE_NO_PAD.decode(payload)?;
    if claims.len() > MAX_JWT_PAYLOAD_BYTES {
        bail!("fixed API access token claims exceed their size limit");
    }
    let claims: Value = serde_json::from_slice(&claims)?;
    let subject = claims
        .get("sub")
        .and_then(Value::as_str)
        .filter(|subject| is_canonical_uuid(subject))
        .ok_or_else(|| anyhow::anyhow!("fixed API access token has no canonical subject"))?;
    Ok(subject.to_string())
}

fn is_canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

#[cfg(test)]
#[path = "api_profile_tests.rs"]
mod tests;
