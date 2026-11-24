//! Kiosk mode (Settings > GENERAL, `KIOSK_MODE`): children-proof, the
//! Library only, where games can only be launched (`on`), and not even the
//! app quit (`strict`). Left by holding a button combination, or by setting
//! `KIOSK_MODE=off` in `config.txt`; neither needs to be secret, only out
//! of a child's reach.

use super::engine::input::Action;

pub const KEY: &str = "KIOSK_MODE";
pub const LEVELS: &[&str] = &["off", "on", "strict"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Off,
    /// The Library only: launch a game, or quit.
    On,
    /// Launch a game, nothing else.
    Strict,
}

impl Level {
    /// The level a `KIOSK_MODE` value asks for; anything else is off.
    pub fn of(value: Option<&str>) -> Level {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("on") => Level::On,
            Some("strict") => Level::Strict,
            _ => Level::Off,
        }
    }
}

/// L1 + R1 + X on a gamepad, Q + E + X on a keyboard.
pub const UNLOCK: [Action; 3] = [Action::TabPrev, Action::TabNext, Action::Edit];

/// Long enough that pressing buttons at random never gets there.
pub const UNLOCK_SECS: f64 = 5.0;

/// How far the unlock is (0..1), from how long each button of [`UNLOCK`]
/// has been held: the shortest counts, and none held is no progress.
pub fn unlock_progress(held: &[Option<f64>]) -> f32 {
    let shortest = held
        .iter()
        .map(|secs| secs.unwrap_or(0.0))
        .fold(f64::INFINITY, f64::min);
    (shortest.clamp(0.0, UNLOCK_SECS) / UNLOCK_SECS) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_level_is_read_from_the_setting_whatever_its_case() {
        assert_eq!(Level::of(Some("on")), Level::On);
        assert_eq!(Level::of(Some(" Strict ")), Level::Strict);
        assert_eq!(Level::of(Some("off")), Level::Off);
        assert_eq!(Level::of(Some("maybe")), Level::Off);
        assert_eq!(Level::of(None), Level::Off);
    }

    #[test]
    fn the_unlock_needs_every_button_held_the_whole_time() {
        assert_eq!(unlock_progress(&[Some(6.0), Some(6.0), Some(6.0)]), 1.0);
        assert_eq!(unlock_progress(&[Some(6.0), Some(2.5), Some(6.0)]), 0.5);
        assert_eq!(unlock_progress(&[Some(6.0), None, Some(6.0)]), 0.0);
        assert_eq!(unlock_progress(&[None, None, None]), 0.0);
    }
}
