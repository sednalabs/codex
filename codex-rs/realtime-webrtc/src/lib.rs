<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
#[cfg(target_os = "macos")]
mod native;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicU16;
use std::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum RealtimeWebrtcError {
    #[error("{0}")]
    Message(String),
    #[error("realtime WebRTC is not supported on this platform")]
    UnsupportedPlatform,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeWebrtcEvent {
    Connected,
    LocalAudioLevel(u16),
    Closed,
    Failed(String),
}

pub type Result<T> = std::result::Result<T, RealtimeWebrtcError>;

pub struct StartedRealtimeWebrtcSession {
    pub offer_sdp: String,
    pub handle: RealtimeWebrtcSessionHandle,
    pub events: mpsc::Receiver<RealtimeWebrtcEvent>,
}

pub struct RealtimeWebrtcSessionHandle {
    #[cfg(target_os = "macos")]
    inner: native::SessionHandle,
    local_audio_peak: Arc<AtomicU16>,
}

impl fmt::Debug for RealtimeWebrtcSessionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealtimeWebrtcSessionHandle")
            .finish_non_exhaustive()
    }
}

impl RealtimeWebrtcSessionHandle {
    pub fn apply_answer_sdp(&self, answer_sdp: String) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            self.inner.apply_answer_sdp(answer_sdp)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = answer_sdp;
            Err(RealtimeWebrtcError::UnsupportedPlatform)
        }
    }

    pub fn close(&self) {
        #[cfg(target_os = "macos")]
        self.inner.close();
    }

    pub fn local_audio_peak(&self) -> Arc<AtomicU16> {
        self.local_audio_peak.clone()
    }
}

pub struct RealtimeWebrtcSession;

impl RealtimeWebrtcSession {
    pub fn start() -> Result<StartedRealtimeWebrtcSession> {
        #[cfg(target_os = "macos")]
        {
            let started = native::start()?;
            Ok(StartedRealtimeWebrtcSession {
                offer_sdp: started.offer_sdp,
                handle: RealtimeWebrtcSessionHandle {
                    inner: started.handle,
                    local_audio_peak: Arc::new(AtomicU16::new(0)),
                },
                events: started.events,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(RealtimeWebrtcError::UnsupportedPlatform)
        }
    }
}
=======
mod client;
mod helper_exit;
#[cfg(any(target_os = "linux", test))]
mod linux_alsa;
mod message_reader;
mod protocol;
mod session;

pub use client::ConnectionError;
pub use client::VoiceHost;
pub use helper_exit::HelperExitStage;
pub use protocol::AudioControls;
pub use protocol::AudioState;
pub use protocol::MAX_FRAME_BYTES;
pub use protocol::Message;
pub use protocol::RUNTIME_ENVIRONMENT;
pub use protocol::SessionDescription;
pub use protocol::decode_frame;
pub use protocol::encode_frame;
pub use protocol::read_message;
pub use session::RealtimeWebrtcSession;
pub use session::RealtimeWebrtcSessionHandle;
pub use session::StartedRealtimeWebrtcSession;
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
