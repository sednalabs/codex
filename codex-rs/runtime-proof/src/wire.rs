use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Sha256;
use uuid::Uuid;

pub const RESERVED_META_KEY: &str = "runtime/execution-proof";
pub const MAX_CERTIFICATE_BYTES: usize = 16 * 1024;
pub const MAX_CERTIFICATE_SECONDS: i64 = 30 * 60;
pub const MAX_PROOF_SECONDS: i64 = 60;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CertificateHeader {
    pub alg: String,
    pub typ: String,
    pub kid: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CertificateClaims {
    pub version: u8,
    pub runtime_public_key: String,
    pub runtime_incarnation: Uuid,
    pub provider: String,
    pub artifact_sha256: String,
    pub principal: Uuid,
    pub project_id: u64,
    pub execution_lease_seconds: u32,
    pub work_item_ref: String,
    pub recipient: String,
    pub mcp_server: String,
    pub method: String,
    pub tool: String,
    pub purpose: String,
    pub iat: i64,
    pub exp: i64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RequestHeader<'a> {
    pub alg: &'static str,
    pub typ: &'static str,
    pub kid: &'a str,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestClaims {
    pub version: u8,
    pub certificate_sha256: String,
    pub runtime_incarnation: Uuid,
    pub execution_nonce: Uuid,
    pub provider_thread_id: String,
    pub recipient: String,
    pub method: String,
    pub tool: String,
    pub purpose: String,
    pub operation_sha256: String,
    pub nonce: Uuid,
    pub iat: i64,
    pub exp: i64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProofEnvelope<'a> {
    pub certificate: &'a str,
    pub proof: &'a str,
}

pub(crate) fn parse_certificate(
    certificate: &str,
) -> Result<(CertificateHeader, CertificateClaims)> {
    if certificate.len() > MAX_CERTIFICATE_BYTES {
        bail!("runtime proof certificate exceeds its size limit");
    }
    let mut parts = certificate.split('.');
    let (Some(header), Some(claims), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("runtime proof certificate is not compact JWS");
    };
    let signature = URL_SAFE_NO_PAD.decode(signature)?;
    if signature.len() != 64 {
        bail!("runtime proof certificate signature has an invalid size");
    }
    let header = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header)?)
        .context("decode runtime proof certificate header")?;
    let claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims)?)
        .context("decode runtime proof certificate claims")?;
    Ok((header, claims))
}

pub(crate) fn canonical_operation(tool: &str, parameters: &Value) -> Result<Vec<u8>> {
    let operation = serde_json::json!({
        "transport": "mcp",
        "method": "tools/call",
        "tool": tool,
        "parameters": parameters,
    });
    validate_numbers(&operation)?;
    let mut canonical = Vec::new();
    serde_json_canonicalizer::to_writer(&operation, &mut canonical)
        .context("canonicalize protected MCP claim operation")?;
    Ok(canonical)
}

fn validate_numbers(value: &Value) -> Result<()> {
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => Ok(()),
        Value::Number(number) => {
            const MAX_SAFE_INTEGER: i64 = (1_i64 << 53) - 1;
            if let Some(signed) = number.as_i64() {
                if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&signed) {
                    bail!("runtime proof rejects integers outside the exact IEEE-754 range");
                }
            } else if let Some(unsigned) = number.as_u64() {
                if unsigned > MAX_SAFE_INTEGER as u64 {
                    bail!("runtime proof rejects integers outside the exact IEEE-754 range");
                }
            } else {
                bail!("runtime proof rejects non-integer JSON numbers");
            }
            Ok(())
        }
        Value::Array(values) => values.iter().try_for_each(validate_numbers),
        Value::Object(values) => values.values().try_for_each(validate_numbers),
    }
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    let mut output = String::from("sha256:");
    use sha2::Digest as _;
    for byte in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}
