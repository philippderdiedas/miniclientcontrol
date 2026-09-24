//! A screen that stopped being painted. CDP answers normally while the
//! compositor hangs -- measured on kiosk2 and seen for thirteen hours on a Pi --
//! so the signal is the overlay runtime's frame counter, sampled from here.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::models::{AppState, Display};

pub const SAMPLE: Duration = Duration::from_secs(5);
/// No second restart within this long of the last one: a panel switched off,
/// or a GPU that is gone, must not become a restart loop.
pub const BRAKE: Duration = Duration::from_secs(30 * 60);

/// What one sample found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// The counter's value.
    Frames(u64),
    /// The page is there and did not answer in time. Not "no information":
    /// with Xorg stopped for longer than a few seconds even `Runtime.evaluate`
    /// hangs (measured on kiosk2) -- treating that as unknown detected the
    /// freeze only when the display server came back, and then restarted a
    /// screen that was already recovering.
    Unanswered,
    /// Nothing to read: no page, no runtime, a navigation under the evaluate.
    Nothing,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    None,
    Frozen { seconds: u64, restart: bool },
    Recovered { seconds: u64 },
}

#[derive(Default)]
pub struct Watch {
    /// The last sample that showed progress, and its counter.
    last: Option<(Instant, u64)>,
    frozen: bool,
    /// When the stall that led to the freeze began -- kept across a new page,
    /// so a recovery reports how long the screen was really frozen.
    stalled_from: Option<Instant>,
    last_restart: Option<Instant>,
}

impl Watch {
    /// A different page is on screen: its counter is unrelated to the last.
    pub fn new_page(&mut self) {
        self.last = None;
    }

    /// One sample. `Reading::Nothing` is no information, never evidence of a
    /// freeze; `Reading::Unanswered` counts as no progress.
    pub fn observe(&mut self, now: Instant, reading: Reading, timeout: Duration, can_restart: bool) -> Event {
        let frames = match reading {
            Reading::Nothing => return Event::None,
            Reading::Frames(frames) => Some(frames),
            Reading::Unanswered => None,
        };
        match (self.last, frames) {
            // A page that does not answer before any baseline -- it hung right
            // after a page change: the stall is measured from now. The counter
            // can never be `u64::MAX`, so any real reading after it is progress.
            (None, None) => {
                self.last = Some((now, u64::MAX));
                Event::None
            }
            (Some((since, _)), None) => self.stalled(now, since, timeout, can_restart),
            (last, Some(frames)) => self.counted(now, last, frames, timeout, can_restart),
        }
    }

    fn counted(&mut self, now: Instant, last: Option<(Instant, u64)>, frames: u64, timeout: Duration, can_restart: bool) -> Event {
        match last {
            // First sample, a new document (the counter starts over), or progress.
            None => {
                self.last = Some((now, frames));
                Event::None
            }
            Some((_, seen)) if frames != seen => {
                let since = self.last.map(|(at, _)| at).unwrap_or(now);
                self.last = Some((now, frames));
                if std::mem::take(&mut self.frozen) {
                    let from = self.stalled_from.take().unwrap_or(since);
                    Event::Recovered { seconds: now.duration_since(from).as_secs() }
                } else {
                    Event::None
                }
            }
            Some((since, _)) => self.stalled(now, since, timeout, can_restart),
        }
    }

    fn stalled(&mut self, now: Instant, since: Instant, timeout: Duration, can_restart: bool) -> Event {
        let stalled = now.duration_since(since);
        if self.frozen || stalled < timeout {
            return Event::None;
        }
        self.frozen = true;
        self.stalled_from = Some(since);
        let restart = can_restart
            && self.last_restart.is_none_or(|at| now.duration_since(at) >= BRAKE);
        if restart {
            self.last_restart = Some(now);
        }
        Event::Frozen { seconds: stalled.as_secs(), restart }
    }
}

/// Sample the page on screen every `SAMPLE` for as long as the process runs.
pub async fn watch(state: AppState, display: Arc<Display>) {
    let timeout = Duration::from_secs(state.args.freeze_timeout.max(10));
    let mut watch = Watch::default();
    let mut target: Option<String> = None;
    loop {
        tokio::time::sleep(SAMPLE).await;
        let page = display.screen_page.lock().await.clone();
        // Another page on screen (a keep_loaded tab, the page of a restarted
        // browser) gets a fresh baseline -- but whether the screen is frozen,
        // and when the browser was last restarted, carry over: a restart that
        // brings a painting page back is exactly the recovery to report.
        let id = page.as_ref().map(|p| p.target_id().inner().clone());
        if id != target {
            watch.new_page();
            target = id;
        }
        let reading = match &page {
            Some(page) => read_frames(page).await,
            None => Reading::Nothing,
        };
        let can_restart = display.browser_pid.lock().await.is_some();
        let name = display.name.clone();
        match watch.observe(Instant::now(), reading, timeout, can_restart) {
            Event::None => {}
            Event::Frozen { seconds, restart } => {
                tracing::warn!(
                    "Display {} has painted nothing for {} s{}",
                    name,
                    seconds,
                    if restart { ", restarting its browser" } else { "" }
                );
                *display.frozen_since.lock().await =
                    Some(chrono::Utc::now() - chrono::Duration::seconds(seconds as i64));
                state.webhooks.fire(&name, crate::webhook::Event::DisplayFrozen { seconds, restarted: restart });
                if restart {
                    display.browser_restart.notify_one();
                }
            }
            Event::Recovered { seconds } => {
                tracing::info!("Display {} is painting again after {} s", name, seconds);
                *display.frozen_since.lock().await = None;
                state.webhooks.fire(&name, crate::webhook::Event::DisplayRecovered { seconds });
            }
        }
    }
}

/// The overlay runtime's counter; `Unanswered` when the page is there and
/// hangs, `Nothing` when there is nothing to read.
async fn read_frames(page: &chromiumoxide::Page) -> Reading {
    let answer = tokio::time::timeout(
        Duration::from_secs(3),
        page.evaluate("(() => typeof globalThis.__ovFrames === 'number' ? globalThis.__ovFrames : null)()"),
    )
    .await;
    match answer {
        Err(_) => Reading::Unanswered,
        Ok(Err(_)) => Reading::Nothing,
        Ok(Ok(result)) => match result.into_value::<Option<u64>>() {
            Ok(Some(frames)) => Reading::Frames(frames),
            _ => Reading::Nothing,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(60);

    fn at(start: Instant, secs: u64) -> Instant {
        start + Duration::from_secs(secs)
    }

    #[test]
    fn a_ticking_counter_is_fine() {
        let (mut w, s) = (Watch::default(), Instant::now());
        for (i, n) in [(0, 10), (5, 300), (10, 600), (70, 4000)] {
            assert_eq!(w.observe(at(s, i), Reading::Frames(n), T, true), Event::None);
        }
    }

    #[test]
    fn a_stalled_counter_freezes_once_then_recovers() {
        let (mut w, s) = (Watch::default(), Instant::now());
        w.observe(at(s, 0), Reading::Frames(100), T, true);
        assert_eq!(w.observe(at(s, 30), Reading::Frames(100), T, true), Event::None);
        assert_eq!(w.observe(at(s, 60), Reading::Frames(100), T, true), Event::Frozen { seconds: 60, restart: true });
        assert_eq!(w.observe(at(s, 65), Reading::Frames(100), T, true), Event::None, "reported once");
        assert_eq!(w.observe(at(s, 90), Reading::Frames(5), T, true), Event::Recovered { seconds: 90 });
    }

    #[test]
    fn the_brake_stops_a_second_restart() {
        let (mut w, s) = (Watch::default(), Instant::now());
        w.observe(at(s, 0), Reading::Frames(1), T, true);
        assert_eq!(w.observe(at(s, 60), Reading::Frames(1), T, true), Event::Frozen { seconds: 60, restart: true });
        w.observe(at(s, 70), Reading::Frames(2), T, true);
        assert_eq!(w.observe(at(s, 130), Reading::Frames(2), T, true), Event::Frozen { seconds: 60, restart: false });
        w.observe(at(s, 140), Reading::Frames(3), T, true);
        let later = 140 + BRAKE.as_secs();
        w.observe(at(s, later), Reading::Frames(4), T, true);
        assert_eq!(w.observe(at(s, later + 60), Reading::Frames(4), T, true), Event::Frozen { seconds: 60, restart: true });
    }

    #[test]
    fn a_browser_not_ours_is_never_restarted() {
        let (mut w, s) = (Watch::default(), Instant::now());
        w.observe(at(s, 0), Reading::Frames(1), T, false);
        assert_eq!(w.observe(at(s, 60), Reading::Frames(1), T, false), Event::Frozen { seconds: 60, restart: false });
    }

    #[test]
    fn a_new_page_after_a_restart_is_a_recovery() {
        let (mut w, s) = (Watch::default(), Instant::now());
        w.observe(at(s, 0), Reading::Frames(500), T, true);
        assert_eq!(w.observe(at(s, 60), Reading::Frames(500), T, true), Event::Frozen { seconds: 60, restart: true });
        w.new_page();
        assert_eq!(w.observe(at(s, 80), Reading::Frames(3), T, true), Event::None, "a baseline, not yet progress");
        assert_eq!(w.observe(at(s, 85), Reading::Frames(300), T, true), Event::Recovered { seconds: 85 },
                   "frozen since the last frame at 0, not since the new page's baseline");
    }

    #[test]
    fn a_page_that_does_not_answer_is_a_stall() {
        let (mut w, s) = (Watch::default(), Instant::now());
        w.observe(at(s, 0), Reading::Frames(7), T, true);
        assert_eq!(w.observe(at(s, 30), Reading::Unanswered, T, true), Event::None);
        assert_eq!(w.observe(at(s, 60), Reading::Unanswered, T, true), Event::Frozen { seconds: 60, restart: true });
        assert_eq!(w.observe(at(s, 70), Reading::Frames(900), T, true), Event::Recovered { seconds: 70 });
    }

    #[test]
    fn a_page_that_hangs_right_after_a_change_is_measured_from_then() {
        let (mut w, s) = (Watch::default(), Instant::now());
        assert_eq!(w.observe(at(s, 0), Reading::Unanswered, T, true), Event::None);
        assert_eq!(w.observe(at(s, 30), Reading::Unanswered, T, true), Event::None);
        assert_eq!(w.observe(at(s, 60), Reading::Unanswered, T, true), Event::Frozen { seconds: 60, restart: true });
        assert_eq!(w.observe(at(s, 65), Reading::Frames(2), T, true), Event::Recovered { seconds: 65 });
    }

    #[test]
    fn no_reading_is_no_evidence() {
        let (mut w, s) = (Watch::default(), Instant::now());
        w.observe(at(s, 0), Reading::Frames(1), T, true);
        for i in [30, 60, 90, 300] {
            assert_eq!(w.observe(at(s, i), Reading::Nothing, T, true), Event::None);
        }
    }
}
