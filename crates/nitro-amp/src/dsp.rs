//! The signal path between the decoder and the output: the ten-band
//! equaliser, volume and balance — and, on the side, the analysis the
//! visualiser draws.
//!
//! All of it is plain arithmetic on interleaved stereo `f32`, and all of
//! it runs on the audio thread (see [`crate::engine`]); nothing here
//! knows about a widget or a process.

use std::f32::consts::PI;

/// The equaliser's band centres, in Hz — Winamp's own ten.
pub const EQ_FREQS: [f32; 10] = [
    60.0, 170.0, 310.0, 600.0, 1_000.0, 3_000.0, 6_000.0, 12_000.0, 14_000.0, 16_000.0,
];

/// Short labels for the bands, as the equaliser prints them.
pub const EQ_LABELS: [&str; 10] = [
    "60", "170", "310", "600", "1K", "3K", "6K", "12K", "14K", "16K",
];

/// The range of a band and of the preamp, in dB either side of flat.
pub const EQ_RANGE_DB: f32 = 12.0;

/// How wide each band is. About an octave and a half: wide enough that
/// adjacent bands overlap into a smooth curve rather than ten notches,
/// which is what a graphic equaliser with this spacing wants.
const EQ_Q: f32 = 1.0;

/// The equaliser's settings, as the UI owns them and the audio thread
/// receives them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqSettings {
    /// Whether the equaliser is in the path at all. Off is a true
    /// bypass, not ten flat filters: nothing is computed.
    pub enabled: bool,
    /// Gain applied before the bands, in dB.
    pub preamp: f32,
    /// Each band's gain, in dB.
    pub bands: [f32; 10],
}

impl Default for EqSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            preamp: 0.0,
            bands: [0.0; 10],
        }
    }
}

/// A named equaliser curve.
#[derive(Debug, Clone, Copy)]
pub struct Preset {
    /// What the preset button shows.
    pub name: &'static str,
    /// Band gains, in dB.
    pub bands: [f32; 10],
}

/// A handful of the curves a Winamp user expects to find, written down
/// from their shapes rather than copied from anyone's preset file.
pub const PRESETS: [Preset; 6] = [
    Preset {
        name: "Flat",
        bands: [0.0; 10],
    },
    Preset {
        name: "Rock",
        bands: [5.0, 3.0, -2.0, -4.0, -1.5, 2.0, 5.0, 6.5, 6.5, 6.5],
    },
    Preset {
        name: "Pop",
        bands: [-1.0, 3.0, 4.5, 5.0, 3.5, -1.0, -1.5, -1.5, -1.0, -1.0],
    },
    Preset {
        name: "Classical",
        bands: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, -4.5, -4.5, -4.5, -6.0],
    },
    Preset {
        name: "Bass",
        bands: [7.0, 7.0, 5.0, 2.0, 0.0, -2.0, -3.0, -3.0, -3.0, -3.0],
    },
    Preset {
        name: "Treble",
        bands: [-6.0, -6.0, -6.0, -2.5, 1.5, 6.5, 9.5, 9.5, 9.5, 10.0],
    },
];

/// Decibels to a linear gain.
#[must_use]
pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// One second-order section: the RBJ "Audio EQ Cookbook" peaking filter,
/// in transposed direct form II with one state pair per channel.
#[derive(Debug, Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    /// `[z1, z2]` for the left and right channels.
    z: [[f32; 2]; 2],
    /// Whether this band does anything; a flat band or one above
    /// Nyquist is skipped rather than run as an identity.
    active: bool,
}

impl Biquad {
    /// A peaking filter at `freq` Hz with `gain_db` of boost or cut.
    fn peaking(rate: f32, freq: f32, gain_db: f32) -> Self {
        // A band at or past Nyquist cannot be represented at this rate
        // (a 16 kHz band on an 22.05 kHz file); it is left out rather
        // than aliased onto something audible.
        if gain_db.abs() < 1e-3 || freq >= rate * 0.45 {
            return Self::default();
        }
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * freq / rate;
        let alpha = w0.sin() / (2.0 * EQ_Q);
        let cos = w0.cos();
        let a0 = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / a0,
            b1: (-2.0 * cos) / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha / a) / a0,
            z: [[0.0; 2]; 2],
            active: true,
        }
    }

    /// Filter one sample on channel `ch`.
    fn run(&mut self, ch: usize, x: f32) -> f32 {
        let z = &mut self.z[ch];
        let y = self.b0 * x + z[0];
        z[0] = self.b1 * x - self.a1 * y + z[1];
        z[1] = self.b2 * x - self.a2 * y;
        y
    }
}

/// The equaliser, as the audio thread runs it.
#[derive(Debug, Clone)]
pub struct Equalizer {
    settings: EqSettings,
    rate: u32,
    preamp: f32,
    bands: [Biquad; 10],
}

impl Equalizer {
    /// An equaliser for a stream at `rate` Hz.
    #[must_use]
    pub fn new(rate: u32, settings: EqSettings) -> Self {
        let mut eq = Self {
            settings,
            rate: rate.max(1),
            preamp: 1.0,
            bands: [Biquad::default(); 10],
        };
        eq.design();
        eq
    }

    /// Change the curve. Filter state is kept where a band's shape did
    /// not change, so dragging one slider does not click the other nine.
    pub fn set(&mut self, settings: EqSettings) {
        let old = self.settings;
        self.settings = settings;
        self.preamp = db_to_gain(settings.preamp);
        for (i, band) in self.bands.iter_mut().enumerate() {
            if old.bands[i].to_bits() != settings.bands[i].to_bits() {
                let z = band.z;
                *band = Biquad::peaking(self.rate as f32, EQ_FREQS[i], settings.bands[i]);
                band.z = z;
            }
        }
    }

    /// Change the stream rate: every band is redesigned and its state
    /// cleared, since a new track is a new signal.
    pub fn set_rate(&mut self, rate: u32) {
        self.rate = rate.max(1);
        self.design();
    }

    /// The current settings.
    #[must_use]
    pub fn settings(&self) -> EqSettings {
        self.settings
    }

    fn design(&mut self) {
        self.preamp = db_to_gain(self.settings.preamp);
        for (i, band) in self.bands.iter_mut().enumerate() {
            *band = Biquad::peaking(self.rate as f32, EQ_FREQS[i], self.settings.bands[i]);
        }
    }

    /// Filter interleaved stereo `samples` in place.
    pub fn process(&mut self, samples: &mut [f32]) {
        if !self.settings.enabled {
            return;
        }
        for frame in samples.chunks_exact_mut(2) {
            for (ch, s) in frame.iter_mut().enumerate() {
                let mut x = *s * self.preamp;
                for band in &mut self.bands {
                    if band.active {
                        x = band.run(ch, x);
                    }
                }
                *s = x;
            }
        }
    }
}

/// Apply volume and balance to interleaved stereo `samples`, and clip.
///
/// `volume` is `0.0..=1.0` on the slider and is **squared** on the way
/// to a gain, so the slider's middle is a quarter of the power rather
/// than barely quieter than the top — the ear hears loudness roughly
/// logarithmically, and a linear slider spends its whole lower half on
/// the last few dB. `balance` is `-1.0` (left only) to `1.0` (right
/// only); the centre leaves both sides alone rather than dropping each
/// by 3 dB, which is what a player's balance (as against a pan) does.
///
/// The clip is the end of the chain: an equaliser boost can push a
/// full-scale signal past 1.0, and the output format has no headroom.
pub fn apply_gain(samples: &mut [f32], volume: f32, balance: f32) {
    let v = volume.clamp(0.0, 1.0);
    let g = v * v;
    let b = balance.clamp(-1.0, 1.0);
    let left = g * (1.0 - b.max(0.0));
    let right = g * (1.0 + b.min(0.0));
    for frame in samples.chunks_exact_mut(2) {
        frame[0] = (frame[0] * left).clamp(-1.0, 1.0);
        frame[1] = (frame[1] * right).clamp(-1.0, 1.0);
    }
}

/// Points in the analysis window. At 44.1 kHz this is 11.6 ms of sound
/// and 43 Hz per bin, which resolves the bottom band of a 19-bar
/// analyser without making the display lag what is heard.
pub const FFT_SIZE: usize = 512;

/// In-place iterative radix-2 FFT over `re`/`im`, whose length must be a
/// power of two.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two() && im.len() == n);
    // Bit-reversal permutation.
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let a = start + k;
                let b = a + len / 2;
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
        }
        len <<= 1;
    }
}

/// Bars in the spectrum analyser: Winamp's "thick bands" count.
pub const BARS: usize = 19;

/// The level of each of [`BARS`] log-spaced bands in `mono`, as
/// `0.0..=1.0` on a 60 dB scale.
///
/// `mono` is the most recent [`FFT_SIZE`] samples (fewer is padded with
/// silence). It is Hann-windowed, transformed, and the bins are grouped
/// into bands from 40 Hz to 16 kHz on a logarithmic axis, each band
/// taking its loudest bin — the peak, not the mean, because a
/// visualiser that averages a kick drum into its neighbours stops
/// moving with the music.
#[must_use]
pub fn spectrum(mono: &[f32], rate: u32) -> [f32; BARS] {
    let mut re = [0.0f32; FFT_SIZE];
    let mut im = [0.0f32; FFT_SIZE];
    let start = mono.len().saturating_sub(FFT_SIZE);
    for (i, s) in mono[start..].iter().enumerate() {
        let w = 0.5 - 0.5 * (2.0 * PI * i as f32 / (FFT_SIZE - 1) as f32).cos();
        re[i] = s * w;
    }
    fft(&mut re, &mut im);
    let bin_hz = rate.max(1) as f32 / FFT_SIZE as f32;
    let (lo, hi) = (40.0f32, 16_000.0f32.min(rate as f32 / 2.0));
    let mut out = [0.0f32; BARS];
    for (b, level) in out.iter_mut().enumerate() {
        let f0 = lo * (hi / lo).powf(b as f32 / BARS as f32);
        let f1 = lo * (hi / lo).powf((b + 1) as f32 / BARS as f32);
        let k0 = ((f0 / bin_hz) as usize).clamp(1, FFT_SIZE / 2 - 1);
        let k1 = ((f1 / bin_hz).ceil() as usize).clamp(k0 + 1, FFT_SIZE / 2);
        let mut peak = 0.0f32;
        for k in k0..k1 {
            peak = peak.max((re[k] * re[k] + im[k] * im[k]).sqrt());
        }
        // A full-scale sine through a Hann window peaks at N/4.
        let norm = peak / (FFT_SIZE as f32 / 4.0);
        let db = 20.0 * norm.max(1e-9).log10();
        *level = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    }
    out
}

/// The analyser's display state: bars that jump up and fall slowly,
/// and peak caps that hold, then drop.
#[derive(Debug, Clone, PartialEq)]
pub struct Analyzer {
    /// Bar heights, `0.0..=1.0`.
    pub bars: [f32; BARS],
    /// Peak-cap heights.
    pub peaks: [f32; BARS],
    /// Ticks each cap has left to hold before it falls.
    hold: [u8; BARS],
}

impl Default for Analyzer {
    fn default() -> Self {
        Self {
            bars: [0.0; BARS],
            peaks: [0.0; BARS],
            hold: [0; BARS],
        }
    }
}

/// How far a bar falls per tick.
const BAR_FALL: f32 = 0.06;
/// How far a peak cap falls per tick once its hold expires.
const PEAK_FALL: f32 = 0.02;
/// Ticks a peak cap holds.
const PEAK_HOLD: u8 = 10;

impl Analyzer {
    /// Take one tick's measurement.
    pub fn update(&mut self, levels: &[f32; BARS]) {
        let each = self
            .bars
            .iter_mut()
            .zip(&mut self.peaks)
            .zip(&mut self.hold)
            .zip(levels);
        for (((bar, peak), hold), &target) in each {
            *bar = if target >= *bar {
                target
            } else {
                (*bar - BAR_FALL).max(target)
            };
            if *bar >= *peak {
                *peak = *bar;
                *hold = PEAK_HOLD;
            } else if *hold > 0 {
                *hold -= 1;
            } else {
                *peak = (*peak - PEAK_FALL).max(*bar);
            }
        }
    }

    /// Whether everything has come to rest at zero — the point at which
    /// a stopped player may stop ticking.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.peaks.iter().all(|p| *p <= 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: u32, n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    fn rms_stereo(s: &[f32]) -> f32 {
        let sum: f32 = s.iter().step_by(2).map(|x| x * x).sum();
        (sum / (s.len() / 2) as f32).sqrt()
    }

    fn stereo(mono: &[f32]) -> Vec<f32> {
        mono.iter().flat_map(|s| [*s, *s]).collect()
    }

    #[test]
    fn a_boosted_band_boosts_its_own_frequency_and_not_others() {
        let rate = 44_100;
        let mut bands = [0.0; 10];
        bands[4] = 12.0; // 1 kHz
        let settings = EqSettings {
            enabled: true,
            preamp: 0.0,
            bands,
        };
        let at = |freq: f32| {
            let mut eq = Equalizer::new(rate, settings);
            let mut s = stereo(&sine(freq, rate, 8192, 0.1));
            eq.process(&mut s);
            // Skip the filter's settling time.
            rms_stereo(&s[4096..]) / (0.1 / 2f32.sqrt())
        };
        let on = at(1_000.0);
        assert!((on - db_to_gain(12.0)).abs() < 0.2, "1 kHz gain {on}");
        let off = at(60.0);
        assert!((off - 1.0).abs() < 0.1, "60 Hz gain {off}");
    }

    #[test]
    fn a_disabled_equaliser_is_a_true_bypass() {
        let mut eq = Equalizer::new(
            44_100,
            EqSettings {
                enabled: false,
                preamp: 12.0,
                bands: [12.0; 10],
            },
        );
        let orig = stereo(&sine(440.0, 44_100, 256, 0.5));
        let mut s = orig.clone();
        eq.process(&mut s);
        assert_eq!(s, orig);
    }

    #[test]
    fn bands_above_nyquist_are_left_out() {
        let mut bands = [0.0; 10];
        bands[9] = 12.0; // 16 kHz, unrepresentable at 22.05 kHz
        let mut eq = Equalizer::new(
            22_050,
            EqSettings {
                enabled: true,
                preamp: 0.0,
                bands,
            },
        );
        let orig = stereo(&sine(5_000.0, 22_050, 512, 0.5));
        let mut s = orig.clone();
        eq.process(&mut s);
        assert_eq!(s, orig);
    }

    #[test]
    fn volume_is_squared_and_balance_cuts_one_side() {
        let mut s = vec![1.0, 1.0];
        apply_gain(&mut s, 0.5, 0.0);
        assert_eq!(s, [0.25, 0.25]);
        let mut s = vec![1.0, 1.0];
        apply_gain(&mut s, 1.0, 1.0);
        assert_eq!(s, [0.0, 1.0]);
        let mut s = vec![1.0, 1.0];
        apply_gain(&mut s, 1.0, -0.5);
        assert_eq!(s, [1.0, 0.5]);
        let mut s = vec![3.0, -3.0];
        apply_gain(&mut s, 1.0, 0.0);
        assert_eq!(s, [1.0, -1.0], "clipped");
    }

    #[test]
    fn the_spectrum_puts_a_tone_in_its_own_bar() {
        let rate = 44_100;
        let low = spectrum(&sine(100.0, rate, FFT_SIZE, 1.0), rate);
        let high = spectrum(&sine(8_000.0, rate, FFT_SIZE, 1.0), rate);
        let loudest = |s: &[f32; BARS]| {
            s.iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(i, _)| i)
                .unwrap()
        };
        assert!(loudest(&low) < 5, "100 Hz lands low: {low:?}");
        assert!(loudest(&high) > 13, "8 kHz lands high: {high:?}");
        assert!(low[loudest(&low)] > 0.9, "full scale reads near the top");
        let silent = spectrum(&[0.0; FFT_SIZE], rate);
        assert!(silent.iter().all(|v| *v == 0.0));
        // Short input is padded, not a panic.
        let _ = spectrum(&[0.5; 10], rate);
    }

    #[test]
    fn bars_jump_up_fall_slowly_and_peaks_hold() {
        let mut a = Analyzer::default();
        let mut levels = [0.0; BARS];
        levels[0] = 1.0;
        a.update(&levels);
        assert!((a.bars[0] - 1.0).abs() < f32::EPSILON);
        levels[0] = 0.0;
        a.update(&levels);
        assert!((a.bars[0] - (1.0 - BAR_FALL)).abs() < 1e-6);
        assert!((a.peaks[0] - 1.0).abs() < f32::EPSILON, "held");
        for _ in 0..200 {
            a.update(&levels);
        }
        assert!(a.is_settled());
    }
}
