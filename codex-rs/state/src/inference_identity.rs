use std::error::Error;
use std::fmt;

use codex_protocol::models::ThreadInferenceIdentityAuthority;
use codex_protocol::models::ThreadInferenceIdentityValidationError;
use serde::Deserialize;
use serde::Serialize;

pub const THREAD_INFERENCE_IDENTITY_AUTHORITY_VERSION: u8 = 1;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VersionedThreadInferenceIdentityAuthority {
    version: u8,
    authority: ThreadInferenceIdentityAuthority,
}

/// Failure to encode typed thread inference identity authority for durable storage.
#[derive(Debug)]
pub enum ThreadInferenceIdentityAuthorityEncodeError {
    InvalidIdentity(ThreadInferenceIdentityValidationError),
    MalformedAuthority,
    Serialization(serde_json::Error),
}

impl fmt::Display for ThreadInferenceIdentityAuthorityEncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity(error) => {
                write!(formatter, "invalid inference identity: {error}")
            }
            Self::MalformedAuthority => {
                formatter.write_str("malformed inference identity authority cannot be written")
            }
            Self::Serialization(error) => {
                write!(
                    formatter,
                    "failed to serialize inference identity authority: {error}"
                )
            }
        }
    }
}

impl Error for ThreadInferenceIdentityAuthorityEncodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidIdentity(error) => Some(error),
            Self::Serialization(error) => Some(error),
            Self::MalformedAuthority => None,
        }
    }
}

/// Decodes versioned durable authority without treating malformed data as legacy absence.
///
/// SQL `NULL` is the only representation of [`ThreadInferenceIdentityAuthority::LegacyMissing`].
/// Unsupported versions, semantically invalid identities, and disallowed nested authority states
/// retain the exact stored text as [`ThreadInferenceIdentityAuthority::Malformed`].
pub fn decode_thread_inference_identity_authority(
    raw: Option<&str>,
) -> ThreadInferenceIdentityAuthority {
    let Some(raw) = raw else {
        return ThreadInferenceIdentityAuthority::LegacyMissing;
    };
    match serde_json::from_str::<VersionedThreadInferenceIdentityAuthority>(raw) {
        Ok(envelope)
            if envelope.version == THREAD_INFERENCE_IDENTITY_AUTHORITY_VERSION
                && matches!(
                    &envelope.authority,
                    ThreadInferenceIdentityAuthority::Valid(_)
                        | ThreadInferenceIdentityAuthority::Cleared
                ) =>
        {
            envelope.authority
        }
        Ok(_) | Err(_) => ThreadInferenceIdentityAuthority::Malformed {
            raw: raw.to_string(),
        },
    }
}

/// Encodes validated authority for durable storage.
///
/// Legacy absence becomes SQL `NULL`; malformed read diagnostics are rejected at the typed write
/// boundary rather than being re-authored as if they were valid authority.
pub fn encode_thread_inference_identity_authority(
    authority: &ThreadInferenceIdentityAuthority,
) -> Result<Option<String>, ThreadInferenceIdentityAuthorityEncodeError> {
    match authority {
        ThreadInferenceIdentityAuthority::LegacyMissing => Ok(None),
        ThreadInferenceIdentityAuthority::Valid(identity) => {
            identity
                .validate()
                .map_err(ThreadInferenceIdentityAuthorityEncodeError::InvalidIdentity)?;
            encode_envelope(authority).map(Some)
        }
        ThreadInferenceIdentityAuthority::Cleared => encode_envelope(authority).map(Some),
        ThreadInferenceIdentityAuthority::Malformed { .. } => {
            Err(ThreadInferenceIdentityAuthorityEncodeError::MalformedAuthority)
        }
    }
}

fn encode_envelope(
    authority: &ThreadInferenceIdentityAuthority,
) -> Result<String, ThreadInferenceIdentityAuthorityEncodeError> {
    serde_json::to_string(&VersionedThreadInferenceIdentityAuthority {
        version: THREAD_INFERENCE_IDENTITY_AUTHORITY_VERSION,
        authority: authority.clone(),
    })
    .map_err(ThreadInferenceIdentityAuthorityEncodeError::Serialization)
}

#[cfg(test)]
#[path = "inference_identity_tests.rs"]
mod tests;
