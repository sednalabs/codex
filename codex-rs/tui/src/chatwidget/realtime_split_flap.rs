//! Bounded real amplitude histories for voice recording controls.

#[cfg(test)]
#[path = "realtime_split_flap_tests.rs"]
mod tests;

/// Four bounded, calibrated amplitude samples; this is not frequency analysis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct VoiceAmplitudeHistory([usize; 4]);

impl VoiceAmplitudeHistory {
    pub(super) fn push(&mut self, level: usize) {
        self.0.rotate_left(/*mid*/ 1);
        self.0[3] = level.min(/*other*/ 5);
    }

    pub(super) fn glyphs(&self) -> String {
        const LEFT_DOTS: [u32; 6] = [0, 0x40, 0x44, 0x46, 0x47, 0x47];
        const RIGHT_DOTS: [u32; 6] = [0, 0x80, 0xa0, 0xb0, 0xb8, 0xb8];

        self.0
            .chunks_exact(/*chunk_size*/ 2)
            .map(|levels| {
                char::from_u32(0x2800 | LEFT_DOTS[levels[0]] | RIGHT_DOTS[levels[1]])
                    .unwrap_or('\u{2800}')
            })
            .collect()
    }

    pub(super) fn peak(&self) -> usize {
        self.0.iter().copied().max().unwrap_or_default()
    }
}
