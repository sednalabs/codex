//! Live voice capture controls, activity meters, and interruption presentation.
//! Meter samples remain bounded and the current thread owns every shortcut.

use super::*;

impl ChatWidget {
    pub(in crate::chatwidget) fn toggle_realtime_microphone(&mut self) {
        let muted = !self.realtime_conversation.microphone_muted;
        if let Some(handle) = self.realtime_conversation.handle.as_ref() {
            if let Err(error) = handle.set_microphone_muted(muted) {
                self.on_realtime_error(format!("Failed to update microphone: {error}"));
                return;
            }
        } else if self.realtime_conversation.phase != RealtimeConversationPhase::Starting {
            self.add_error_message("Start voice mode before muting the microphone.".to_string());
            return;
        }

        self.realtime_conversation.microphone_muted = muted;
        self.realtime_conversation.microphone_level = 0;
        if muted {
            self.realtime_conversation.microphone_history = VoiceAmplitudeHistory::default();
            self.realtime_conversation.audio_meter_history.clear();
            self.realtime_conversation.interruption_acknowledged_until = None;
        }
        self.update_realtime_footer();
        self.refresh_terminal_title();
    }

    pub(in crate::chatwidget) fn handle_realtime_microphone_shortcut(
        &mut self,
        key_event: KeyEvent,
    ) -> bool {
        if key_event.kind != KeyEventKind::Press
            || !REALTIME_MICROPHONE_SHORTCUT.is_press(key_event)
            || self.realtime_conversation.phase != RealtimeConversationPhase::Active
            || self.realtime_conversation.thread_id.is_none()
            || self.realtime_conversation.thread_id != self.thread_id()
            || !self.bottom_pane.no_modal_or_popup_active()
        {
            return false;
        }

        self.toggle_realtime_microphone();
        true
    }

    pub(in crate::chatwidget) fn realtime_microphone_is_listening(&self) -> bool {
        !self.realtime_conversation.microphone_muted
            && self.realtime_conversation.thread_id.is_some()
            && self.realtime_conversation.thread_id == self.thread_id()
            && match self.realtime_conversation.phase {
                RealtimeConversationPhase::Active => true,
                RealtimeConversationPhase::Starting => self.realtime_conversation.handle.is_some(),
                RealtimeConversationPhase::Inactive | RealtimeConversationPhase::Stopping => false,
            }
    }

    pub(super) fn release_realtime_speaker(&mut self) {
        let acknowledged = self
            .realtime_conversation
            .interruption_acknowledged_until
            .take()
            .is_some();
        self.realtime_conversation.speaker_suppression_generation = None;
        if let Some(handle) = self.realtime_conversation.handle.as_ref() {
            handle.set_speaker_suppressed(/*suppressed*/ false);
        }
        if acknowledged {
            self.update_realtime_footer();
        }
    }

    pub(super) fn suppress_realtime_speaker(&mut self) {
        self.realtime_conversation.speaker_suppression_generation =
            Some(self.realtime_conversation.input_generation);
        if let Some(handle) = self.realtime_conversation.handle.as_ref() {
            handle.set_speaker_suppressed(/*suppressed*/ true);
        }
        self.realtime_conversation.speaker_level = 0;
        self.realtime_conversation.speaker_active_until = None;
        self.update_realtime_footer();
    }

    pub(super) fn resume_realtime_speaker_for(&mut self, role: &str, text: &str) {
        if role == "assistant"
            && !text.trim().is_empty()
            && self.realtime_conversation.assistant_transcript_generation
                == Some(self.realtime_conversation.input_generation)
            && self.realtime_conversation.latest_input_was_voice
            && self.realtime_conversation.speaker_suppression_generation
                == Some(self.realtime_conversation.input_generation)
        {
            self.release_realtime_speaker();
        }
    }

    pub(in crate::chatwidget) fn refresh_realtime_microphone_level(&mut self) {
        if !matches!(
            self.realtime_conversation.phase,
            RealtimeConversationPhase::Starting | RealtimeConversationPhase::Active
        ) {
            return;
        }
        let Some(handle) = self.realtime_conversation.handle.as_ref() else {
            return;
        };
        if let Some(error) = handle.take_error() {
            self.on_realtime_error(format!("Voice conversation failed: {error}"));
            return;
        }
        if self.realtime_conversation.phase == RealtimeConversationPhase::Starting {
            self.frame_requester
                .schedule_frame_in(MICROPHONE_METER_INTERVAL);
            return;
        }
        let microphone_level = if self.realtime_conversation.microphone_muted {
            0
        } else {
            audio_meter_level(handle.take_microphone_peak())
        };
        let speaker_level = audio_meter_level(handle.take_speaker_peak());
        let previous_microphone_history = self.realtime_conversation.microphone_history;
        let previous_speaker_history = self.realtime_conversation.speaker_history;
        let mut changed = self.realtime_conversation.microphone_level != microphone_level
            || self.realtime_conversation.speaker_level != speaker_level;
        if speaker_level > 0 {
            self.realtime_conversation.speaker_active_until =
                Some(Instant::now() + SPEAKER_ACTIVITY_HOLD);
        } else {
            changed |= self
                .realtime_conversation
                .speaker_active_until
                .take_if(|deadline| *deadline <= Instant::now())
                .is_some();
        }
        changed |= self
            .realtime_conversation
            .interruption_acknowledged_until
            .take_if(|deadline| *deadline <= Instant::now())
            .is_some();
        self.realtime_conversation.microphone_level = microphone_level;
        self.realtime_conversation.speaker_level = speaker_level;
        self.realtime_conversation
            .microphone_history
            .push(microphone_level);
        self.realtime_conversation
            .speaker_history
            .push(speaker_level);
        if self.realtime_conversation.audio_meter_history.len() >= MAX_REALTIME_AUDIO_METER_FRAMES {
            self.realtime_conversation.audio_meter_history.pop_front();
        }
        self.realtime_conversation.audio_meter_history.push_back((
            self.realtime_conversation.microphone_history,
            self.realtime_conversation.speaker_history,
        ));
        changed |= previous_microphone_history != self.realtime_conversation.microphone_history
            || previous_speaker_history != self.realtime_conversation.speaker_history;
        if changed {
            self.update_realtime_footer();
        }
        self.frame_requester
            .schedule_frame_in(MICROPHONE_METER_INTERVAL);
    }

    pub(super) fn update_realtime_footer(&mut self) {
        let status = if self.realtime_conversation.phase == RealtimeConversationPhase::Starting {
            "◌ connecting"
        } else if self.realtime_conversation.microphone_muted {
            "◌ muted"
        } else if self
            .realtime_conversation
            .interruption_acknowledged_until
            .is_some_and(|deadline| deadline > Instant::now())
        {
            "● heard"
        } else if self.realtime_conversation.speaker_level > 0
            || self
                .realtime_conversation
                .speaker_active_until
                .is_some_and(|deadline| deadline > Instant::now())
        {
            "● speaking"
        } else {
            "● listening"
        };
        let mut hints = vec![
            ("voice".to_string(), status.to_string()),
            ("/voice".to_string(), "stop".to_string()),
        ];
        if self.realtime_conversation.phase == RealtimeConversationPhase::Active {
            let microphone = if self.realtime_conversation.microphone_muted {
                "off".to_string()
            } else {
                audio_meter_braille(self.realtime_conversation.microphone_history)
            };
            let speaker = audio_meter_braille(self.realtime_conversation.speaker_history);
            hints.push(("mic".to_string(), format!("{microphone}  codex {speaker}")));
        }
        let mute_shortcut = REALTIME_MICROPHONE_SHORTCUT
            .display_label()
            .replace(" + ", "+");
        hints.push(("/voice".to_string(), format!("mute {mute_shortcut}")));
        self.set_footer_hint_override(Some(hints));
    }
}

pub(super) fn audio_meter_level(peak: u16) -> usize {
    usize::from(peak.saturating_sub(AUDIO_METER_NOISE_FLOOR))
        .saturating_mul(AUDIO_METER_SEGMENTS)
        .div_ceil(usize::from(
            AUDIO_METER_FULL_SCALE - AUDIO_METER_NOISE_FLOOR,
        ))
        .min(AUDIO_METER_SEGMENTS)
}

fn audio_meter_braille(samples: VoiceAmplitudeHistory) -> String {
    if samples.peak() == 0 {
        "⠄⠄".to_string()
    } else {
        samples.glyphs()
    }
}
