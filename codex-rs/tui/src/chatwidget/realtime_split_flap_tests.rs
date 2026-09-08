//! Regressions for bounded state used by realtime voice presentation.
//! Retained history must preserve ordering without growing beyond its limits.

use super::VoiceAmplitudeHistory;
use pretty_assertions::assert_eq;

#[test]
fn voice_meter_shows_four_real_amplitude_samples_in_two_braille_cells() {
    let samples = VoiceAmplitudeHistory([1, 4, 2, 5]);
    insta::assert_snapshot!(samples.glyphs(), @"⣸⣼");
    assert_eq!(samples.peak(), 5);
    assert_eq!(VoiceAmplitudeHistory::default().peak(), 0);
}
