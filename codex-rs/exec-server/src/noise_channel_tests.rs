use base64::Engine;
use clatter::traits::{CryptoRng, Kem, RngCore};
#[allow(deprecated)]
use ml_kem::ExpandedKeyEncoding;
use ml_kem::kem::KeyExport;
use pretty_assertions::assert_eq;
use zeroize::Zeroize;

use super::InitiatorHandshake;
use super::MAX_MESSAGE_LEN;
use super::MlKem768;
use super::NOISE_CHANNEL_SUITE;
use super::NoiseChannelError;
use super::NoiseChannelIdentity;
use super::NoiseChannelPublicKey;
use super::PendingResponderHandshake;
use super::STANDARD;
use super::noise_channel_prologue;

#[path = "noise_channel_fips203_vectors.rs"]
mod fips203_vectors;

#[derive(Clone)]
struct VectorRng {
    randomness: [u8; 32],
    offset: usize,
}

impl VectorRng {
    fn new(randomness: [u8; 32]) -> Self {
        Self {
            randomness,
            offset: 0,
        }
    }

    fn take_bytes(&mut self, destination: &mut [u8]) {
        let available = self.randomness.len().saturating_sub(self.offset);
        let copied = available.min(destination.len());
        destination[..copied].copy_from_slice(&self.randomness[self.offset..self.offset + copied]);
        destination[copied..].fill(0);
        self.offset += copied;
    }
}

impl Default for VectorRng {
    fn default() -> Self {
        Self::new([0; 32])
    }
}

impl RngCore for VectorRng {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0; 4];
        self.take_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0; 8];
        self.take_bytes(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn fill_bytes(&mut self, destination: &mut [u8]) {
        self.take_bytes(destination);
    }

    fn try_fill_bytes(&mut self, destination: &mut [u8]) -> Result<(), clatter::rand_core::Error> {
        self.take_bytes(destination);
        Ok(())
    }
}

impl CryptoRng for VectorRng {}

#[test]
fn hybrid_ik_roundtrip_authenticates_both_endpoints() {
    let initiator = NoiseChannelIdentity::generate().expect("generate initiator identity");
    let responder = NoiseChannelIdentity::generate().expect("generate responder identity");
    let prologue = noise_channel_prologue("env-1", "registration-1", "stream-1");
    let authorization = b"harness-key-authorization";

    let (initiator_handshake, request) = InitiatorHandshake::start(
        &initiator,
        &responder.public_key(),
        &prologue,
        authorization,
    )
    .expect("start initiator handshake");
    let responder_handshake =
        PendingResponderHandshake::read_request(&responder, &prologue, &request)
            .expect("read responder handshake");

    assert_eq!(
        &responder_handshake.initiator_public_key,
        &initiator.public_key()
    );
    assert_eq!(responder_handshake.payload.as_slice(), authorization);

    let (mut responder_transport, response) = responder_handshake
        .complete()
        .expect("complete responder handshake");
    let mut initiator_transport = initiator_handshake
        .finish(&response)
        .expect("complete initiator handshake");

    let request_ciphertext = initiator_transport
        .encrypt(b"request")
        .expect("encrypt request");
    assert_ne!(request_ciphertext, b"request");
    assert_eq!(
        responder_transport
            .decrypt(&request_ciphertext)
            .expect("decrypt request"),
        b"request"
    );

    let response_ciphertext = responder_transport
        .encrypt(b"response")
        .expect("encrypt response");
    assert_ne!(response_ciphertext, b"response");
    assert_eq!(
        initiator_transport
            .decrypt(&response_ciphertext)
            .expect("decrypt response"),
        b"response"
    );
}

#[test]
fn initiator_rejects_wrong_responder_key() {
    let initiator = NoiseChannelIdentity::generate().expect("generate initiator identity");
    let expected_responder = NoiseChannelIdentity::generate().expect("generate expected identity");
    let actual_responder = NoiseChannelIdentity::generate().expect("generate actual identity");
    let prologue = noise_channel_prologue("env-1", "registration-1", "stream-1");

    let (_initiator_handshake, request) = InitiatorHandshake::start(
        &initiator,
        &expected_responder.public_key(),
        &prologue,
        b"authorization",
    )
    .expect("start initiator handshake");

    assert!(
        PendingResponderHandshake::read_request(&actual_responder, &prologue, &request).is_err()
    );
}

#[test]
fn responder_rejects_mismatched_prologue() {
    let initiator = NoiseChannelIdentity::generate().expect("generate initiator identity");
    let responder = NoiseChannelIdentity::generate().expect("generate responder identity");
    let initiator_prologue = noise_channel_prologue("env-1", "registration-1", "stream-1");
    let responder_prologue = noise_channel_prologue("env-1", "registration-1", "stream-2");
    let (_initiator_handshake, request) = InitiatorHandshake::start(
        &initiator,
        &responder.public_key(),
        &initiator_prologue,
        b"authorization",
    )
    .expect("start initiator handshake");

    assert!(
        PendingResponderHandshake::read_request(&responder, &responder_prologue, &request).is_err()
    );
}

#[test]
fn prologue_encoding_is_stable_and_unambiguous() {
    let prologue = noise_channel_prologue("env-1", "registration-1", "stream-1");

    assert_eq!(
        prologue,
        b"\x00\x00\x00\x00\x00\x00\x00\x20codex-exec-server-relay-noise/v1\
          \x00\x00\x00\x00\x00\x00\x00\x05env-1\
          \x00\x00\x00\x00\x00\x00\x00\x0eregistration-1\
          \x00\x00\x00\x00\x00\x00\x00\x08stream-1"
            .to_vec()
    );
}

#[test]
fn transport_rejects_tampered_ciphertext() {
    let initiator = NoiseChannelIdentity::generate().expect("generate initiator identity");
    let responder = NoiseChannelIdentity::generate().expect("generate responder identity");
    let prologue = noise_channel_prologue("env-1", "registration-1", "stream-1");
    let (initiator_handshake, request) = InitiatorHandshake::start(
        &initiator,
        &responder.public_key(),
        &prologue,
        b"authorization",
    )
    .expect("start initiator handshake");
    let responder_handshake =
        PendingResponderHandshake::read_request(&responder, &prologue, &request)
            .expect("read responder handshake");
    let (mut responder_transport, response) = responder_handshake
        .complete()
        .expect("complete responder handshake");
    let mut initiator_transport = initiator_handshake
        .finish(&response)
        .expect("complete initiator handshake");
    let mut ciphertext = initiator_transport
        .encrypt(b"request")
        .expect("encrypt request");
    ciphertext[0] ^= 1;

    assert!(responder_transport.decrypt(&ciphertext).is_err());
}

#[test]
fn transport_rejects_replayed_ciphertext() {
    let initiator = NoiseChannelIdentity::generate().expect("generate initiator identity");
    let responder = NoiseChannelIdentity::generate().expect("generate responder identity");
    let prologue = noise_channel_prologue("env-1", "registration-1", "stream-1");
    let (initiator_handshake, request) = InitiatorHandshake::start(
        &initiator,
        &responder.public_key(),
        &prologue,
        b"authorization",
    )
    .expect("start initiator handshake");
    let responder_handshake =
        PendingResponderHandshake::read_request(&responder, &prologue, &request)
            .expect("read responder handshake");
    let (mut responder_transport, response) = responder_handshake
        .complete()
        .expect("complete responder handshake");
    let mut initiator_transport = initiator_handshake
        .finish(&response)
        .expect("complete initiator handshake");
    let ciphertext = initiator_transport
        .encrypt(b"request")
        .expect("encrypt request");

    assert_eq!(
        responder_transport
            .decrypt(&ciphertext)
            .expect("decrypt request"),
        b"request"
    );
    assert!(matches!(
        responder_transport.decrypt(&ciphertext),
        Err(NoiseChannelError::Transport(_))
    ));
}

#[test]
fn public_key_validation_rejects_unknown_suite() {
    let key = NoiseChannelIdentity::generate()
        .expect("generate identity")
        .public_key();
    let json = serde_json::to_value(key).expect("serialize key");
    let mut object = json.as_object().expect("key object").clone();
    object.insert("suite".to_string(), serde_json::json!("unknown"));
    let key: NoiseChannelPublicKey =
        serde_json::from_value(serde_json::Value::Object(object)).expect("deserialize key");

    let initiator = NoiseChannelIdentity::generate().expect("generate initiator identity");
    assert!(InitiatorHandshake::start(&initiator, &key, b"prologue", b"").is_err());
}

#[test]
fn public_key_serializes_with_expected_suite() {
    let key = NoiseChannelIdentity::generate()
        .expect("generate identity")
        .public_key();

    let json = serde_json::to_value(key).expect("serialize key");

    assert_eq!(json["suite"], NOISE_CHANNEL_SUITE);
    let encoded_mlkem_key = STANDARD
        .decode(
            json["mlkem768_public_key"]
                .as_str()
                .expect("ML-KEM-768 key string"),
        )
        .expect("base64 ML-KEM-768 key");
    assert_eq!(encoded_mlkem_key.len(), 1184);
}

#[allow(deprecated)]
#[test]
fn ml_kem_768_matches_nist_fips203_acvp_vector() {
    let public_key: [u8; 1184] = STANDARD
        .decode(fips203_vectors::EK)
        .expect("decode NIST ML-KEM-768 encapsulation key")
        .as_slice()
        .try_into()
        .expect("NIST ML-KEM-768 encapsulation key length");
    let expected_ciphertext: [u8; 1088] = STANDARD
        .decode(fips203_vectors::C)
        .expect("decode NIST ML-KEM-768 ciphertext")
        .as_slice()
        .try_into()
        .expect("NIST ML-KEM-768 ciphertext length");
    let expected_shared_secret: [u8; 32] = STANDARD
        .decode(fips203_vectors::K)
        .expect("decode NIST ML-KEM-768 shared secret")
        .as_slice()
        .try_into()
        .expect("NIST ML-KEM-768 shared-secret length");
    let randomness: [u8; 32] = STANDARD
        .decode(fips203_vectors::M)
        .expect("decode NIST ML-KEM-768 encapsulation randomness")
        .as_slice()
        .try_into()
        .expect("NIST ML-KEM-768 randomness length");
    let mut rng = VectorRng::new(randomness);

    let (ciphertext, shared_secret) =
        MlKem768::encapsulate(&public_key, &mut rng).expect("encapsulate NIST vector key");

    assert_eq!(rng.offset, randomness.len());
    assert_eq!(ciphertext.as_slice(), expected_ciphertext.as_slice());
    assert_eq!(shared_secret.as_slice(), expected_shared_secret.as_slice());

    let mut expanded_key_bytes = STANDARD
        .decode(fips203_vectors::DK)
        .expect("decode NIST ML-KEM-768 expanded decapsulation key");
    let mut expanded_key: ml_kem::ml_kem_768::ExpandedDecapsulationKey = expanded_key_bytes
        .as_slice()
        .try_into()
        .expect("NIST ML-KEM-768 expanded-key length");
    expanded_key_bytes.zeroize();
    let decapsulation_key =
        ml_kem::ml_kem_768::DecapsulationKey::from_expanded_bytes(&expanded_key)
            .expect("parse NIST ML-KEM-768 expanded decapsulation key");
    expanded_key.zeroize();

    assert_eq!(
        decapsulation_key.encapsulation_key().to_bytes().as_slice(),
        public_key.as_slice()
    );
    let decapsulated_shared_secret =
        MlKem768::decapsulate_with_key(ciphertext.as_slice(), &decapsulation_key)
            .expect("decapsulate NIST vector ciphertext");
    assert_eq!(
        decapsulated_shared_secret.as_slice(),
        expected_shared_secret.as_slice()
    );
}

#[test]
fn initiator_rejects_oversized_handshake_payload() {
    let initiator = NoiseChannelIdentity::generate().expect("generate initiator identity");
    let responder = NoiseChannelIdentity::generate().expect("generate responder identity");
    let payload = vec![0; MAX_MESSAGE_LEN];

    let result =
        InitiatorHandshake::start(&initiator, &responder.public_key(), b"prologue", &payload);

    assert!(matches!(
        result,
        Err(NoiseChannelError::InvalidMessage(
            "handshake payload is too large"
        ))
    ));
}
