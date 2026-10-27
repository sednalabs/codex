//! Noise channel used by the remote exec-server relay.
//!
//! The harness initiates hybrid IK and pins the exec-server static key returned
//! by the registry. The first handshake message lets the exec-server authenticate
//! the harness static key; the exec-server then asks the registry whether that
//! key is authorized before completing the handshake.
//!
//! "Hybrid" means the session keys include both X25519 and ML-KEM-768 key
//! agreement. Once the two-message handshake finishes, AES-GCM protects the
//! ordered transport records carrying JSON-RPC.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use clatter::HybridHandshake;
use clatter::HybridHandshakeParams;
use clatter::KeyPair;
use clatter::bytearray::ByteArray;
use clatter::bytearray::SensitiveByteArray;
use clatter::constants::MAX_MESSAGE_LEN;
use clatter::crypto::cipher::AesGcm;
use clatter::crypto::dh::X25519;
use clatter::crypto::hash::Sha256;
use clatter::error::KemError;
use clatter::handshakepattern::noise_hybrid_ik;
use clatter::traits::Cipher;
use clatter::traits::CryptoComponent;
use clatter::traits::Dh;
use clatter::traits::Handshaker;
use clatter::traits::Kem;
use clatter::traits::Rng;
use clatter::traits::RngCore;
use clatter::transportstate::TransportState;
use ml_kem::Key;
use ml_kem::Seed;
use ml_kem::kem::Decapsulate;
use ml_kem::kem::Encapsulate;
use ml_kem::kem::KeyExport;
use serde::Deserialize;
use serde::Serialize;
use zeroize::Zeroize;

/// Identifies the handshake pattern and algorithms used by this channel.
pub(crate) const NOISE_CHANNEL_SUITE: &str = "Noise_hybridIK_X25519+MLKEM768_AESGCM_SHA256";

const PROLOGUE_DOMAIN: &[u8] = b"codex-exec-server-relay-noise/v1";

// Keep the Noise suite on Clatter's KEM interface while using RustCrypto's stable ML-KEM.
#[derive(Clone)]
struct MlKem768;

impl MlKem768 {
    fn decapsulate_with_key(
        ciphertext: &[u8],
        decapsulation_key: &ml_kem::ml_kem_768::DecapsulationKey,
    ) -> clatter::error::KemResult<<Self as Kem>::Ss> {
        let ciphertext: ml_kem::ml_kem_768::Ciphertext =
            ciphertext.try_into().map_err(|_| KemError::Input)?;
        let mut shared_secret = decapsulation_key.decapsulate(&ciphertext);
        let result = <Self as Kem>::Ss::from_slice(shared_secret.as_slice());
        shared_secret.zeroize();
        Ok(result)
    }
}

impl CryptoComponent for MlKem768 {
    fn name() -> &'static str {
        "MLKEM768"
    }
}

impl Kem for MlKem768 {
    type SecretKey = SensitiveByteArray<[u8; 64]>;
    type PubKey = [u8; 1184];
    type Ct = [u8; 1088];
    type Ss = SensitiveByteArray<[u8; 32]>;

    fn genkey_rng<R: Rng>(
        rng: &mut R,
    ) -> clatter::error::KemResult<KeyPair<Self::PubKey, Self::SecretKey>> {
        let mut seed_bytes = [0u8; 64];
        rng.fill_bytes(&mut seed_bytes);
        let mut seed = Seed::from(seed_bytes);
        let decapsulation_key = ml_kem::ml_kem_768::DecapsulationKey::from_seed(seed);
        seed.zeroize();
        let public_key = decapsulation_key.encapsulation_key().to_bytes();
        let key_pair = KeyPair {
            public: Self::PubKey::from_slice(public_key.as_slice()),
            secret: Self::SecretKey::from_slice(&seed_bytes),
        };
        seed_bytes.zeroize();
        Ok(key_pair)
    }

    fn encapsulate<R: Rng>(
        public_key: &[u8],
        rng: &mut R,
    ) -> clatter::error::KemResult<(Self::Ct, Self::Ss)> {
        let encoded_public_key: Key<ml_kem::ml_kem_768::EncapsulationKey> =
            public_key.try_into().map_err(|_| KemError::Input)?;
        let encapsulation_key = ml_kem::ml_kem_768::EncapsulationKey::new(&encoded_public_key)
            .map_err(|_| KemError::Input)?;
        let (ciphertext, mut shared_secret) =
            encapsulation_key.encapsulate_with_rng(&mut MlKemRngAdapter(rng));
        let result = (
            Self::Ct::from_slice(ciphertext.as_slice()),
            Self::Ss::from_slice(shared_secret.as_slice()),
        );
        shared_secret.zeroize();
        Ok(result)
    }

    fn decapsulate(ciphertext: &[u8], secret_key: &[u8]) -> clatter::error::KemResult<Self::Ss> {
        let mut seed: Seed = secret_key.try_into().map_err(|_| KemError::Input)?;
        let decapsulation_key = ml_kem::ml_kem_768::DecapsulationKey::from_seed(seed);
        seed.zeroize();
        Self::decapsulate_with_key(ciphertext, &decapsulation_key)
    }
}

struct MlKemRngAdapter<'a, R>(&'a mut R);

// Clatter's RNG trait uses rand_core 0.6; stable RustCrypto ML-KEM uses rand_core 0.10.
impl<R: Rng> rand_core::TryRng for MlKemRngAdapter<'_, R> {
    type Error = std::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, std::convert::Infallible> {
        Ok(RngCore::next_u32(self.0))
    }

    fn try_next_u64(&mut self) -> Result<u64, std::convert::Infallible> {
        Ok(RngCore::next_u64(self.0))
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), std::convert::Infallible> {
        RngCore::fill_bytes(self.0, dest);
        Ok(())
    }
}

impl<R: Rng> rand_core::TryCryptoRng for MlKemRngAdapter<'_, R> {}

type Handshake = HybridHandshake<X25519, MlKem768, MlKem768, AesGcm, Sha256>;
type Transport = TransportState<AesGcm, Sha256>;
type DhKeyPair = KeyPair<<X25519 as Dh>::PubKey, <X25519 as Dh>::PrivateKey>;
type MlKem768PublicKey = <MlKem768 as Kem>::PubKey;
type KemKeyPair = KeyPair<<MlKem768 as Kem>::PubKey, <MlKem768 as Kem>::SecretKey>;

/// Public key material for the exec-server Noise suite.
/// The suite tag prevents keys for another protocol from being accepted just
/// because their components have the expected lengths.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoiseChannelPublicKey {
    suite: String,
    x25519_public_key: String,
    mlkem768_public_key: String,
}

impl std::fmt::Debug for NoiseChannelPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoiseChannelPublicKey")
            .field("suite", &self.suite)
            .field("x25519_public_key", &"<redacted>")
            .field("mlkem768_public_key", &"<redacted>")
            .finish()
    }
}

impl NoiseChannelPublicKey {
    /// Decode registry-provided key material before passing it to Clatter.
    fn decode(&self) -> Result<(<X25519 as Dh>::PubKey, MlKem768PublicKey), NoiseChannelError> {
        if self.suite != NOISE_CHANNEL_SUITE {
            return Err(NoiseChannelError::InvalidPublicKey(
                "unsupported Noise channel suite",
            ));
        }
        let dh = STANDARD
            .decode(&self.x25519_public_key)
            .map_err(|_| NoiseChannelError::InvalidPublicKey("invalid X25519 public key"))?;
        let dh: <X25519 as Dh>::PubKey = dh
            .try_into()
            .map_err(|_| NoiseChannelError::InvalidPublicKey("invalid X25519 public key length"))?;
        let kem = STANDARD
            .decode(&self.mlkem768_public_key)
            .map_err(|_| NoiseChannelError::InvalidPublicKey("invalid ML-KEM-768 public key"))?;
        if kem.len() != MlKem768PublicKey::LENGTH {
            return Err(NoiseChannelError::InvalidPublicKey(
                "invalid ML-KEM-768 public key length",
            ));
        }

        Ok((dh, MlKem768PublicKey::from_slice(kem.as_slice())))
    }
}

/// Static Noise identity kept for the lifetime of an executor or harness process.
#[derive(Clone)]
pub struct NoiseChannelIdentity {
    dh: DhKeyPair,
    kem: KemKeyPair,
}

impl NoiseChannelIdentity {
    pub fn generate() -> Result<Self, NoiseChannelError> {
        let dh = X25519::genkey()
            .map_err(|error| NoiseChannelError::KeyGeneration(error.to_string()))?;
        let kem = MlKem768::genkey()
            .map_err(|error| NoiseChannelError::KeyGeneration(error.to_string()))?;
        Ok(Self { dh, kem })
    }

    pub fn public_key(&self) -> NoiseChannelPublicKey {
        NoiseChannelPublicKey {
            suite: NOISE_CHANNEL_SUITE.to_string(),
            x25519_public_key: STANDARD.encode(self.dh.public),
            mlkem768_public_key: STANDARD.encode(self.kem.public.as_slice()),
        }
    }
}

/// Harness-side state between the two hybrid-IK messages.
/// Consuming it in [`Self::finish`] keeps a handshake tied to one relay stream.
pub(crate) struct InitiatorHandshake {
    handshake: Handshake,
}

impl InitiatorHandshake {
    /// Start hybrid IK and pin the expected executor key.
    /// `payload` carries the short-lived registry authorization inside the first
    /// encrypted handshake message.
    pub(crate) fn start(
        identity: &NoiseChannelIdentity,
        responder_public_key: &NoiseChannelPublicKey,
        prologue: &[u8],
        payload: &[u8],
    ) -> Result<(Self, Vec<u8>), NoiseChannelError> {
        let (responder_dh, responder_kem) = responder_public_key.decode()?;

        // Both executor key components are pinned before any JSON-RPC is sent.
        let params = HybridHandshakeParams::new(noise_hybrid_ik(), true)
            .with_prologue(prologue)
            .with_s(identity.dh.clone())
            .with_s_kem(identity.kem.clone())
            .with_rs(responder_dh)
            .with_rs_kem(responder_kem);
        let mut handshake = Handshake::new(params)?;
        let overhead = handshake.get_next_message_overhead()?;
        if payload.len() > MAX_MESSAGE_LEN - overhead {
            return Err(NoiseChannelError::InvalidMessage(
                "handshake payload is too large",
            ));
        }
        let mut output = vec![0u8; payload.len() + overhead];
        let output_len = handshake.write_message(payload, &mut output)?;
        output.truncate(output_len);
        Ok((Self { handshake }, output))
    }

    /// Consume the executor response and enter transport mode.
    /// The v1 response does not carry an application payload.
    pub(crate) fn finish(mut self, response: &[u8]) -> Result<NoiseTransport, NoiseChannelError> {
        ensure_noise_frame_len(response.len(), "handshake response is too large")?;
        let overhead = self.handshake.get_next_message_overhead()?;
        let mut payload = vec![0u8; response.len().saturating_sub(overhead)];
        let payload_len = self.handshake.read_message(response, &mut payload)?;
        if payload_len != 0 {
            return Err(NoiseChannelError::InvalidMessage(
                "handshake response payload must be empty",
            ));
        }
        Ok(NoiseTransport {
            transport: self.handshake.finalize()?,
        })
    }
}

/// Executor-side handshake state while harness authorization is pending.
/// This is not a usable transport until the registry accepts the authenticated
/// harness key.
pub(crate) struct PendingResponderHandshake {
    handshake: Handshake,
    pub(crate) initiator_public_key: NoiseChannelPublicKey,
    pub(crate) payload: Vec<u8>,
}

impl PendingResponderHandshake {
    /// Parse the first IK message and recover the authenticated harness key.
    /// Callers must authorize that key before calling [`Self::complete`].
    pub(crate) fn read_request(
        identity: &NoiseChannelIdentity,
        prologue: &[u8],
        request: &[u8],
    ) -> Result<Self, NoiseChannelError> {
        ensure_noise_frame_len(request.len(), "handshake request is too large")?;
        let params = HybridHandshakeParams::new(noise_hybrid_ik(), false)
            .with_prologue(prologue)
            .with_s(identity.dh.clone())
            .with_s_kem(identity.kem.clone());
        let mut handshake = Handshake::new(params)?;
        let overhead = handshake.get_next_message_overhead()?;
        let mut payload = vec![0u8; request.len().saturating_sub(overhead)];
        let payload_len = handshake.read_message(request, &mut payload)?;
        // Clatter exposes this key only after the first IK message authenticates.
        let remote = handshake
            .get_remote_static()
            .ok_or(NoiseChannelError::InvalidMessage(
                "handshake request is missing initiator static key",
            ))?;
        let initiator_public_key = NoiseChannelPublicKey {
            suite: NOISE_CHANNEL_SUITE.to_string(),
            x25519_public_key: STANDARD.encode(remote.dh()),
            mlkem768_public_key: STANDARD.encode(remote.kem().as_slice()),
        };
        payload.truncate(payload_len);
        Ok(Self {
            handshake,
            initiator_public_key,
            payload,
        })
    }

    /// Finish the handshake after the registry authorizes the harness key.
    pub(crate) fn complete(mut self) -> Result<(NoiseTransport, Vec<u8>), NoiseChannelError> {
        let overhead = self.handshake.get_next_message_overhead()?;
        let mut response = vec![0u8; overhead];
        let response_len = self.handshake.write_message(&[], &mut response)?;
        response.truncate(response_len);
        Ok((
            NoiseTransport {
                transport: self.handshake.finalize()?,
            },
            response,
        ))
    }
}

/// Established channel with independent implicit send and receive nonces.
/// Relay records must be ordered before decryption, and a logical record must
/// not be encrypted again for retry.
pub(crate) struct NoiseTransport {
    transport: Transport,
}

impl NoiseTransport {
    /// Encrypt the next transport record.
    pub(crate) fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, NoiseChannelError> {
        let frame_len = plaintext.len().checked_add(AesGcm::tag_len()).ok_or(
            NoiseChannelError::InvalidMessage("transport plaintext is too large"),
        )?;
        ensure_noise_frame_len(frame_len, "transport plaintext is too large")?;
        Ok(self.transport.send_vec(plaintext)?)
    }

    /// Decrypt the next ordered transport record.
    pub(crate) fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseChannelError> {
        if ciphertext.len() < AesGcm::tag_len() {
            return Err(NoiseChannelError::InvalidMessage(
                "transport ciphertext is too short",
            ));
        }
        ensure_noise_frame_len(ciphertext.len(), "transport ciphertext is too large")?;
        Ok(self.transport.receive_vec(ciphertext)?)
    }
}

/// Bind the handshake to one environment registration and relay stream.
/// Both peers include these values in the Noise transcript before processing
/// the first handshake message.
pub(crate) fn noise_channel_prologue(
    environment_id: &str,
    executor_registration_id: &str,
    stream_id: &str,
) -> Vec<u8> {
    let mut prologue = Vec::new();
    append_prologue_part(&mut prologue, PROLOGUE_DOMAIN);
    append_prologue_part(&mut prologue, environment_id.as_bytes());
    append_prologue_part(&mut prologue, executor_registration_id.as_bytes());
    append_prologue_part(&mut prologue, stream_id.as_bytes());
    prologue
}

fn append_prologue_part(prologue: &mut Vec<u8>, part: &[u8]) {
    // Length prefixes make component boundaries unambiguous. Raw concatenation
    // would allow different identifier tuples to produce the same prologue.
    let len = part.len() as u64;
    prologue.extend_from_slice(&len.to_be_bytes());
    prologue.extend_from_slice(part);
}

fn ensure_noise_frame_len(
    frame_len: usize,
    message: &'static str,
) -> Result<(), NoiseChannelError> {
    if frame_len > MAX_MESSAGE_LEN {
        return Err(NoiseChannelError::InvalidMessage(message));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum NoiseChannelError {
    #[error("Noise channel key generation failed: {0}")]
    KeyGeneration(String),
    #[error("invalid Noise channel public key: {0}")]
    InvalidPublicKey(&'static str),
    #[error("invalid Noise channel message: {0}")]
    InvalidMessage(&'static str),
    #[error("Noise channel handshake failed: {0}")]
    Handshake(String),
    #[error("Noise channel transport failed: {0}")]
    Transport(String),
}

impl From<clatter::error::HandshakeError> for NoiseChannelError {
    fn from(error: clatter::error::HandshakeError) -> Self {
        Self::Handshake(error.to_string())
    }
}

impl From<clatter::error::TransportError> for NoiseChannelError {
    fn from(error: clatter::error::TransportError) -> Self {
        Self::Transport(error.to_string())
    }
}

#[cfg(test)]
#[path = "noise_channel_tests.rs"]
mod tests;
