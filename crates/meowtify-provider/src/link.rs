//! The link the app is on, and which bitrate downloads should use.
//!
//! The platform reports whether the current link is metered (a phone on cellular). The user
//! picks a bitrate mode in the settings: `Off` always takes the best copy, `Low` always the
//! small one, and `Auto` takes the small one on a metered link or when the measured speed of
//! the last large ranges is slow. A low copy is replaced by the better one once the link is
//! unmetered again (see [`Provider::upgrade`](crate::Provider::upgrade)).

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// True while the platform says the link is metered. Unknown counts as unmetered, so a
/// computer never saves the small copy by accident.
static METERED: AtomicBool = AtomicBool::new(false);
/// The saved [`BitrateMode`], as the index the enum uses.
static MODE: AtomicU8 = AtomicU8::new(BitrateMode::Off as u8);

/// Below this smoothed speed (bytes per second, about 1.2 Mbit/s) a link counts as slow.
const SLOW_BYTES_PER_SEC: f64 = 150_000.0;
/// Weight of the newest sample in the smoothed average.
const ALPHA: f64 = 0.3;
/// One slow sample proves nothing; this many are needed before `Auto` reacts.
const MIN_SAMPLES: u32 = 2;
/// Samples older than this are dropped, so yesterday's slow train does not make today's
/// fast Wi-Fi save small files.
const STALE_AFTER: Duration = Duration::from_secs(10 * 60);

/// What downloads should do about the bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BitrateMode {
    /// Always the best copy.
    #[default]
    Off,
    /// The small copy on a metered or slow link, the best copy otherwise.
    Auto,
    /// Always the small copy.
    Low,
}

impl BitrateMode {
    /// Parses the value stored in the settings. Anything unknown means `Off`.
    pub fn from_str(text: &str) -> BitrateMode {
        match text.trim().to_ascii_lowercase().as_str() {
            "low" => BitrateMode::Low,
            "auto" => BitrateMode::Auto,
            _ => BitrateMode::Off,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BitrateMode::Off => "off",
            BitrateMode::Auto => "auto",
            BitrateMode::Low => "low",
        }
    }
}

#[derive(Default)]
struct Speed {
    smoothed: Option<f64>,
    samples: u32,
    last: Option<Instant>,
}

static SPEED: Mutex<Option<Speed>> = Mutex::new(None);

/// Says whether the link is metered. Called by the platform when the network changes.
pub fn set_metered(metered: bool) {
    METERED.store(metered, Ordering::Relaxed);
}

pub fn metered() -> bool {
    METERED.load(Ordering::Relaxed)
}

/// Sets the bitrate mode. Set from the settings at start and when the user changes it.
pub fn set_bitrate_mode(mode: BitrateMode) {
    MODE.store(mode as u8, Ordering::Relaxed);
}

pub fn bitrate_mode() -> BitrateMode {
    match MODE.load(Ordering::Relaxed) {
        1 => BitrateMode::Auto,
        2 => BitrateMode::Low,
        _ => BitrateMode::Off,
    }
}

/// The old on/off setting: `true` is [`BitrateMode::Low`], `false` is [`BitrateMode::Off`].
pub fn set_low_bitrate(on: bool) {
    set_bitrate_mode(if on { BitrateMode::Low } else { BitrateMode::Off });
}

/// True only for [`BitrateMode::Low`].
pub fn low_bitrate() -> bool {
    bitrate_mode() == BitrateMode::Low
}

/// Adds one finished range to the speed average. Called by the fetch code with
/// `bytes / elapsed` after each range of at least 1 MiB.
pub fn record_throughput(bytes_per_sec: u64) {
    let mut guard = SPEED.lock().unwrap();
    let state = guard.get_or_insert_with(Speed::default);
    if state.last.is_some_and(|at| at.elapsed() > STALE_AFTER) {
        *state = Speed::default();
    }
    let sample = bytes_per_sec as f64;
    state.smoothed = Some(match state.smoothed {
        Some(old) => ALPHA * sample + (1.0 - ALPHA) * old,
        None => sample,
    });
    state.samples = state.samples.saturating_add(1);
    state.last = Some(Instant::now());
}

/// Whether the measured speed says the link is slow. An average that has had no samples for
/// ten minutes is forgotten first.
fn measured_slow() -> bool {
    let mut guard = SPEED.lock().unwrap();
    let Some(state) = guard.as_mut() else { return false };
    if state.last.is_some_and(|at| at.elapsed() > STALE_AFTER) {
        *guard = None;
        return false;
    }
    state.samples >= MIN_SAMPLES && state.smoothed.is_some_and(|speed| speed < SLOW_BYTES_PER_SEC)
}

/// Whether a download now should take the low bitrate copy.
pub fn save_data() -> bool {
    match bitrate_mode() {
        BitrateMode::Off => false,
        BitrateMode::Low => true,
        BitrateMode::Auto => metered() || measured_slow(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_and_speed_rules() {
        // One test only: the state is global, so parallel tests would race.
        set_metered(false);
        set_bitrate_mode(BitrateMode::Off);
        assert!(!save_data(), "Off never saves data");
        set_metered(true);
        assert!(!save_data(), "Off stays off on a metered link");

        set_bitrate_mode(BitrateMode::Low);
        assert!(save_data(), "Low always saves data");
        set_metered(false);
        assert!(save_data(), "Low stays on over Wi-Fi");

        set_bitrate_mode(BitrateMode::Auto);
        assert!(!save_data(), "Auto over a fast link takes the best copy");
        set_metered(true);
        assert!(save_data(), "Auto on a metered link saves data");
        set_metered(false);

        // Reset the average, then feed two slow samples.
        *SPEED.lock().unwrap() = None;
        record_throughput(120_000);
        assert!(!save_data(), "one sample is not enough to call a link slow");
        record_throughput(90_000);
        assert!(save_data(), "two slow samples make Auto save data");

        // A fast sample pulls the average up.
        for _ in 0..10 {
            record_throughput(8_000_000);
        }
        assert!(!save_data(), "a fast link makes Auto take the best copy");

        // Ten minutes with no samples forgets the average.
        {
            let mut guard = SPEED.lock().unwrap();
            let state = guard.as_mut().unwrap();
            state.last = Instant::now().checked_sub(STALE_AFTER + Duration::from_secs(1));
            state.smoothed = Some(1.0); // would be slow if it survived
            state.samples = 5;
        }
        assert!(!save_data(), "a stale average is forgotten");
        assert!(SPEED.lock().unwrap().is_none(), "the stale average was reset");

        // Parsing for the settings.
        assert_eq!(BitrateMode::from_str("off"), BitrateMode::Off);
        assert_eq!(BitrateMode::from_str("AUTO"), BitrateMode::Auto);
        assert_eq!(BitrateMode::from_str(" low "), BitrateMode::Low);
        assert_eq!(BitrateMode::from_str("nonsense"), BitrateMode::Off);
        assert_eq!(BitrateMode::as_str(BitrateMode::Auto), "auto");

        set_metered(false);
        set_bitrate_mode(BitrateMode::Off);
        *SPEED.lock().unwrap() = None;
    }
}
