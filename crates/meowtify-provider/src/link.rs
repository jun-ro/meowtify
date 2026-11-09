//! The link the app is on, and whether songs should be saved at the low bitrate.
//!
//! The platform reports whether the current link is metered (a phone on cellular). The user
//! asks for the data-saving mode in the settings. Both must be true for a download to take
//! the low bitrate copy, and the copy is replaced by the better one once the link is
//! unmetered again (see [`Provider::upgrade`](crate::Provider::upgrade)).

use std::sync::atomic::{AtomicBool, Ordering};

/// True while the platform says the link is metered. Unknown counts as unmetered, so a
/// computer never saves the small copy by accident.
static METERED: AtomicBool = AtomicBool::new(false);
/// True when the user turned the data-saving mode on.
static LOW_BITRATE: AtomicBool = AtomicBool::new(false);

/// Says whether the link is metered. Called by the platform when the network changes.
pub fn set_metered(metered: bool) {
    METERED.store(metered, Ordering::Relaxed);
}

pub fn metered() -> bool {
    METERED.load(Ordering::Relaxed)
}

/// Turns the data-saving mode on or off. Set from the settings at start.
pub fn set_low_bitrate(on: bool) {
    LOW_BITRATE.store(on, Ordering::Relaxed);
}

pub fn low_bitrate() -> bool {
    LOW_BITRATE.load(Ordering::Relaxed)
}

/// Whether a download now should take the low bitrate copy: the user asked for it, and the
/// link is metered.
pub fn save_data() -> bool {
    metered() && low_bitrate()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_small_copy_needs_both_the_setting_and_a_metered_link() {
        let cases = [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, true),
        ];
        for (low, metered, want) in cases {
            set_low_bitrate(low);
            set_metered(metered);
            assert_eq!(save_data(), want, "low bitrate {low}, metered {metered}");
        }
        set_low_bitrate(false);
        set_metered(false);
    }
}
