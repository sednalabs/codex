use crate::startup;
use crate::wire;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use serde_json::Value;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use uuid::Uuid;
use zeroize::Zeroizing;

pub type ExecutionNonce = Uuid;

struct RuntimeProofSigner {
    certificate: String,
    claims: wire::CertificateClaims,
    seed: Zeroizing<[u8; 32]>,
}

#[derive(Default)]
enum SignerState {
    #[default]
    Uninitialized,
    Unconfigured,
    Ready(Box<RuntimeProofSigner>),
}

static SIGNER: OnceLock<Mutex<SignerState>> = OnceLock::new();

fn signer_state() -> &'static Mutex<SignerState> {
    SIGNER.get_or_init(|| Mutex::new(SignerState::Uninitialized))
}

pub(crate) fn install_bootstrap(certificate: String, seed: Zeroizing<[u8; 32]>) -> Result<()> {
    let artifact_sha256 = startup::current_artifact_sha256()?;
    let (header, claims) = wire::parse_certificate(&certificate)?;
    validate_certificate(&header, &claims, &seed, &artifact_sha256, unix_seconds()?)?;
    let mut state = signer_state()
        .lock()
        .map_err(|_| anyhow::anyhow!("runtime proof signer state is unavailable"))?;
    if !matches!(&*state, SignerState::Uninitialized) {
        bail!("runtime proof startup was already initialized");
    }
    *state = SignerState::Ready(Box::new(RuntimeProofSigner {
        certificate,
        claims,
        seed,
    }));
    Ok(())
}

pub(crate) fn install_unconfigured() -> Result<()> {
    let mut state = signer_state()
        .lock()
        .map_err(|_| anyhow::anyhow!("runtime proof signer state is unavailable"))?;
    if !matches!(&*state, SignerState::Uninitialized) {
        bail!("runtime proof startup was already initialized");
    }
    *state = SignerState::Unconfigured;
    Ok(())
}

pub(crate) fn erase_signer_authority() -> Result<()> {
    let mut state = signer_state()
        .lock()
        .map_err(|_| anyhow::anyhow!("runtime proof signer state is unavailable"))?;
    *state = SignerState::Unconfigured;
    Ok(())
}

pub(crate) fn certificate_for_auth() -> Result<(wire::CertificateClaims, String)> {
    let state = signer_state()
        .lock()
        .map_err(|_| anyhow::anyhow!("runtime proof signer state is unavailable"))?;
    let SignerState::Ready(signer) = &*state else {
        bail!("protected runtime proof certificate is unavailable");
    };
    Ok((
        signer.claims.clone(),
        wire::digest(signer.certificate.as_bytes()),
    ))
}

pub fn sign_claim_proof(
    execution_nonce: &ExecutionNonce,
    provider_thread_id: &str,
    server: &str,
    transport_url: Option<&str>,
    tool: &str,
    parameters: &Value,
) -> Result<Option<Value>> {
    let mut state = signer_state()
        .lock()
        .map_err(|_| anyhow::anyhow!("runtime proof signer state is unavailable"))?;
    let SignerState::Ready(signer) = &*state else {
        return Ok(None);
    };
    if !signer.matches_target(server, transport_url, tool)? {
        return Ok(None);
    }
    erase_on_protection_failure(&mut state, startup::verify_runtime_protection())?;
    let certificate_expired = match &*state {
        SignerState::Ready(signer) => unix_seconds()? >= signer.claims.exp,
        SignerState::Uninitialized | SignerState::Unconfigured => false,
    };
    if certificate_expired {
        *state = SignerState::Unconfigured;
        bail!("runtime proof certificate expired; signer authority erased");
    }
    let SignerState::Ready(signer) = &*state else {
        bail!("runtime proof signer authority was erased");
    };
    signer.create_claim_proof(execution_nonce, provider_thread_id, parameters)
}

impl RuntimeProofSigner {
    fn matches_target(
        &self,
        server: &str,
        transport_url: Option<&str>,
        tool: &str,
    ) -> Result<bool> {
        let server_matches = server == self.claims.mcp_server;
        let recipient_matches = transport_url == Some(self.claims.recipient.as_str());
        let selected_tool = tool == self.claims.tool;
        if !server_matches && !recipient_matches {
            return Ok(false);
        }
        if !server_matches || !recipient_matches {
            if selected_tool {
                bail!("protected claim MCP server does not match its pinned recipient");
            }
            return Ok(false);
        }
        Ok(selected_tool)
    }

    fn create_claim_proof(
        &self,
        execution_nonce: &ExecutionNonce,
        provider_thread_id: &str,
        parameters: &Value,
    ) -> Result<Option<Value>> {
        let now = unix_seconds()?;
        if now < self.claims.iat || now >= self.claims.exp {
            bail!("runtime proof certificate is outside its validity period");
        }
        if self.claims.method != "tools/call" || self.claims.purpose != "work-claim" {
            bail!("runtime proof certificate is outside the claim operation scope");
        }
        if provider_thread_id.is_empty() || provider_thread_id.len() > 256 {
            bail!("native provider thread evidence is invalid");
        }
        let work_item_ref = parameters
            .as_object()
            .and_then(|parameters| parameters.get("work_item_ref"))
            .and_then(Value::as_str);
        if work_item_ref != Some(self.claims.work_item_ref.as_str()) {
            bail!("protected claim target does not match the signed work item");
        }
        validate_claim_precondition(parameters)?;

        let operation = wire::canonical_operation(&self.claims.tool, parameters)?;
        let proof_claims = wire::RequestClaims {
            version: 1,
            certificate_sha256: wire::digest(self.certificate.as_bytes()),
            runtime_incarnation: self.claims.runtime_incarnation,
            execution_nonce: *execution_nonce,
            provider_thread_id: provider_thread_id.to_string(),
            recipient: self.claims.recipient.clone(),
            method: self.claims.method.clone(),
            tool: self.claims.tool.clone(),
            purpose: self.claims.purpose.clone(),
            operation_sha256: wire::digest(&operation),
            nonce: Uuid::new_v4(),
            iat: now,
            exp: now
                .saturating_add(wire::MAX_PROOF_SECONDS)
                .min(self.claims.exp),
        };
        let runtime_kid = self.claims.runtime_incarnation.to_string();
        let header = wire::RequestHeader {
            alg: "EdDSA",
            typ: "runtime-invocation+jwt",
            kid: &runtime_kid,
        };
        let header = serde_json::to_vec(&header)?;
        let claims = serde_json::to_vec(&proof_claims)?;
        let encoded_header = URL_SAFE_NO_PAD.encode(header);
        let encoded_claims = URL_SAFE_NO_PAD.encode(claims);
        let signing_input = format!("{encoded_header}.{encoded_claims}");
        let key = SigningKey::from_bytes(&self.seed);
        let signature = key.sign(signing_input.as_bytes());
        drop(key);
        let proof = format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        );
        let envelope = wire::ProofEnvelope {
            certificate: &self.certificate,
            proof: &proof,
        };
        Ok(Some(serde_json::to_value(envelope)?))
    }
}

fn validate_claim_precondition(parameters: &Value) -> Result<()> {
    let precondition = parameters
        .as_object()
        .and_then(|parameters| parameters.get("claim_precondition"))
        .and_then(Value::as_object)
        .context("protected claim requires a complete claim_precondition")?;
    let generation = precondition
        .get("expected_generation")
        .and_then(Value::as_u64)
        .context("protected claim generation must be a nonnegative safe integer")?;
    if generation > ((1_u64 << 53) - 1) {
        bail!("protected claim generation is outside the exact IEEE-754 range");
    }
    let updated_at = precondition
        .get("expected_updated_at")
        .and_then(Value::as_str)
        .context("protected claim revision timestamp is missing")?;
    chrono::DateTime::parse_from_rfc3339(updated_at)
        .context("protected claim revision timestamp is not RFC3339")?;
    let request_id = precondition
        .get("request_id")
        .and_then(Value::as_str)
        .context("protected claim request id is missing")?;
    Uuid::parse_str(request_id).context("protected claim request id is not a UUID")?;
    Ok(())
}

fn erase_on_protection_failure(state: &mut SignerState, protection: Result<()>) -> Result<()> {
    if let Err(error) = protection {
        *state = SignerState::Unconfigured;
        bail!("runtime proof protection changed; signer authority erased: {error}");
    }
    Ok(())
}

fn validate_certificate(
    header: &wire::CertificateHeader,
    claims: &wire::CertificateClaims,
    seed: &[u8; 32],
    artifact_sha256: &str,
    now: i64,
) -> Result<()> {
    let signing_key = SigningKey::from_bytes(seed);
    let public_key = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().as_bytes());
    if header.alg != "EdDSA"
        || header.typ != "runtime-certificate+jwt"
        || header.kid.is_empty()
        || header.kid.len() > 128
        || claims.version != 1
        || claims.runtime_public_key != public_key
        || claims.runtime_incarnation.is_nil()
        || claims.provider != "codex-native-runtime"
        || claims.artifact_sha256 != artifact_sha256
        || claims.principal.is_nil()
        || claims.project_id == 0
        || claims.execution_lease_seconds == 0
        || claims.execution_lease_seconds > 30 * 60
        || claims.work_item_ref.is_empty()
        || claims.work_item_ref.len() > 128
        || claims.recipient.is_empty()
        || claims.recipient.len() > 2048
        || claims.mcp_server.is_empty()
        || claims.mcp_server.len() > 128
        || claims.method != "tools/call"
        || claims.tool != "work_item_claim"
        || claims.purpose != "work-claim"
        || claims.iat > now
        || claims.exp <= now
        || claims.exp <= claims.iat
        || claims.exp.saturating_sub(claims.iat) > wire::MAX_CERTIFICATE_SECONDS
    {
        bail!("runtime proof bootstrap certificate fields are invalid");
    }
    Ok(())
}

fn unix_seconds() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_secs() as i64)
}

#[cfg(test)]
#[path = "signer_tests.rs"]
mod tests;
