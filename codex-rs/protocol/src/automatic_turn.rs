const AUTOMATIC_TURN_REFERENCE_MAX_BYTES: usize = 256;
/// Prefix for the opaque, non-authoritative continuation reference delivered in the existing
/// turn error details field. The server-side State row remains the only authority.
pub const AUTOMATIC_TURN_REFERENCE_DETAILS_PREFIX: &str = "automatic-turn-reference:v1:";

/// An opaque reference to a server-side automatic-turn admission.
///
/// It is safe to transport, but grants no authority without the matching durable State ticket,
/// exact thread and stable authenticated-principal identity, current epochs, and one-time
/// admission check. It must
/// never be used as user content or persisted in rollout history.
#[derive(
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
    ts_rs::TS,
)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
#[ts(type = "string")]
pub struct AutomaticTurnContinuationRef(String);

impl std::fmt::Debug for AutomaticTurnContinuationRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AutomaticTurnContinuationRef(<opaque>)")
    }
}

impl AutomaticTurnContinuationRef {
    /// Construct a reference returned by the server-side ticket issuer or received from the
    /// transport. Construction validates only the opaque token shape; it does not authorize it.
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        let bytes = value.as_bytes();
        (!value.is_empty()
            && bytes.len() <= AUTOMATIC_TURN_REFERENCE_MAX_BYTES
            && bytes.iter().all(u8::is_ascii_graphic))
            .then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Encode a non-authoritative reference for the existing live error-details transport.
    pub fn to_error_details(&self) -> String {
        format!("{AUTOMATIC_TURN_REFERENCE_DETAILS_PREFIX}{}", self.0)
    }

    pub fn from_error_details(details: Option<&str>) -> Option<Self> {
        let value = details?.strip_prefix(AUTOMATIC_TURN_REFERENCE_DETAILS_PREFIX)?;
        Self::new(value)
    }
}

impl TryFrom<String> for AutomaticTurnContinuationRef {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value).ok_or_else(|| "invalid automatic-turn continuation reference".to_string())
    }
}

impl From<AutomaticTurnContinuationRef> for String {
    fn from(value: AutomaticTurnContinuationRef) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuation_reference_round_trips_as_a_control_plane_value() {
        let reference = AutomaticTurnContinuationRef::new("opaque-reference-123")
            .expect("valid opaque reference");
        assert_eq!(
            serde_json::to_string(&reference).expect("serialize ref"),
            "\"opaque-reference-123\""
        );
        assert_eq!(
            serde_json::from_str::<AutomaticTurnContinuationRef>("\"opaque-reference-123\"")
                .expect("deserialize ref"),
            reference.clone()
        );
        assert_eq!(
            AutomaticTurnContinuationRef::from_error_details(Some(
                &reference.to_error_details()
            )),
            Some(reference)
        );
    }

    #[test]
    fn continuation_reference_is_opaque_and_shape_validation_is_not_authority() {
        assert!(AutomaticTurnContinuationRef::new(String::new()).is_none());
        assert!(AutomaticTurnContinuationRef::new("contains space").is_none());
        assert!(serde_json::from_str::<AutomaticTurnContinuationRef>("\"contains space\"")
            .is_err());

        // A valid-looking reference can be parsed but only the server-side State row authorizes
        // redemption; this protocol type has no provenance, principal, or capability data.
        let reference = AutomaticTurnContinuationRef::new("forged-but-well-shaped")
            .expect("shape alone is accepted");
        assert_eq!(reference.as_str(), "forged-but-well-shaped");
        assert_eq!(format!("{reference:?}"), "AutomaticTurnContinuationRef(<opaque>)");
    }
}
