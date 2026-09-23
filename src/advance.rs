//! When a playlist item moves on: after a time, or after its content has run
//! through a number of times.
//!
//! What one pass *is* follows from the content and is not stored: a video
//! played to its end; anything scrolled reached the bottom (a paged PDF, its
//! last page). Storing the kind beside the count would allow "last page" on a
//! video -- a setting that silently does nothing.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::models::ScrollMode;

const MIN_SECONDS: u32 = 1;
const MAX_SECONDS: u32 = 7 * 24 * 60 * 60;
const MIN_COUNT: u16 = 1;
const MAX_COUNT: u16 = 100;

/// How often the loop asks the page how far it is.
pub const POLL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum Advance {
    Time { seconds: u32 },
    Passes { count: u16 },
}

impl Advance {
    /// Clamped rather than refused, like every number that reaches the screen:
    /// zero seconds spins the loop, and an absurd count is a stuck screen.
    pub fn clamped(self) -> Self {
        match self {
            Advance::Time { seconds } => Advance::Time { seconds: seconds.clamp(MIN_SECONDS, MAX_SECONDS) },
            Advance::Passes { count } => Advance::Passes { count: count.clamp(MIN_COUNT, MAX_COUNT) },
        }
    }

    /// What an item gets when it names none: its asset's length if it has
    /// one (a video's is measured at upload), else ten seconds -- exactly what
    /// the loop used to fall back to.
    pub fn default_for(asset_seconds: Option<i64>) -> Self {
        let seconds = asset_seconds.unwrap_or(10).clamp(MIN_SECONDS as i64, MAX_SECONDS as i64) as u32;
        Advance::Time { seconds }
    }
}

/// Whether `advance` can work for this content. `Passes` needs something that
/// ends: a video, or a scroll mode that moves.
pub fn check(advance: &Advance, mimetype: Option<&str>, scroll: &ScrollMode) -> Result<(), &'static str> {
    let Advance::Passes { .. } = advance else {
        return Ok(());
    };
    let video = mimetype.is_some_and(|m| m.to_ascii_lowercase().starts_with("video/"));
    if video || !matches!(scroll, ScrollMode::None) {
        Ok(())
    } else {
        Err("Durchläufe brauchen etwas, das endet: ein Video oder einen Scroll-Modus.")
    }
}

/// What the page reports through `globalThis.__advance.state()`.
#[derive(Debug, Deserialize)]
pub struct RuntimeState {
    pub passes: u32,
    /// How long the content has not moved on schedule, in the page's clock.
    pub idle_ms: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Wait,
    Done,
    Stalled(&'static str),
}

/// The loop's decision for a `Passes` item on one poll. `missing_for` is how
/// long the page has had no runtime at all; `stall` is `--advance-stall-timeout`.
pub fn verdict(state: Option<&RuntimeState>, count: u16, missing_for: Duration, stall: Duration) -> Verdict {
    match state {
        None if missing_for >= stall => Verdict::Stalled("no advance runtime on the page"),
        None => Verdict::Wait,
        Some(s) if s.passes >= u32::from(count) => Verdict::Done,
        Some(s) if Duration::from_millis(s.idle_ms) >= stall => Verdict::Stalled("the content stopped moving"),
        Some(_) => Verdict::Wait,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ScrollOptions;
    use serde_json::json;

    #[test]
    fn the_json_shape_is_tagged_by_on() {
        assert_eq!(serde_json::to_value(Advance::Time { seconds: 30 }).unwrap(), json!({"on": "time", "seconds": 30}));
        assert_eq!(serde_json::to_value(Advance::Passes { count: 2 }).unwrap(), json!({"on": "passes", "count": 2}));
        let parsed: Advance = serde_json::from_value(json!({"on": "passes", "count": 3})).unwrap();
        assert_eq!(parsed, Advance::Passes { count: 3 });
        assert!(serde_json::from_value::<Advance>(json!({"on": "never"})).is_err());
    }

    #[test]
    fn numbers_are_clamped() {
        assert_eq!(Advance::Time { seconds: 0 }.clamped(), Advance::Time { seconds: 1 });
        assert_eq!(Advance::Time { seconds: u32::MAX }.clamped(), Advance::Time { seconds: MAX_SECONDS });
        assert_eq!(Advance::Passes { count: 0 }.clamped(), Advance::Passes { count: 1 });
        assert_eq!(Advance::Passes { count: 5000 }.clamped(), Advance::Passes { count: 100 });
    }

    #[test]
    fn the_default_is_the_assets_length_or_ten_seconds() {
        assert_eq!(Advance::default_for(Some(42)), Advance::Time { seconds: 42 });
        assert_eq!(Advance::default_for(None), Advance::Time { seconds: 10 });
        assert_eq!(Advance::default_for(Some(-5)), Advance::Time { seconds: 1 });
    }

    #[test]
    fn passes_need_something_that_ends() {
        let passes = Advance::Passes { count: 2 };
        let scrolling = ScrollMode::Continuous(ScrollOptions::default());
        assert!(check(&passes, Some("video/mp4"), &ScrollMode::None).is_ok());
        assert!(check(&passes, Some("application/pdf"), &scrolling).is_ok());
        assert!(check(&passes, None, &scrolling).is_ok());
        assert!(check(&passes, None, &ScrollMode::None).is_err());
        assert!(check(&passes, Some("image/png"), &ScrollMode::None).is_err());
        assert!(check(&Advance::Time { seconds: 5 }, None, &ScrollMode::None).is_ok());
    }

    #[test]
    fn the_verdict() {
        let stall = Duration::from_secs(120);
        let state = |passes, idle_ms| RuntimeState { passes, idle_ms };
        assert_eq!(verdict(Some(&state(1, 0)), 2, Duration::ZERO, stall), Verdict::Wait);
        assert_eq!(verdict(Some(&state(2, 0)), 2, Duration::ZERO, stall), Verdict::Done);
        assert_eq!(verdict(Some(&state(0, 120_000)), 2, Duration::ZERO, stall),
                   Verdict::Stalled("the content stopped moving"));
        assert_eq!(verdict(None, 2, Duration::from_secs(5), stall), Verdict::Wait);
        assert_eq!(verdict(None, 2, stall, stall), Verdict::Stalled("no advance runtime on the page"));
        // A count reached wins over a stall reported in the same poll.
        assert_eq!(verdict(Some(&state(2, 500_000)), 2, Duration::ZERO, stall), Verdict::Done);
    }
}
