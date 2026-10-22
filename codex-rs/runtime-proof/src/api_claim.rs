//! One fixed HTTP API claim request from the root-owned runtime-proof launcher.
//!
//! The API remains responsible for authenticating the bearer and verifying the
//! signed proof. Local JWT claim checks below are only a fail-early consistency
//! check against the fixed policy; they do not authenticate a token.

use crate::provider_auth::SecretString;
use crate::wire;
use anyhow::Result;
use anyhow::bail;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientBuilder;
use ed25519_dalek::Signer as _;
use ed25519_dalek::SigningKey;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;
use zeroize::Zeroizing;

pub(crate) const API_RECIPIENT: &str = "https://ops-api.grantmatler.com";
const API_ROUTE: &str = "/v0/work-items/claim";
const API_EXECUTION_RECEIPT_ROUTE: &str = "/v0/work-items/execution-receipt/read";
pub(crate) const API_PROVIDER: &str = "agent-ops-api-claim-client";
const API_ISSUER: &str = "https://auth.grantmatler.com/realms/agent-ops";
const API_AUDIENCE: &str = "ops-api";
pub(crate) const API_SCOPE: &str = "ops:write";
pub(crate) const API_BEARER_CREDENTIAL: &str = "ops-api-bearer";
pub(crate) const API_BEARER_MAX_BYTES: usize = 16 * 1024;
pub(crate) const API_BEARER_CREDENTIAL_MAX_BYTES: usize = API_BEARER_MAX_BYTES + 256;
const MAX_TOKEN_CLAIMS_BYTES: usize = 8 * 1024;
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_CLAIM_RESPONSE_BYTES: usize = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct ApiBearer {
    pub(crate) value: SecretString,
    pub(crate) principal: Uuid,
    pub(crate) expires_at: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApiBearerCredential {
    version: u8,
    generation: Uuid,
    access_token: SecretString,
}

#[derive(serde::Serialize)]
struct ApiBearerCredentialRef<'a> {
    version: u8,
    generation: Uuid,
    access_token: &'a str,
}

#[derive(Deserialize)]
struct TokenClaims {
    iss: String,
    aud: Audience,
    sub: String,
    exp: i64,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    scp: Option<ScopeClaim>,
    #[serde(default)]
    nbf: Option<i64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, expected: &str) -> bool {
        match self {
            Self::One(audience) => audience == expected,
            Self::Many(audiences) => audiences.iter().any(|audience| audience == expected),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScopeClaim {
    One(String),
    Many(Vec<String>),
}

impl ScopeClaim {
    fn contains(&self, expected: &str) -> bool {
        match self {
            Self::One(scopes) => scopes == expected,
            Self::Many(scopes) => {
                scopes.len() == 1 && scopes.first().is_some_and(|scope| scope == expected)
            }
        }
    }
}

pub(crate) fn encode_bearer_credential(value: &SecretString) -> Result<Zeroizing<Vec<u8>>> {
    let credential = ApiBearerCredentialRef {
        version: 1,
        generation: Uuid::new_v4(),
        access_token: &value.0,
    };
    Ok(Zeroizing::new(serde_json::to_vec(&credential)?))
}

pub(crate) fn decode_bearer_credential(bytes: &[u8]) -> Result<SecretString> {
    if bytes.is_empty() || bytes.len() > API_BEARER_CREDENTIAL_MAX_BYTES {
        bail!("fixed Ops API credential envelope exceeds its size limit");
    }
    let credential: ApiBearerCredential = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("fixed Ops API credential envelope is invalid"))?;
    if credential.version != 1 || credential.generation.is_nil() {
        bail!("fixed Ops API credential envelope is outside its versioned contract");
    }
    Ok(credential.access_token)
}

pub(crate) fn validate_bearer(value: String, principal: Uuid, now: i64) -> Result<ApiBearer> {
    validate_bearer_profile(value, Some(principal), now)
}

pub(crate) fn validate_bearer_for_provisioning(value: String, now: i64) -> Result<ApiBearer> {
    validate_bearer_profile(value, /*expected_principal*/ None, now)
}

fn validate_bearer_profile(
    value: String,
    expected_principal: Option<Uuid>,
    now: i64,
) -> Result<ApiBearer> {
    let value = SecretString(value);
    if value.0.is_empty()
        || value.0.len() > API_BEARER_MAX_BYTES
        || value
            .0
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        bail!("fixed Ops API bearer is outside its bounded format");
    }
    let mut segments = value.0.split('.');
    let (Some(header), Some(payload), Some(signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        bail!("fixed Ops API bearer is not a compact JWT");
    };
    if header.is_empty() || signature.is_empty() || payload.len() > MAX_TOKEN_CLAIMS_BYTES * 2 {
        bail!("fixed Ops API bearer claims exceed their size limit");
    }
    let claims_bytes = URL_SAFE_NO_PAD.decode(payload)?;
    if claims_bytes.len() > MAX_TOKEN_CLAIMS_BYTES {
        bail!("fixed Ops API bearer claims exceed their size limit");
    }
    let claims: TokenClaims = serde_json::from_slice(&claims_bytes)
        .map_err(|_| anyhow::anyhow!("fixed Ops API bearer claims are invalid"))?;
    let token_principal = Uuid::parse_str(&claims.sub)
        .ok()
        .filter(|principal| principal.to_string() == claims.sub);
    let principal = token_principal
        .ok_or_else(|| anyhow::anyhow!("fixed Ops API bearer has no canonical principal"))?;
    let subject_matches = expected_principal.is_none_or(|expected| principal == expected);
    let has_scope = claims
        .scope
        .as_deref()
        .is_some_and(|scope| scope == API_SCOPE)
        || claims
            .scp
            .as_ref()
            .is_some_and(|scopes| scopes.contains(API_SCOPE));
    let scope_claims_are_consistent = claims
        .scope
        .as_deref()
        .is_none_or(|scope| scope == API_SCOPE)
        && claims
            .scp
            .as_ref()
            .is_none_or(|scopes| scopes.contains(API_SCOPE));
    if claims.iss != API_ISSUER
        || !claims.aud.contains(API_AUDIENCE)
        || !subject_matches
        || !has_scope
        || !scope_claims_are_consistent
        || claims.nbf.is_some_and(|not_before| not_before > now)
        || claims.exp <= now
    {
        bail!("fixed Ops API bearer does not match the admitted caller profile");
    }
    Ok(ApiBearer {
        value,
        principal,
        expires_at: claims.exp,
    })
}

pub(crate) fn build_claim_body(
    work_item_ref: &str,
    expected_generation: u64,
    expected_updated_at: &str,
    request_id: Uuid,
) -> Result<Value> {
    validate_precondition(work_item_ref, expected_generation, expected_updated_at)?;
    Ok(serde_json::json!({
        "work_item_ref": work_item_ref,
        "claim_precondition": {
            "expected_generation": expected_generation,
            "expected_updated_at": expected_updated_at,
            "request_id": request_id.to_string(),
        },
    }))
}

pub(crate) fn validate_precondition(
    work_item_ref: &str,
    expected_generation: u64,
    expected_updated_at: &str,
) -> Result<()> {
    if !is_canonical_work_item_ref(work_item_ref)
        || expected_generation > ((1_u64 << 53) - 1)
        || expected_updated_at.is_empty()
        || expected_updated_at.len() > 64
        || expected_updated_at.chars().any(char::is_control)
        || chrono::DateTime::parse_from_rfc3339(expected_updated_at).is_err()
    {
        bail!("fixed Ops API claim precondition is invalid");
    }
    Ok(())
}

fn is_canonical_work_item_ref(value: &str) -> bool {
    let digits = value.strip_prefix('w').unwrap_or_default();
    !digits.is_empty()
        && digits.len() <= 16
        && digits.as_bytes()[0] != b'0'
        && digits.bytes().all(|byte| byte.is_ascii_digit())
}

pub(crate) fn create_proof(
    certificate: &str,
    runtime_seed: &[u8; 32],
    runtime_incarnation: Uuid,
    execution_nonce: Uuid,
    body: &Value,
    now: i64,
    certificate_expires_at: i64,
) -> Result<String> {
    let operation = wire::canonical_api_operation(body)?;
    let claims = wire::RequestClaims {
        version: 1,
        certificate_sha256: wire::certificate_digest(certificate.as_bytes()),
        runtime_incarnation,
        execution_nonce,
        provider_thread_id: None,
        recipient: API_RECIPIENT.to_string(),
        method: "POST".to_string(),
        tool: None,
        purpose: "work-claim".to_string(),
        operation_sha256: wire::digest(&operation),
        nonce: Uuid::new_v4(),
        iat: now,
        exp: now
            .saturating_add(wire::MAX_PROOF_SECONDS)
            .min(certificate_expires_at),
    };
    if claims.exp <= now {
        bail!("fixed Ops API proof expired before dispatch");
    }
    let runtime_kid = runtime_incarnation.to_string();
    let header = wire::RequestHeader {
        alg: "EdDSA",
        typ: "runtime-invocation+jwt",
        kid: &runtime_kid,
    };
    let encoded_header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?);
    let encoded_claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
    let signing_input = format!("{encoded_header}.{encoded_claims}");
    let key = SigningKey::from_bytes(runtime_seed);
    let signature = key.sign(signing_input.as_bytes());
    drop(key);
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

pub(crate) fn api_http_client() -> Result<HttpClient> {
    HttpClientBuilder::new()
        .without_redirects()
        .without_retries()
        .without_request_logging()
        .connect_timeout(CONNECT_TIMEOUT)
        .build_direct()
        .map_err(|_| anyhow::anyhow!("build fixed Ops API client"))
}

pub(crate) fn send_claim(
    client: &HttpClient,
    bearer: &SecretString,
    request_id: Uuid,
    certificate: &str,
    proof: &str,
    body: &Value,
) -> Result<()> {
    send_claim_to(
        client,
        API_RECIPIENT,
        bearer,
        request_id,
        certificate,
        proof,
        body,
    )
}

fn validate_claim_receipt(response_body: &[u8], expected_work_item_ref: &str) -> Result<()> {
    let receipt: Value = serde_json::from_slice(response_body).map_err(|_| {
        anyhow::anyhow!("fixed Ops API claim receipt is invalid; outcome unknown; do not replay")
    })?;
    let Some(receipt) = receipt.as_object() else {
        bail!("fixed Ops API claim receipt is invalid; outcome unknown; do not replay");
    };
    if receipt.keys().any(|key| {
        !matches!(
            key.as_str(),
            "outcome" | "work_item" | "blockers" | "queue_claim"
        )
    }) {
        bail!("fixed Ops API claim receipt is invalid; outcome unknown; do not replay");
    }
    let outcome = receipt.get("outcome").and_then(Value::as_str);
    let work_item_ref = receipt
        .get("work_item")
        .and_then(Value::as_object)
        .and_then(|work_item| work_item.get("work_item_ref"))
        .and_then(Value::as_str);
    if outcome != Some("claimed") || work_item_ref != Some(expected_work_item_ref) {
        bail!("fixed Ops API claim returned no matching claimed receipt; do not replay");
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionReceiptReadResponse {
    work_item_ref: String,
    request_id: Uuid,
    historical: Option<HistoricalExecutionReceipt>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoricalExecutionReceipt {
    generation: i64,
    execution_instance_id: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExecutionReceiptResolution {
    Recorded,
    Unresolved,
}

fn parse_execution_receipt(
    response_body: &[u8],
    expected_work_item_ref: &str,
    expected_request_id: Uuid,
) -> Result<ExecutionReceiptResolution> {
    let response: ExecutionReceiptReadResponse = serde_json::from_slice(response_body)
        .map_err(|_| anyhow::anyhow!("fixed Ops API execution receipt is invalid"))?;
    if response.work_item_ref != expected_work_item_ref
        || response.request_id != expected_request_id
    {
        bail!("fixed Ops API execution receipt does not match the requested operation");
    }
    match response.historical {
        Some(historical) => {
            // The authenticated Ops service validates the SQL receipt's work and project IDs
            // before projecting this request-scoped claim identity to the public response.
            let _claim_identity = (historical.generation, historical.execution_instance_id);
            Ok(ExecutionReceiptResolution::Recorded)
        }
        None => Ok(ExecutionReceiptResolution::Unresolved),
    }
}

async fn read_execution_receipt_async(
    client: &HttpClient,
    recipient: &str,
    bearer: &SecretString,
    expected_work_item_ref: &str,
    request_id: Uuid,
) -> Result<ExecutionReceiptResolution> {
    let body = Zeroizing::new(serde_json::to_vec(&serde_json::json!({
        "work_item_ref": expected_work_item_ref,
        "request_id": request_id,
    }))?);
    if body.len() > MAX_REQUEST_BYTES {
        bail!("fixed Ops API execution receipt request exceeds its size limit");
    }
    let mut authorization = Zeroizing::new(String::with_capacity(bearer.0.len() + 7));
    authorization.push_str("Bearer ");
    authorization.push_str(&bearer.0);
    let response = client
        .post(format!("{recipient}{API_EXECUTION_RECEIPT_ROUTE}"))
        .header("authorization", authorization.as_str())
        .header("content-type", "application/json")
        .body(body.as_slice().to_vec())
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("fixed Ops API execution receipt request failed"))?;
    if response.status().as_u16() != 200 {
        bail!("fixed Ops API execution receipt returned a non-success status");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CLAIM_RESPONSE_BYTES as u64)
    {
        bail!("fixed Ops API execution receipt exceeds its size limit");
    }
    let mut response = response;
    let mut response_body = Zeroizing::new(Vec::with_capacity(1024));
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("read fixed Ops API execution receipt failed"))?
    {
        let next_length = response_body
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| anyhow::anyhow!("fixed Ops API execution receipt size overflowed"))?;
        if next_length > MAX_CLAIM_RESPONSE_BYTES {
            bail!("fixed Ops API execution receipt exceeds its size limit");
        }
        response_body.extend_from_slice(&chunk);
    }
    parse_execution_receipt(&response_body, expected_work_item_ref, request_id)
}

async fn send_claim_to_async(
    client: &HttpClient,
    recipient: &str,
    bearer: &SecretString,
    request_id: Uuid,
    certificate: &str,
    proof: &str,
    body: &Value,
) -> Result<()> {
    let expected_work_item_ref = body
        .get("work_item_ref")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("fixed Ops API claim body has no work item reference"))?;
    let body = Zeroizing::new(serde_json::to_vec(body)?);
    if body.len() > MAX_REQUEST_BYTES {
        bail!("fixed Ops API claim body exceeds its size limit");
    }
    let proof_envelope = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&wire::ProofEnvelope {
        certificate,
        proof,
    })?);
    let mut authorization = Zeroizing::new(String::with_capacity(bearer.0.len() + 7));
    authorization.push_str("Bearer ");
    authorization.push_str(&bearer.0);
    let request = client
        .post(format!("{recipient}{API_ROUTE}"))
        .header("authorization", authorization.as_str())
        .header("content-type", "application/json")
        .header("x-ops-runtime-proof", proof_envelope)
        .header("x-ops-request-id", request_id.to_string())
        .body(body.as_slice().to_vec())
        .timeout(REQUEST_TIMEOUT);
    let claim_recorded = match request.send().await {
        Ok(mut response) if response.status().as_u16() == 200 => {
            if response
                .content_length()
                .is_some_and(|length| length > MAX_CLAIM_RESPONSE_BYTES as u64)
            {
                false
            } else {
                let mut receipt_body = Zeroizing::new(Vec::with_capacity(1024));
                let mut complete = true;
                loop {
                    match response.chunk().await {
                        Ok(Some(chunk)) => {
                            let Some(next_length) = receipt_body.len().checked_add(chunk.len())
                            else {
                                complete = false;
                                break;
                            };
                            if next_length > MAX_CLAIM_RESPONSE_BYTES {
                                complete = false;
                                break;
                            }
                            receipt_body.extend_from_slice(&chunk);
                        }
                        Ok(None) => break,
                        Err(_) => {
                            complete = false;
                            break;
                        }
                    }
                }
                complete && validate_claim_receipt(&receipt_body, expected_work_item_ref).is_ok()
            }
        }
        Ok(_) | Err(_) => false,
    };
    if claim_recorded {
        return Ok(());
    }
    match read_execution_receipt_async(
        client,
        recipient,
        bearer,
        expected_work_item_ref,
        request_id,
    )
    .await
    {
        Ok(ExecutionReceiptResolution::Recorded) => Ok(()),
        Ok(ExecutionReceiptResolution::Unresolved) => {
            bail!("fixed Ops API claim outcome remains unresolved; do not replay")
        }
        Err(_) => {
            bail!("fixed Ops API claim outcome is unknown; receipt read failed; do not replay")
        }
    }
}

fn send_claim_to(
    client: &HttpClient,
    recipient: &str,
    bearer: &SecretString,
    request_id: Uuid,
    certificate: &str,
    proof: &str,
    body: &Value,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| anyhow::anyhow!("build fixed Ops API runtime"))?;
    runtime.block_on(send_claim_to_async(
        client,
        recipient,
        bearer,
        request_id,
        certificate,
        proof,
        body,
    ))
}

pub(crate) fn unix_seconds() -> Result<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock is before the Unix epoch"))
        .and_then(|duration| {
            i64::try_from(duration.as_secs())
                .map_err(|_| anyhow::anyhow!("system clock is outside the supported range"))
        })
}

#[cfg(test)]
#[path = "api_claim_tests.rs"]
mod tests;
