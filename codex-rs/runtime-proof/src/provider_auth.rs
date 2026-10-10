use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Deserializer;
use zeroize::Zeroize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderAuth {
    pub(crate) kind: String,
    pub(crate) credential_class: ProviderCredentialClass,
    pub(crate) access_token: SecretString,
    pub(crate) account_id: String,
    pub(crate) plan_type: Option<String>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderCredentialClass {
    Live,
    SyntheticFixture,
}

pub(crate) struct SecretString(pub(crate) String);

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

pub(crate) fn valid_provider_recipient(recipient: &str, class: &ProviderCredentialClass) -> bool {
    const LIVE_PROVIDER: &str = "https://chatgpt.com/backend-api/codex";
    match class {
        ProviderCredentialClass::Live => recipient == LIVE_PROVIDER,
        ProviderCredentialClass::SyntheticFixture => url::Url::parse(recipient).is_ok_and(|url| {
            url.scheme() == "http"
                && url.host_str() == Some("127.0.0.1")
                && url.port().is_some_and(|port| port > 0)
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.as_str() == recipient
        }),
    }
}

/// Protected runtime currently admits only account classes that the existing
/// cloud-config service treats as ineligible for its credential-bearing fetch.
pub(crate) fn ensure_protected_cloud_config_ineligible(plan_type: Option<&str>) -> Result<()> {
    let Some(plan_type) = plan_type else {
        bail!("protected runtime cannot classify cloud-config credential eligibility");
    };
    let plan_type = plan_type.to_ascii_lowercase();
    match plan_type.as_str() {
        "business"
        | "ent26"
        | "enterprise_cbp_usage_based"
        | "enterprise"
        | "hc"
        | "edu"
        | "education" => {
            bail!(
                "protected runtime account requires an unadmitted cloud-config credential consumer"
            )
        }
        "free" | "go" | "plus" | "pro" | "prolite" | "team" | "self_serve_business_usage_based" => {
            Ok(())
        }
        _ => bail!("protected runtime cannot classify cloud-config credential eligibility"),
    }
}

pub(crate) fn validate_provider_credential(
    provider: &ProviderAuth,
    recipient: &str,
    mcp_bearer: &str,
    expires_at: i64,
) -> Result<()> {
    match &provider.credential_class {
        ProviderCredentialClass::Live => {
            if provider.access_token.0 == synthetic_access_token(expires_at)? {
                bail!("live provider credential cannot use the public fixture token");
            }
        }
        ProviderCredentialClass::SyntheticFixture => {
            if !valid_provider_recipient(recipient, &provider.credential_class)
                || provider.account_id != "00000000-0000-4000-8000-000000000001"
                || provider.plan_type.as_deref() != Some("free")
                || mcp_bearer != "runtime-proof-fixture-mcp-token-v1"
                || provider.access_token.0 != synthetic_access_token(expires_at)?
            {
                bail!("synthetic provider credential does not match its fixed fixture contract");
            }
        }
    }
    Ok(())
}

fn synthetic_access_token(expires_at: i64) -> Result<String> {
    let header = serde_json::json!({
        "alg": "HS256",
        "kid": "runtime-proof-fixture-only",
        "typ": "JWT",
    });
    let claims = serde_json::json!({
        "aud": "runtime-proof-fixture-only",
        "exp": expires_at,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "00000000-0000-4000-8000-000000000001",
            "chatgpt_plan_type": "free",
            "chatgpt_user_id": "runtime-proof-fixture-user-v1",
        },
        "iss": "https://runtime-proof-fixture.invalid",
        "sub": "runtime-proof-fixture-user-v1",
    });
    let mut header_bytes = Vec::new();
    serde_json_canonicalizer::to_writer(&header, &mut header_bytes)?;
    let mut claims_bytes = Vec::new();
    serde_json_canonicalizer::to_writer(&claims, &mut claims_bytes)?;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    Ok(format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(header_bytes),
        URL_SAFE_NO_PAD.encode(claims_bytes),
        URL_SAFE_NO_PAD.encode("not-a-real-signature-runtime-proof-fixture-v1")
    ))
}
