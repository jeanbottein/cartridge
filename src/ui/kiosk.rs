//! Kiosk mode (Settings > GENERAL, `KIOSK_MODE`): children-proof, the
//! Library only, and only launching games. Left by holding a button
//! combination, or by setting `KIOSK_MODE=False` in `config.txt`; neither
//! needs to be secret, only out of a child's reach.

use super::engine::input::Action;

pub const KEY: &str = "KIOSK_MODE";

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
    fn the_unlock_needs_every_button_held_the_whole_time() {
        assert_eq!(unlock_progress(&[Some(6.0), Some(6.0), Some(6.0)]), 1.0);
        assert_eq!(unlock_progress(&[Some(6.0), Some(2.5), Some(6.0)]), 0.5);
        assert_eq!(unlock_progress(&[Some(6.0), None, Some(6.0)]), 0.0);
        assert_eq!(unlock_progress(&[None, None, None]), 0.0);
    }
}
