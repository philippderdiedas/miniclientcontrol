# Dayparting Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Each display gets a timetable — a default playlist plus ordered time windows (weekdays, from, to → playlist) — and the control loop plays whatever the timetable says is live, switching at window boundaries.

**Architecture:** A new `src/schedule/` module: `mod.rs` holds the pure rules (`active`, `next_boundary`, `overlaps`, parsing) plus the two storage reads, `api.rs` the `GET`/`PUT /api/displays/{name}/schedule` handlers. `displays.playlist_id` is renamed `default_playlist_id`, windows live in a new `schedule_windows` table. The loop resolves through `schedule::active_playlist` and gets one new `select!` branch that wakes at the next boundary.

**Tech Stack:** Rust (axum, sqlx/SQLite, chrono 0.4 with `Local`), vanilla HTML/JS, stdlib-only Python end-to-end tests in `tests/cast/`.

**Spec:** `docs/superpowers/specs/2026-09-23-dayparting-design.md`

## Global Constraints

- Schema only in `src/db.rs::run_migrations`, idempotent, behind `pragma_table_info` probes. `main.rs` must not create tables.
- `web/` is compiled in via `include_dir!` — **`cargo build` after every change under `web/`** before any Python test.
- Time is the device's local wall clock: `chrono::Local::now().naive_local()`.
- Weekdays: ISO numbers in the API (Monday = 1 … Sunday = 7); in the DB a bitmask, bit 0 = Monday … bit 6 = Sunday. Weekdays name the day a window **starts**.
- Minutes: `start_minute` 0..=1439, `end_minute` 1..=1440. `end_minute <= start_minute` means the window crosses midnight. An end of `00:00` or `24:00` is stored as 1440 (end of day); after that normalisation `start == end` is refused.
- At most 50 windows per display.
- First matching window in list order wins; otherwise the default; otherwise nothing (idle screen).
- A boundary switches immediately, like a reassignment.
- One word everywhere: **window** (`schedule_windows`, `Window`, "Zeitfenster" in the UI).
- Error bodies are `{ "error": "<German, names the row>" }` with `400`. A request body that does not deserialise (wrong type, unknown field) is axum's `422`.
- Routes are operator-only: in neither `is_display_path` nor `cast::is_cast_public_path`.
- `notify_one()`, never `notify_waiters()`.
- UI: `createElement`/`textContent` via the page's `el()` helper, **never `innerHTML` interpolation**; the per-card `dirty` rule stays.
- Git: no `Co-Authored-By` / `Claude-Session` trailers (user's global rule).
- Python suites: stop any local instance first. `test_display.py` uses CDP `9242`+`9243` and must not run concurrently with `test_webhook.py` or `test_castscreens.py`.
- A test about a specific screen names the **non-primary** one (`werkstatt`, not `foyer`).

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `src/schedule/mod.rs` | create | `Window`, `Active`, `active`, `next_boundary`, `overlaps`, `parse_time`, `format_time`, weekday masks, `validate`, `load`, `now`, `active_playlist` |
| `src/schedule/api.rs` | create | `body`, `get_schedule`, `put_schedule`, the transactional write |
| `src/main.rs` | modify | `mod schedule;` |
| `src/db.rs` | modify | rename column, create `schedule_windows`, tests |
| `src/display.rs` | modify | `default_playlist_id` everywhere, `known_display` helper, schedule route, list embeds schedule, `UpdateDisplay` without `playlist_id` |
| `src/browser.rs` | modify | resolve through `schedule::active_playlist`, boundary branch |
| `web/displays.html` | modify | Zeitplan editor |
| `web/admin.html`, `web/playlist.html` | modify | read `schedule.now.playlist_id` |
| `tests/cast/test_display.py` | modify | helpers, `[73]`, new `[78]` (API) and `[79]` (loop) |
| `tests/cast/test_overlay.py`, `test_media.py`, `test_webhook.py` | modify | `assign` helpers |
| `CLAUDE.md`, `README.md`, `docs/features.md`, `docs/roadmap.md`, `tests/cast/README.md`, the spec | modify | docs |

---

### Task 1: The rules, as pure functions

**Files:**
- Create: `src/schedule/mod.rs`, `src/schedule/api.rs` (empty module for now)
- Modify: `src/main.rs:1-16` (module list)

**Interfaces:**
- Produces: `crate::schedule::{Window, Active, WindowInput, MAX_WINDOWS, active, next_boundary, overlaps, parse_time, format_time, mask_from_iso, iso_from_mask, validate}` with the signatures below. Later tasks use exactly these.

- [ ] **Step 1: Create the module with its tests first**

`src/schedule/api.rs`:

```rust
//! `GET`/`PUT /api/displays/{name}/schedule`.
```

`src/schedule/mod.rs` — the types, stub functions that compile, and the tests:

```rust
//! Dayparting: which playlist a display shows, by weekday and time of day.
//!
//! The rules are pure functions over `Window`s and a local wall-clock time, so
//! they are tested without a database or a runtime.

pub mod api;

use chrono::{Datelike, Duration as ChronoDuration, NaiveDateTime, NaiveTime, Timelike};
use serde::Deserialize;

/// A timetable longer than this is a mistake, and the list is re-read on every
/// wake of the control loop.
pub const MAX_WINDOWS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Bit 0 = Monday … bit 6 = Sunday: the days a window *starts* on.
    pub weekdays: u8,
    /// 0..=1439.
    pub start_minute: u16,
    /// 1..=1440. At or before `start_minute`, the window crosses midnight.
    pub end_minute: u16,
    pub playlist_id: i64,
}

/// What is live: the playlist, and the window that made it so (`None` when the
/// default applies, or nothing does).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Active {
    pub playlist_id: Option<i64>,
    pub window: Option<usize>,
}

/// One row of a `PUT`, before validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowInput {
    pub weekdays: Vec<u8>,
    pub from: String,
    pub to: String,
    pub playlist_id: i64,
}

pub fn active(_default: Option<i64>, _windows: &[Window], _now: NaiveDateTime) -> Active {
    unimplemented!()
}

pub fn next_boundary(_windows: &[Window], _now: NaiveDateTime) -> Option<NaiveDateTime> {
    unimplemented!()
}

pub fn overlaps(_windows: &[Window]) -> Vec<(usize, usize)> {
    unimplemented!()
}

pub fn parse_time(_raw: &str, _is_end: bool) -> Option<u16> {
    unimplemented!()
}

pub fn format_time(_minute: u16) -> String {
    unimplemented!()
}

pub fn mask_from_iso(_days: &[u8]) -> Option<u8> {
    unimplemented!()
}

pub fn iso_from_mask(_mask: u8) -> Vec<u8> {
    unimplemented!()
}

pub fn validate(_inputs: &[WindowInput]) -> Result<Vec<Window>, String> {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    /// 2026-09-21 is a Monday.
    fn at(day_of_month: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 9, day_of_month)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    const MON_FRI: u8 = 0b0011111;
    const FRI: u8 = 0b0010000;
    const SUN: u8 = 0b1000000;
    const EVERY_DAY: u8 = 0b1111111;

    fn window(weekdays: u8, from: u16, to: u16, playlist_id: i64) -> Window {
        Window { weekdays, start_minute: from, end_minute: to, playlist_id }
    }

    #[test]
    fn the_default_applies_outside_every_window() {
        let office = [window(MON_FRI, 8 * 60, 18 * 60, 3)];
        assert_eq!(active(Some(1), &office, at(21, 7, 59)),
                   Active { playlist_id: Some(1), window: None });
        assert_eq!(active(Some(1), &office, at(21, 8, 0)),
                   Active { playlist_id: Some(3), window: Some(0) });
        // [start, end): 18:00 is already outside 08:00–18:00.
        assert_eq!(active(Some(1), &office, at(21, 18, 0)).playlist_id, Some(1));
        // Saturday is not listed.
        assert_eq!(active(Some(1), &office, at(26, 12, 0)).playlist_id, Some(1));
        // No default and no window: nothing, which is the idle screen.
        assert_eq!(active(None, &office, at(26, 12, 0)).playlist_id, None);
    }

    #[test]
    fn the_first_matching_window_wins() {
        let windows = [window(MON_FRI, 12 * 60, 13 * 60, 7), window(MON_FRI, 8 * 60, 18 * 60, 3)];
        assert_eq!(active(Some(1), &windows, at(21, 12, 30)),
                   Active { playlist_id: Some(7), window: Some(0) });
        assert_eq!(active(Some(1), &windows, at(21, 14, 0)),
                   Active { playlist_id: Some(3), window: Some(1) });
    }

    #[test]
    fn a_window_crossing_midnight_belongs_to_the_day_it_starts() {
        let friday_night = [window(FRI, 22 * 60, 6 * 60, 4)];
        // Friday 23:00 and Saturday 05:59 are inside…
        assert_eq!(active(None, &friday_night, at(25, 23, 0)).playlist_id, Some(4));
        assert_eq!(active(None, &friday_night, at(26, 5, 59)).playlist_id, Some(4));
        // …Saturday 06:00 is not, and neither is Friday 05:00: that morning
        // belongs to a Thursday night nobody listed.
        assert_eq!(active(None, &friday_night, at(26, 6, 0)).playlist_id, None);
        assert_eq!(active(None, &friday_night, at(25, 5, 0)).playlist_id, None);
    }

    #[test]
    fn sunday_night_runs_into_monday_morning() {
        let sunday_night = [window(SUN, 22 * 60, 6 * 60, 4)];
        assert_eq!(active(None, &sunday_night, at(28, 5, 59)).playlist_id, Some(4));
    }

    #[test]
    fn a_whole_day_is_midnight_to_midnight() {
        let all_day = [window(EVERY_DAY, 0, 1440, 5)];
        assert_eq!(active(None, &all_day, at(21, 0, 0)).playlist_id, Some(5));
        assert_eq!(active(None, &all_day, at(21, 23, 59)).playlist_id, Some(5));
    }

    #[test]
    fn the_next_boundary_is_the_soonest_start_or_end() {
        let office = [window(MON_FRI, 8 * 60, 18 * 60, 3)];
        assert_eq!(next_boundary(&office, at(21, 7, 0)), Some(at(21, 8, 0)));
        assert_eq!(next_boundary(&office, at(21, 9, 0)), Some(at(21, 18, 0)));
        // Friday evening: the next one is Monday morning.
        assert_eq!(next_boundary(&office, at(25, 19, 0)), Some(at(28, 8, 0)));
        // Strictly after: standing on a boundary looks for the next one.
        assert_eq!(next_boundary(&office, at(21, 8, 0)), Some(at(21, 18, 0)));
    }

    #[test]
    fn the_end_of_a_window_that_started_yesterday_is_a_boundary() {
        let friday_night = [window(FRI, 22 * 60, 6 * 60, 4)];
        assert_eq!(next_boundary(&friday_night, at(26, 1, 0)), Some(at(26, 6, 0)));
    }

    #[test]
    fn no_windows_means_no_boundary() {
        assert_eq!(next_boundary(&[], at(21, 12, 0)), None);
    }

    #[test]
    fn overlapping_windows_with_different_playlists_are_reported() {
        let windows = [window(MON_FRI, 8 * 60, 18 * 60, 3), window(MON_FRI, 12 * 60, 13 * 60, 7)];
        assert_eq!(overlaps(&windows), vec![(0, 1)]);
    }

    #[test]
    fn overlapping_windows_naming_the_same_playlist_are_harmless() {
        let windows = [window(MON_FRI, 8 * 60, 18 * 60, 3), window(MON_FRI, 12 * 60, 13 * 60, 3)];
        assert!(overlaps(&windows).is_empty());
    }

    #[test]
    fn adjacent_windows_do_not_overlap() {
        let windows = [window(MON_FRI, 8 * 60, 12 * 60, 3), window(MON_FRI, 12 * 60, 18 * 60, 7)];
        assert!(overlaps(&windows).is_empty());
    }

    #[test]
    fn a_night_window_overlaps_the_next_mornings_window() {
        let windows = [window(SUN, 22 * 60, 6 * 60, 4), window(0b0000001, 5 * 60, 9 * 60, 3)];
        // Sunday 22:00–Monday 06:00 meets Monday 05:00–09:00, across the week's end.
        assert_eq!(overlaps(&windows), vec![(0, 1)]);
    }

    #[test]
    fn times_parse_and_the_end_of_the_day_is_1440() {
        assert_eq!(parse_time("08:00", false), Some(480));
        assert_eq!(parse_time("23:59", false), Some(1439));
        assert_eq!(parse_time("00:00", false), Some(0));
        assert_eq!(parse_time("24:00", true), Some(1440));
        assert_eq!(parse_time("00:00", true), Some(1440));
        for bad in ["24:00", "8:00", "08:60", "25:00", "ab:cd", "", "08-00"] {
            assert_eq!(parse_time(bad, false), None, "{bad:?}");
        }
        assert_eq!(parse_time("24:30", true), None);
        assert_eq!(format_time(480), "08:00");
        assert_eq!(format_time(1440), "24:00");
    }

    #[test]
    fn weekdays_convert_both_ways() {
        assert_eq!(mask_from_iso(&[1, 2, 3, 4, 5]), Some(MON_FRI));
        assert_eq!(mask_from_iso(&[7]), Some(SUN));
        assert_eq!(mask_from_iso(&[]), None);
        assert_eq!(mask_from_iso(&[0]), None);
        assert_eq!(mask_from_iso(&[8]), None);
        assert_eq!(iso_from_mask(MON_FRI), vec![1, 2, 3, 4, 5]);
    }

    fn input(weekdays: &[u8], from: &str, to: &str) -> WindowInput {
        WindowInput { weekdays: weekdays.to_vec(), from: from.into(), to: to.into(), playlist_id: 1 }
    }

    #[test]
    fn validation_names_the_row() {
        let ok = validate(&[input(&[1], "08:00", "18:00"), input(&[5], "22:00", "00:00")]).unwrap();
        assert_eq!(ok[1].end_minute, 1440);

        let err = validate(&[input(&[1], "08:00", "18:00"), input(&[], "08:00", "18:00")]).unwrap_err();
        assert!(err.starts_with("Zeile 2:"), "{err}");
        let err = validate(&[input(&[1], "8 Uhr", "18:00")]).unwrap_err();
        assert!(err.starts_with("Zeile 1:"), "{err}");
        let err = validate(&[input(&[1], "08:00", "08:00")]).unwrap_err();
        assert!(err.contains("gleich"), "{err}");
        // Midnight to midnight is not "start == end" but a whole day.
        assert!(validate(&[input(&[1], "00:00", "00:00")]).is_ok());

        let too_many: Vec<_> = (0..=MAX_WINDOWS).map(|_| input(&[1], "08:00", "09:00")).collect();
        assert!(validate(&too_many).is_err());
    }
}
```

In `src/main.rs`, add after `mod playlists;`:

```rust
mod schedule;
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test schedule::`
Expected: every test panics with `not implemented`.

- [ ] **Step 3: Implement the functions**

Replace the eight stubs in `src/schedule/mod.rs` with:

```rust
impl Window {
    fn starts_on(&self, day: u32) -> bool {
        self.weekdays & (1 << day) != 0
    }

    fn crosses_midnight(&self) -> bool {
        self.end_minute <= self.start_minute
    }

    fn matches(&self, now: NaiveDateTime) -> bool {
        let day = now.weekday().num_days_from_monday();
        let minute = (now.hour() * 60 + now.minute()) as u16;
        if self.crosses_midnight() {
            let yesterday = (day + 6) % 7;
            (self.starts_on(day) && minute >= self.start_minute)
                || (self.starts_on(yesterday) && minute < self.end_minute)
        } else {
            self.starts_on(day) && self.start_minute <= minute && minute < self.end_minute
        }
    }

    /// The window on a week-minute axis (0..10080), split where it crosses
    /// midnight -- and where a Sunday-night window runs into Monday.
    fn week_ranges(&self) -> Vec<(u32, u32)> {
        let (start, end) = (self.start_minute as u32, self.end_minute as u32);
        let mut out = Vec::new();
        for day in 0..7u32 {
            if !self.starts_on(day) {
                continue;
            }
            let base = day * 1440;
            if self.crosses_midnight() {
                out.push((base + start, base + 1440));
                let next = ((day + 1) % 7) * 1440;
                out.push((next, next + end));
            } else {
                out.push((base + start, base + end));
            }
        }
        out
    }
}

/// The first window in order that matches `now`, else the default.
pub fn active(default: Option<i64>, windows: &[Window], now: NaiveDateTime) -> Active {
    match windows.iter().position(|w| w.matches(now)) {
        Some(index) => Active { playlist_id: Some(windows[index].playlist_id), window: Some(index) },
        None => Active { playlist_id: default, window: None },
    }
}

/// The soonest instant strictly after `now` at which any window starts or ends.
///
/// Starts from yesterday, because a window that began last night ends this
/// morning, and looks eight days ahead, which covers every weekly pattern.
pub fn next_boundary(windows: &[Window], now: NaiveDateTime) -> Option<NaiveDateTime> {
    let today = now.date();
    let mut soonest: Option<NaiveDateTime> = None;
    for window in windows {
        for offset in -1..=8i64 {
            let date = today + ChronoDuration::days(offset);
            if !window.starts_on(date.weekday().num_days_from_monday()) {
                continue;
            }
            let midnight = date.and_time(NaiveTime::MIN);
            let start = midnight + ChronoDuration::minutes(window.start_minute as i64);
            let end_day = if window.crosses_midnight() { 1 } else { 0 };
            let end = midnight
                + ChronoDuration::days(end_day)
                + ChronoDuration::minutes(window.end_minute as i64);
            for instant in [start, end] {
                if instant > now && soonest.map_or(true, |s| instant < s) {
                    soonest = Some(instant);
                }
            }
        }
    }
    soonest
}

/// Pairs `(i, j)`, `i < j`, that meet somewhere in the week and name different
/// playlists -- meaning `i` hides `j` where they meet. Two windows naming the
/// same playlist overlap harmlessly and are not reported.
pub fn overlaps(windows: &[Window]) -> Vec<(usize, usize)> {
    let ranges: Vec<Vec<(u32, u32)>> = windows.iter().map(Window::week_ranges).collect();
    let mut out = Vec::new();
    for i in 0..windows.len() {
        for j in (i + 1)..windows.len() {
            if windows[i].playlist_id == windows[j].playlist_id {
                continue;
            }
            let meet = ranges[i]
                .iter()
                .any(|a| ranges[j].iter().any(|b| a.0 < b.1 && b.0 < a.1));
            if meet {
                out.push((i, j));
            }
        }
    }
    out
}

/// `HH:MM` to minutes since midnight. As an end, `00:00` and `24:00` are the end
/// of the day (1440): a time input cannot show `24:00`, and "until midnight" is
/// what anybody typing 22:00–00:00 means.
pub fn parse_time(raw: &str, is_end: bool) -> Option<u16> {
    let (hours, minutes) = raw.trim().split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let hours: u16 = hours.parse().ok()?;
    let minutes: u16 = minutes.parse().ok()?;
    if minutes >= 60 {
        return None;
    }
    let minute = hours * 60 + minutes;
    if is_end && (minute == 0 || minute == 1440) {
        return Some(1440);
    }
    (hours < 24).then_some(minute)
}

pub fn format_time(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

/// ISO weekday numbers (Monday = 1) to the stored mask. `None` for an empty list
/// or a number outside 1..=7.
pub fn mask_from_iso(days: &[u8]) -> Option<u8> {
    if days.is_empty() {
        return None;
    }
    let mut mask = 0u8;
    for &day in days {
        if !(1..=7).contains(&day) {
            return None;
        }
        mask |= 1 << (day - 1);
    }
    Some(mask)
}

pub fn iso_from_mask(mask: u8) -> Vec<u8> {
    (0..7u8).filter(|bit| mask & (1 << bit) != 0).map(|bit| bit + 1).collect()
}

/// A `PUT`'s rows, checked and converted, or the refusal naming the first bad
/// row (counted from 1, as the operator sees them).
pub fn validate(inputs: &[WindowInput]) -> Result<Vec<Window>, String> {
    if inputs.len() > MAX_WINDOWS {
        return Err(format!("Höchstens {MAX_WINDOWS} Zeitfenster pro Display."));
    }
    inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let row = index + 1;
            let weekdays = mask_from_iso(&input.weekdays).ok_or_else(|| {
                format!("Zeile {row}: mindestens einen Wochentag wählen (1 = Montag … 7 = Sonntag).")
            })?;
            let start_minute = parse_time(&input.from, false)
                .ok_or_else(|| format!("Zeile {row}: „{}“ ist keine Uhrzeit (HH:MM).", input.from))?;
            let end_minute = parse_time(&input.to, true)
                .ok_or_else(|| format!("Zeile {row}: „{}“ ist keine Uhrzeit (HH:MM).", input.to))?;
            if start_minute == end_minute {
                return Err(format!(
                    "Zeile {row}: Beginn und Ende sind gleich; ein ganzer Tag ist 00:00–24:00."
                ));
            }
            Ok(Window { weekdays, start_minute, end_minute, playlist_id: input.playlist_id })
        })
        .collect()
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test schedule::`
Expected: all 15 pass. `cargo build` may warn that items are unused; Task 3 uses them.

- [ ] **Step 5: Commit**

```bash
git add src/schedule/ src/main.rs
git commit -m "Add the dayparting rules as pure functions"
```

---

### Task 2: Rename the column, add the table, read the timetable

**Files:**
- Modify: `src/db.rs` (displays `CREATE TABLE` ~203, after the `assignment_decided` probe ~231, tests)
- Modify: `src/display.rs` (every `playlist_id` that means the display's playlist: lines ~296, 357, 392-399, 433, 579, tests ~747-894)
- Modify: `src/browser.rs:228` and `:1038`
- Modify: `src/schedule/mod.rs` (add `load`, `now`, `active_playlist`)

**Interfaces:**
- Consumes: Task 1's `Window`, `Active`, `active`.
- Produces: column `displays.default_playlist_id`; table `schedule_windows(id, display, position, weekdays, start_minute, end_minute, playlist_id)`; `pub async fn schedule::load(pool: &sqlx::SqlitePool, display: &str) -> Result<(Option<i64>, Vec<Window>), sqlx::Error>`; `pub fn schedule::now() -> NaiveDateTime`; `pub async fn schedule::active_playlist(pool: &sqlx::SqlitePool, display: &str) -> Result<Active, sqlx::Error>`.

This task changes no HTTP behaviour: `GET/PUT /api/displays` still speak `playlist_id` on the wire until Task 3.

- [ ] **Step 1: Write the failing migration tests**

In `src/db.rs`'s test module, replace `deleting_a_playlist_unassigns_it_from_a_display` with:

```rust
    #[tokio::test]
    async fn deleting_a_playlist_unassigns_it_and_deletes_its_windows() {
        let pool = pool().await;
        sqlx::query("INSERT INTO playlists (id, name) VALUES (1, 'Foyer'), (2, 'Nacht')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO displays (name, default_playlist_id) VALUES ('foyer', 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO schedule_windows (display, position, weekdays, start_minute, end_minute, playlist_id)
             VALUES ('foyer', 0, 127, 1320, 360, 1), ('foyer', 1, 127, 0, 1440, 2)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("DELETE FROM playlists WHERE id = 1").execute(&pool).await.unwrap();

        let default: Option<i64> =
            sqlx::query_scalar("SELECT default_playlist_id FROM displays WHERE name = 'foyer'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(default.is_none(), "ON DELETE SET NULL did not fire -- is PRAGMA foreign_keys on?");
        let windows: Vec<i64> = sqlx::query_scalar("SELECT playlist_id FROM schedule_windows")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(windows, vec![2], "the window naming the deleted playlist must go with it");
    }

    #[tokio::test]
    async fn a_display_assigned_before_dayparting_keeps_its_playlist_as_the_default() {
        let pool = sqlx::SqlitePool::connect("sqlite:file:db_display_rename?mode=memory&cache=shared")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE playlists (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE displays (
                id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE, label TEXT,
                playlist_id INTEGER, assignment_decided BOOLEAN DEFAULT 0,
                FOREIGN KEY(playlist_id) REFERENCES playlists(id) ON DELETE SET NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO playlists (id, name) VALUES (5, 'Standard')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO displays (name, playlist_id, assignment_decided) VALUES ('left', 5, 1)")
            .execute(&pool)
            .await
            .unwrap();

        run_migrations(&pool).await.unwrap();
        run_migrations(&pool).await.unwrap();

        let default: Option<i64> =
            sqlx::query_scalar("SELECT default_playlist_id FROM displays WHERE name = 'left'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(default, Some(5));
        assert_eq!(count(&pool, "SELECT count(*) FROM schedule_windows").await, 0);
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test db::tests`
Expected: both fail with `no such column: default_playlist_id` / `no such table: schedule_windows`.

- [ ] **Step 3: Migrate**

In `src/db.rs`, change the `displays` `CREATE TABLE` to name the column for what it is:

```rust
        "CREATE TABLE IF NOT EXISTS displays (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            name                TEXT NOT NULL UNIQUE,
            label               TEXT,
            default_playlist_id INTEGER,
            assignment_decided  BOOLEAN DEFAULT 0,
            FOREIGN KEY(default_playlist_id) REFERENCES playlists(id) ON DELETE SET NULL
        );"
```

Directly after the `has_assignment_decided` block, add:

```rust
    // `playlist_id` became the *default* playlist when displays got a timetable,
    // and is named for what it is. Renamed in place, so its foreign key, its
    // `ON DELETE SET NULL` and every stored assignment carry over. `?` rather
    // than `let _`: a database left half-way would have every query in
    // `display.rs` naming a column that is not there.
    let has_old_assignment: bool = sqlx::query(
        "SELECT count(*) FROM pragma_table_info('displays') WHERE name='playlist_id'",
    )
    .fetch_one(pool)
    .await
    .map(|row| row.get::<i32, _>(0) > 0)
    .unwrap_or(false);

    if has_old_assignment {
        sqlx::query("ALTER TABLE displays RENAME COLUMN playlist_id TO default_playlist_id")
            .execute(pool)
            .await?;
    }

    // A display's timetable: ordered windows, the first match wins. Deleting a
    // playlist deletes its windows -- a window with no playlist would be a
    // setting that silently does nothing -- and deleting a display row takes its
    // timetable with it.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schedule_windows (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            display      TEXT NOT NULL REFERENCES displays(name) ON DELETE CASCADE,
            position     INTEGER NOT NULL,
            weekdays     INTEGER NOT NULL,
            start_minute INTEGER NOT NULL,
            end_minute   INTEGER NOT NULL,
            playlist_id  INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE
        );",
    )
    .execute(pool)
    .await?;
```

- [ ] **Step 4: Follow the rename through the code**

In `src/display.rs`, replace `playlist_id` with `default_playlist_id` in every SQL string that reads or writes `displays` — `register` (the `undecided` count), `inherit_oldest_playlist` (the `SELECT` and the `UPDATE … WHERE name = ? AND default_playlist_id IS NULL`), `list` (the `SELECT name, label, …`), `update` (the `UPDATE displays SET … assignment_decided = 1`), and the tests (`assignment()` and the three `UPDATE displays SET playlist_id = NULL …` statements). Run `grep -n "playlist_id" src/display.rs` afterwards: every remaining hit must be a Rust identifier, the JSON key `"playlist_id"` in `list`/`update` (changed in Task 3), or a doc comment — update doc comments that say "`playlist_id`" about the display to say "the default playlist".

In `src/browser.rs`, the two SQL strings: `"SELECT playlist_id FROM displays WHERE name = ?"` → `"SELECT default_playlist_id FROM displays WHERE name = ?"`, and `JOIN displays d ON d.playlist_id = p.playlist_id` → `JOIN displays d ON d.default_playlist_id = p.playlist_id`. (Task 4 replaces both with the resolver.)

- [ ] **Step 5: Add the storage reads**

At the end of `src/schedule/mod.rs`, before `#[cfg(test)]`:

```rust
/// A display's default playlist and its windows in order.
///
/// A display with no row reads as no default and no windows -- the idle screen,
/// which is what the loop showed for an unknown row before timetables existed.
pub async fn load(
    pool: &sqlx::SqlitePool,
    display: &str,
) -> Result<(Option<i64>, Vec<Window>), sqlx::Error> {
    let default: Option<i64> =
        sqlx::query_scalar::<_, Option<i64>>("SELECT default_playlist_id FROM displays WHERE name = ?")
            .bind(display)
            .fetch_optional(pool)
            .await?
            .flatten();
    let rows: Vec<(i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT weekdays, start_minute, end_minute, playlist_id FROM schedule_windows
         WHERE display = ? ORDER BY position ASC, id ASC",
    )
    .bind(display)
    .fetch_all(pool)
    .await?;
    let windows = rows
        .into_iter()
        .map(|(weekdays, start, end, playlist_id)| Window {
            weekdays: weekdays as u8,
            start_minute: start as u16,
            end_minute: end as u16,
            playlist_id,
        })
        .collect();
    Ok((default, windows))
}

/// The device's local wall clock, which is what a timetable is written in.
pub fn now() -> NaiveDateTime {
    chrono::Local::now().naive_local()
}

/// What `display` should be playing right now. **The only resolver**: nothing
/// else reads `default_playlist_id` to decide what to play.
pub async fn active_playlist(pool: &sqlx::SqlitePool, display: &str) -> Result<Active, sqlx::Error> {
    let (default, windows) = load(pool, display).await?;
    Ok(active(default, &windows, now()))
}
```

- [ ] **Step 6: Run everything**

Run: `cargo test`
Expected: all pass, including the two new migration tests and the unchanged `display.rs` registration tests (now reading `default_playlist_id`).

Run: `cargo build && cd tests/cast && python3 test_display.py 70 73; cd ../..`
Expected: `ALL PASSED` — the wire format is unchanged, so this proves the rename did not move any screen.

- [ ] **Step 7: Commit**

```bash
git add src/db.rs src/display.rs src/browser.rs src/schedule/mod.rs
git commit -m "Rename a display's playlist to its default, add timetable storage"
```

---

### Task 3: The schedule API, and the display API without `playlist_id`

**Files:**
- Modify: `src/schedule/api.rs`
- Modify: `src/display.rs` — new `known_display`, `routes()`, `list`, `UpdateDisplay`, `update`
- Modify: `tests/cast/test_display.py` (helpers ~276-293, `[73]` ~423, new `case_78`, `CASES`)
- Modify: `tests/cast/test_overlay.py:44-53`, `tests/cast/test_media.py:74-83`, `tests/cast/test_webhook.py:258-269` (assign helpers)

**Interfaces:**
- Consumes: Task 1 (`validate`, `overlaps`, `format_time`, `iso_from_mask`, `active`, `WindowInput`), Task 2 (`load`, `now`).
- Produces: `GET`/`PUT /api/displays/{name}/schedule` returning `{ default_playlist_id, windows: [{weekdays, from, to, playlist_id}], overlaps: [[i, j]], now: { playlist_id, window } }`; `GET /api/displays` rows `{ name, label, declared, schedule }`; `PUT /api/displays/{name}` accepting only `label`; `pub(crate) async fn display::known_display(state: &AppState, name: &str) -> Result<(), Response>`; `pub(crate) async fn schedule::api::body(pool: &sqlx::SqlitePool, display: &str) -> Result<serde_json::Value, sqlx::Error>`. Python: `test_display.put_schedule(display, default, windows)` and `assign(display, playlist_id)` (now via the schedule route).

- [ ] **Step 1: Move the test helpers to the new API and write the failing API case**

In `tests/cast/test_display.py` replace `assign` and `playlist_id_of`:

```python
def put_schedule(display, default, windows=()):
    """The whole timetable of one screen, as the displays page saves it."""
    return http("PUT", f"/api/displays/{display}/schedule",
                {"default_playlist_id": default, "windows": list(windows)})


def assign(display, playlist_id):
    """A screen's default playlist, and no windows."""
    return put_schedule(display, playlist_id)


def schedule_of(display):
    for row in http("GET", "/api/displays")[1] or []:
        if row["name"] == display:
            return row.get("schedule") or {}
    return {}


def playlist_id_of(display):
    for row in http("GET", "/api/displays")[1] or []:
        if row["name"] == display:
            return (row.get("schedule") or {}).get("default_playlist_id")
    return "no such display"
```

In `case_73`, replace `http("PUT", "/api/displays/foyer", {"playlist_id": None})` with `assign("foyer", None)`.

Add, before `CASES`:

```python
def case_78():
    print("\n[78] a screen's timetable is stored in order, checked, and read back")
    # The routes and the checks are an HTTP handler's; no loop needed. On the
    # non-primary screen, so an implementation that resolved every request to
    # `displays[0]` would read the foyer's empty timetable here and fail.
    with Alone():
        office = make_playlist("Büro")
        night = make_playlist("Nacht")
        lunch = make_playlist("Mittag")
        windows = [
            {"weekdays": [1, 2, 3, 4, 5], "from": "12:00", "to": "13:00", "playlist_id": lunch},
            {"weekdays": [1, 2, 3, 4, 5], "from": "08:00", "to": "18:00", "playlist_id": office},
            {"weekdays": [5], "from": "22:00", "to": "00:00", "playlist_id": night},
        ]
        status, body = put_schedule("werkstatt", night, windows)
        check("a timetable saves", status == 200, (status, body))
        check("the answer is the timetable, windows in order",
              [w["playlist_id"] for w in (body or {}).get("windows", [])] == [lunch, office, night],
              body)
        check("an end of 00:00 reads back as the end of the day",
              body["windows"][2]["to"] == "24:00", body["windows"][2])
        check("the lunch window hides the office one where they meet",
              body.get("overlaps") == [[0, 1]], body.get("overlaps"))
        check("and what is live now is reported",
              "playlist_id" in (body.get("now") or {}) and "window" in body["now"], body.get("now"))

        stored = schedule_of("werkstatt")
        check("the list of displays embeds the same timetable",
              stored.get("default_playlist_id") == night and len(stored.get("windows", [])) == 3,
              stored)
        check("and the other screen's is untouched",
              schedule_of("foyer").get("windows") == [], schedule_of("foyer"))

        print("\n[78b] a bad row is refused, names itself, and writes nothing")
        refusals = [
            ({"weekdays": [], "from": "08:00", "to": "18:00", "playlist_id": office}, "Zeile 2"),
            ({"weekdays": [8], "from": "08:00", "to": "18:00", "playlist_id": office}, "Zeile 2"),
            ({"weekdays": [1], "from": "8 Uhr", "to": "18:00", "playlist_id": office}, "Zeile 2"),
            ({"weekdays": [1], "from": "08:00", "to": "08:00", "playlist_id": office}, "Zeile 2"),
            ({"weekdays": [1], "from": "08:00", "to": "18:00", "playlist_id": 99999}, "Zeile 2"),
        ]
        for bad, row in refusals:
            status, body = put_schedule("werkstatt", office, [windows[0], bad])
            check(f"{bad} is a 400 naming {row}",
                  status == 400 and row in (body or {}).get("error", ""), (status, body))
        after = schedule_of("werkstatt")
        check("and none of them changed anything -- the default included",
              after.get("default_playlist_id") == night and len(after.get("windows", [])) == 3,
              after)

        status, body = put_schedule("werkstatt", 99999, [])
        check("an unknown default is a 400 too", status == 400 and "error" in (body or {}),
              (status, body))

        status, _ = http("PUT", "/api/displays/werkstatt/schedule", {"windows": []})
        check("leaving out the default is refused rather than read as none", status == 400, status)

        print("\n[78c] replacing the timetable removes what was not sent")
        status, body = put_schedule("werkstatt", office, [windows[1]])
        check("one window left", status == 200 and len(body["windows"]) == 1, body)
        check("no overlap left to warn about", body["overlaps"] == [], body)

        print("\n[78d] the display's own route no longer takes a playlist")
        status, _ = http("PUT", "/api/displays/werkstatt", {"playlist_id": night})
        check("a script still sending playlist_id is refused, not ignored",
              status == 422, status)
        check("and nothing moved", playlist_id_of("werkstatt") == office, playlist_id_of("werkstatt"))
        status, _ = http("PUT", "/api/displays/werkstatt", {"label": "Werkstatt hinten"})
        check("the label still saves there", status == 200, status)
```

and extend `CASES`:

```python
CASES = [case_70, case_71, case_72, case_73, case_74, case_75, case_76, case_77, case_78]
```

In `tests/cast/test_overlay.py` (`assign`), `tests/cast/test_media.py` (`a_playlist`) and `tests/cast/test_webhook.py` (the helper around line 266), replace the loop body:

```python
    for row in http("GET", "/api/displays", port=port)[1] or []:
        if (row.get("schedule") or {}).get("default_playlist_id") != playlist_id:
            http("PUT", f"/api/displays/{row['name']}/schedule",
                 {"default_playlist_id": playlist_id, "windows": []}, port=port)
```

(`test_webhook.py`'s helper has no `port` parameter: drop `, port=port` from both calls there.)

- [ ] **Step 2: Run the new case to see it fail**

Run: `cargo build && cd tests/cast && python3 test_display.py 78; cd ../..`
Expected: `[78]` fails at "a timetable saves" with `405` (no such route).

- [ ] **Step 3: Extract the display lookup**

In `src/display.rs`, add above `routes()`:

```rust
/// Make sure `name` is a display this API may read or write, and that its row
/// exists.
///
/// A declared display is upserted rather than looked up, so a row deleted out of
/// band comes back instead of turning into a refusal an operator cannot act on.
/// A name this deployment does *not* declare is only reachable when it already
/// has a row -- the screen taken away whose timetable is being reassigned.
/// Anything else is a typo, and accepting it would write a row nothing reads,
/// answer `200`, and leave the operator believing a screen was configured.
pub(crate) async fn known_display(state: &AppState, name: &str) -> Result<(), Response> {
    if state.display(name).is_some() {
        if let Err(e) =
            sqlx::query("INSERT INTO displays (name) VALUES (?) ON CONFLICT(name) DO NOTHING")
                .bind(name)
                .execute(&state.pool)
                .await
        {
            tracing::error!("Failed to ensure display row for {}: {}", name, e);
        }
        return Ok(());
    }
    let known: i64 = sqlx::query_scalar("SELECT count(*) FROM displays WHERE name = ?")
        .bind(name)
        .fetch_one(&state.pool)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to look up display {}: {}", name, e);
            0
        });
    if known == 0 {
        return Err(unknown_here(state, name).await);
    }
    Ok(())
}
```

Replace `UpdateDisplay` and `update`:

```rust
/// What is about the display itself. Its playlist is not: that is the
/// timetable's, at `/api/displays/{name}/schedule`.
///
/// `deny_unknown_fields` so a script still sending the `playlist_id` this route
/// used to take is refused (`422`) rather than answered `200` for a change that
/// never happened.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateDisplay {
    /// `double_option`: without it a JSON `null` collapses into the outer
    /// `None` and reads as "field absent", so a label could only ever be
    /// cleared by sending `""` -- a rule nothing states and the next client
    /// would not guess.
    #[serde(default, deserialize_with = "crate::handlers::double_option")]
    label: Option<Option<String>>,
}

async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(payload): Json<UpdateDisplay>,
) -> Response {
    if let Err(response) = known_display(&state, &name).await {
        return response;
    }
    if let Some(label) = &payload.label {
        // An empty or absent label is stored as SQL `NULL` rather than "": the
        // read path falls back to the name only on `NULL`, so an operator
        // clearing the field would otherwise get a nameless row in the list.
        let stored = label.as_deref().map(str::trim).filter(|l| !l.is_empty());
        if let Err(e) = sqlx::query("UPDATE displays SET label = ? WHERE name = ?")
            .bind(stored)
            .bind(&name)
            .execute(&state.pool)
            .await
        {
            tracing::error!("Failed to set label for display {}: {}", name, e);
        }
    }
    Json(json!({ "ok": true })).into_response()
}
```

Replace `list` so each row embeds its timetable:

```rust
async fn list(State(state): State<AppState>) -> Response {
    let failed = |e: sqlx::Error| {
        tracing::error!("Failed to list displays: {}", e);
        // Swallowing this would render every declared screen as unassigned,
        // which is a real state an operator acts on. Saying nothing could be
        // read is the only honest answer.
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "Displays konnten nicht gelesen werden." })),
        )
            .into_response()
    };
    let rows = match sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT name, label FROM displays ORDER BY name ASC",
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => return failed(e),
    };

    // Driven off the declared displays, not off the table: a row for a screen
    // this deployment no longer declares must still be visible (so its timetable
    // can be reassigned) but must not claim to be attached.
    let mut entries: Vec<(String, Option<String>, bool)> = state
        .displays
        .iter()
        .map(|d| {
            let label = rows.iter().find(|(name, _)| name == &d.name).and_then(|(_, l)| l.clone());
            (d.name.clone(), label, true)
        })
        .collect();
    entries.extend(
        rows.iter()
            .filter(|(name, _)| state.display(name).is_none())
            .map(|(name, label)| (name.clone(), label.clone(), false)),
    );

    let mut out = Vec::with_capacity(entries.len());
    for (name, label, declared) in entries {
        let schedule = match crate::schedule::api::body(&state.pool, &name).await {
            Ok(schedule) => schedule,
            Err(e) => return failed(e),
        };
        out.push(json!({
            "label": label.unwrap_or_else(|| name.clone()),
            "name": name,
            "declared": declared,
            "schedule": schedule,
        }));
    }
    Json(out).into_response()
}
```

Add the route in `routes()`, after `/api/displays/{name}`:

```rust
        .route(
            "/api/displays/{name}/schedule",
            get(crate::schedule::api::get_schedule).put(crate::schedule::api::put_schedule),
        )
```

- [ ] **Step 4: Write the handlers**

`src/schedule/api.rs`:

```rust
//! `GET`/`PUT /api/displays/{name}/schedule`.
//!
//! The timetable is a resource of its own: default and windows together are the
//! whole answer to "what does this screen play, when", with their own validation
//! and their own derived state. Operator-only -- in neither `is_display_path`
//! nor `cast::is_cast_public_path` -- which is also what allows the path to name
//! the screen.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{active, format_time, iso_from_mask, load, now, overlaps, validate, Window, WindowInput};
use crate::models::AppState;

/// The timetable as `GET` answers it and `GET /api/displays` embeds it.
pub(crate) async fn body(pool: &sqlx::SqlitePool, display: &str) -> Result<Value, sqlx::Error> {
    let (default, windows) = load(pool, display).await?;
    let live = active(default, &windows, now());
    Ok(json!({
        "default_playlist_id": default,
        "windows": windows.iter().map(|w| json!({
            "weekdays": iso_from_mask(w.weekdays),
            "from": format_time(w.start_minute),
            "to": format_time(w.end_minute),
            "playlist_id": w.playlist_id,
        })).collect::<Vec<_>>(),
        "overlaps": overlaps(&windows).into_iter().map(|(i, j)| [i, j]).collect::<Vec<_>>(),
        "now": { "playlist_id": live.playlist_id, "window": live.window },
    }))
}

fn bad_request(message: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message.into() }))).into_response()
}

fn unreadable(display: &str, e: sqlx::Error) -> Response {
    tracing::error!("Failed to read the timetable of {}: {}", display, e);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "Zeitplan konnte nicht gelesen werden." })),
    )
        .into_response()
}

pub(crate) async fn get_schedule(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if let Err(response) = crate::display::known_display(&state, &name).await {
        return response;
    }
    match body(&state.pool, &name).await {
        Ok(value) => Json(value).into_response(),
        Err(e) => unreadable(&name, e),
    }
}

/// Both fields required. `Option` only so a missing one gets a sentence rather
/// than serde's `422`: a partial update of an ordered list has no good meaning,
/// and `default_playlist_id: null` ("no default") must not be confused with
/// "not sent".
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScheduleInput {
    #[serde(default, deserialize_with = "crate::handlers::double_option")]
    default_playlist_id: Option<Option<i64>>,
    windows: Option<Vec<WindowInput>>,
}

pub(crate) async fn put_schedule(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(input): Json<ScheduleInput>,
) -> Response {
    if let Err(response) = crate::display::known_display(&state, &name).await {
        return response;
    }
    let Some(default) = input.default_playlist_id else {
        return bad_request("default_playlist_id fehlt – null heißt „keine Standard-Playlist“.");
    };
    let Some(inputs) = input.windows else {
        return bad_request("windows fehlt – eine leere Liste heißt „kein Zeitfenster“.");
    };
    let windows = match validate(&inputs) {
        Ok(windows) => windows,
        Err(message) => return bad_request(message),
    };

    match write(&state.pool, &name, default, &windows).await {
        Ok(Written::Done) => {}
        Ok(Written::UnknownDefault) => return bad_request("Unbekannte Standard-Playlist."),
        Ok(Written::UnknownPlaylist(row)) => {
            return bad_request(format!("Zeile {row}: unbekannte Playlist."))
        }
        Err(e) => {
            tracing::error!("Failed to save the timetable of {}: {}", name, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Zeitplan konnte nicht gespeichert werden." })),
            )
                .into_response();
        }
    }

    // The loop re-resolves every pass, but poking it means a change to what is
    // live lands now rather than at the end of the item -- and it recomputes its
    // boundary timer from the new windows.
    if let Some(display) = state.display(&name) {
        display.playlist_signal.notify_one();
    }

    match body(&state.pool, &name).await {
        Ok(value) => Json(value).into_response(),
        Err(e) => unreadable(&name, e),
    }
}

enum Written {
    Done,
    UnknownDefault,
    /// Counted from 1, as the operator sees the rows.
    UnknownPlaylist(usize),
}

/// The whole timetable in one transaction. Every playlist is checked in the
/// statement that writes it, for the reason the item move does it: a check
/// before the write leaves a window for the playlist to go. An early return
/// drops `tx`, which rolls back -- the default included.
async fn write(
    pool: &sqlx::SqlitePool,
    display: &str,
    default: Option<i64>,
    windows: &[Window],
) -> Result<Written, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // `assignment_decided` in the same statement, as every assignment has it: an
    // operator's "(keine)" is a decision, and a power loss must not tear the
    // record of it from what was chosen.
    let updated = sqlx::query(
        "UPDATE displays SET default_playlist_id = ?1, assignment_decided = 1
         WHERE name = ?2 AND (?1 IS NULL OR EXISTS (SELECT 1 FROM playlists WHERE id = ?1))",
    )
    .bind(default)
    .bind(display)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(Written::UnknownDefault);
    }
    sqlx::query("DELETE FROM schedule_windows WHERE display = ?")
        .bind(display)
        .execute(&mut *tx)
        .await?;
    for (position, window) in windows.iter().enumerate() {
        let inserted = sqlx::query(
            "INSERT INTO schedule_windows
                 (display, position, weekdays, start_minute, end_minute, playlist_id)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6
             WHERE EXISTS (SELECT 1 FROM playlists WHERE id = ?6)",
        )
        .bind(display)
        .bind(position as i64)
        .bind(window.weekdays as i64)
        .bind(window.start_minute as i64)
        .bind(window.end_minute as i64)
        .bind(window.playlist_id)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            return Ok(Written::UnknownPlaylist(position + 1));
        }
    }
    tx.commit().await?;
    Ok(Written::Done)
}
```

- [ ] **Step 5: Build and run the API case and the suites whose helpers moved**

Run:

```bash
cargo build && cargo test
cd tests/cast
python3 test_display.py 70 71 72 73 78
python3 test_media.py
python3 test_overlay.py
cd ../..
```

Expected: `cargo test` green; each suite `ALL PASSED`.

- [ ] **Step 6: Commit**

```bash
git add src/schedule/api.rs src/display.rs tests/cast/test_display.py tests/cast/test_overlay.py tests/cast/test_media.py tests/cast/test_webhook.py
git commit -m "Give each display a timetable resource, drop playlist_id from its route"
```

---

### Task 4: The loop plays what the timetable says, and switches at a boundary

**Files:**
- Modify: `src/browser.rs` — top of the inner pass (~226-243), the per-item `select!` (~515-575), `is_playlist_item_active_now` (~1030)
- Modify: `tests/cast/test_display.py` — new `case_79`, `CASES`

**Interfaces:**
- Consumes: `schedule::{active_playlist, load, next_boundary, now}`.
- Produces: nothing new for other tasks.

- [ ] **Step 1: Write the failing end-to-end case**

In `tests/cast/test_display.py`, before `CASES`:

```python
def local_minute(offset=0):
    """`HH:MM` for the local minute `offset` minutes from now -- the device's
    clock, which is the one the controller reads."""
    t = time.localtime(time.time() + offset * 60)
    return f"{t.tm_hour:02d}:{t.tm_min:02d}"


EVERY_DAY = [1, 2, 3, 4, 5, 6, 7]


def case_79():
    print("\n[79] a window that covers now switches the screen to its playlist")
    # On the non-primary screen: the foyer is displays[0], and a loop that
    # resolved every screen's timetable through `state.primary()` would pass a
    # foyer-only case unchanged.
    with Screens():
        day = make_playlist("Tag")
        window_list = make_playlist("Fenster")
        foyer_list = make_playlist("Foyer")
        day_item = add_item(day)
        window_item = add_item(window_list)
        foyer_item = add_item(foyer_list)
        assign("foyer", foyer_list)

        put_schedule("werkstatt", day, [
            {"weekdays": EVERY_DAY, "from": local_minute(-2), "to": local_minute(30),
             "playlist_id": window_list},
        ])
        check("the workshop plays the window's playlist, not its default",
              until(lambda: item_of("werkstatt") == window_item),
              (item_of("werkstatt"), window_item))
        check("the foyer keeps its own", until(lambda: item_of("foyer") == foyer_item),
              (item_of("foyer"), foyer_item))
        now = schedule_of("werkstatt").get("now") or {}
        check("and the timetable says which window is live",
              now.get("playlist_id") == window_list and now.get("window") == 0, now)

        print("\n[79b] a window starting at the next minute interrupts the item on screen")
        # At least 20 s away, so "not yet" below is not a race with the minute.
        wait = 1 if time.localtime().tm_sec < 40 else 2
        # Taken now, from the same clock reading the window is built from: taken
        # after the wait below it could already be a minute later.
        boundary = (int(time.time() // 60) + wait) * 60
        put_schedule("werkstatt", day, [
            {"weekdays": EVERY_DAY, "from": local_minute(wait), "to": local_minute(wait + 30),
             "playlist_id": window_list},
        ])
        check("until then the default plays",
              until(lambda: item_of("werkstatt") == day_item), (item_of("werkstatt"), day_item))
        check("the window is not live early", item_of("werkstatt") == day_item,
              item_of("werkstatt"))
        # The day item is 600 s long, so only the boundary timer can move it.
        switched = until(lambda: item_of("werkstatt") == window_item,
                         timeout=boundary - time.time() + 20)
        late = round(time.time() - boundary, 1)
        check("at the boundary the screen switches, mid-item", switched,
              (item_of("werkstatt"), window_item))
        check("within a few seconds of the minute, not at the end of the item",
              switched and late < 10, late)
        check("and the foyer never noticed", item_of("foyer") == foyer_item, item_of("foyer"))
```

and extend `CASES` with `case_79`.

- [ ] **Step 2: Run it to see it fail**

Run: `cargo build && cd tests/cast && python3 test_display.py 79; cd ../..`
Expected: `[79]` "the workshop plays the window's playlist" fails — the loop still reads only the default.

- [ ] **Step 3: Resolve through the timetable at the top of the pass**

In `src/browser.rs`, replace the assignment read (the `let assigned = match sqlx::query_scalar::<_, Option<i64>>("SELECT default_playlist_id FROM displays WHERE name = ?") … ;` block) with:

```rust
            // What this screen plays is resolved per pass and never cached: an
            // operator editing the timetable expects the next item to follow it,
            // and a window that opened since the last pass is exactly that. A
            // failure to read it is *not* treated as "no playlist" -- a locked
            // database would then blank a screen that is playing perfectly well.
            let assigned = match crate::schedule::active_playlist(&state.pool, &display_name).await {
                Ok(active) => active.playlist_id,
                Err(e) => {
                    error!("Failed to read the timetable: {}", e);
                    sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
```

Keep the doc comment that sat above the old read only where it still applies; the `last_assigned` / `resume_after_order` block right after stays unchanged — it is what restarts a newly live playlist at its beginning.

- [ ] **Step 4: Judge the item on screen against what is live now**

Replace `is_playlist_item_active_now`:

```rust
/// Whether the item on screen is still one this display should be showing.
///
/// Judged against the playlist the timetable makes live *now*, not the one the
/// pass started with: a reassignment, a timetable edit and an item that was
/// disabled all land on the item already playing. Resolved here rather than
/// taken from the pass's snapshot for the reason that snapshot cannot be trusted
/// anywhere else: it may have changed since.
async fn is_playlist_item_active_now(state: &AppState, display_name: &str, id: i64) -> bool {
    let live = match crate::schedule::active_playlist(&state.pool, display_name).await {
        Ok(active) => active.playlist_id,
        Err(e) => {
            error!("Failed to read the timetable: {}", e);
            return false;
        }
    };
    let Some(playlist_id) = live else {
        return false;
    };
    let row: Result<(i64,), _> = sqlx::query_as(
        r#"
        SELECT COUNT(*)
        FROM playlist_items p
        WHERE p.id = ?
          AND p.playlist_id = ?
          AND p.is_enabled = 1
          AND (p.start_date IS NULL OR datetime(p.start_date) <= datetime('now'))
          AND (p.end_date IS NULL OR datetime('now') <= datetime(p.end_date))
        "#,
    )
    .bind(id)
    .bind(playlist_id)
    .fetch_one(&state.pool)
    .await;

    row.map(|(count,)| count > 0).unwrap_or(false)
}
```

- [ ] **Step 5: Wake at the next boundary**

In the per-item wait, directly inside `while !remaining.is_zero() {` and before `tokio::select! {`:

```rust
                    // Recomputed on every pass rather than once per item: an
                    // edited timetable pokes `playlist_signal`, which lands here,
                    // and a timer computed before the edit would fire at a
                    // boundary that no longer exists -- or miss one that does.
                    let boundary = match crate::schedule::load(&state.pool, &display_name).await {
                        Ok((_, windows)) => {
                            let now = crate::schedule::now();
                            crate::schedule::next_boundary(&windows, now)
                                .and_then(|at| (at - now).to_std().ok())
                        }
                        Err(e) => {
                            debug!("Failed to read the timetable for the boundary timer: {}", e);
                            None
                        }
                    };
```

and add this branch to the `tokio::select!`, after the `playlist_signal` branch:

```rust
                        _ = async {
                            match boundary {
                                Some(wait) => sleep(wait).await,
                                None => std::future::pending::<()>().await,
                            }
                        } => {
                            // A window opened or closed. Switching is immediate,
                            // like a reassignment; two adjacent windows naming the
                            // same playlist are not a switch, and the item carries
                            // on with its remaining time.
                            let live = crate::schedule::active_playlist(&state.pool, &display_name)
                                .await
                                .map(|active| active.playlist_id);
                            if matches!(live, Ok(live) if live != assigned) {
                                info!("The timetable changed what this screen plays, leaving item {} now.", item.id);
                                reload_before_next = true;
                                break;
                            }
                            remaining = intended_duration
                                .checked_sub(item_started_at.elapsed())
                                .unwrap_or(Duration::from_secs(0));
                        },
```

(`reload_before_next` makes the pass break with `resume_after_order` set; the top of the next pass then sees `assigned != last_assigned` and clears it, so the new playlist starts at its beginning — the existing rule, unchanged.)

- [ ] **Step 6: Run**

Run:

```bash
cargo build && cargo test
cd tests/cast && python3 test_display.py; cd ../..
```

Expected: `cargo test` green; `test_display.py` `ALL PASSED`, `[79b]` reporting a switch a few seconds after the minute.

Then the other suites that drive the loop, one at a time:

```bash
cd tests/cast
python3 test_castscreens.py
python3 test_webhook.py
python3 test_overlay.py
python3 test_media.py
cd ../..
```

Expected: `ALL PASSED` each.

- [ ] **Step 7: Commit**

```bash
git add src/browser.rs tests/cast/test_display.py
git commit -m "Play what the timetable says, and switch at a window boundary"
```

---

### Task 5: The timetable in the operator pages

**Files:**
- Modify: `web/displays.html` (styles, `buildCard`, save)
- Modify: `web/admin.html:355-400`
- Modify: `web/playlist.html:778-783`, `:1173`

**Interfaces:**
- Consumes: Task 3's API shapes.

- [ ] **Step 1: Read what is live in the landing page and the playlist page**

In `web/admin.html`, add near the top of the script (after the `el` helper):

```js
    // What a screen plays right now, as its timetable resolves it -- a window's
    // playlist, or the default, or nothing.
    const livePlaylist = (display) => {
      const id = display.schedule && display.schedule.now ? display.schedule.now.playlist_id : null;
      return id === undefined ? null : id;
    };
```

and replace the four uses of `display.playlist_id` in `screenLine` and `refresh` with `livePlaylist(display)` (the null checks become `livePlaylist(display) === null`; the message `'keine Playlist zugewiesen'` becomes `'keine Playlist aktiv'`).

In `web/playlist.html`, add the same helper under the name `livePlaylist` (with `screen` in place of `display`), and replace `screen.playlist_id` with `livePlaylist(screen)` at the three places in `updateScreenNote` and the one in the status refresh (`const elsewhere = …`).

- [ ] **Step 2: The Zeitplan editor**

In `web/displays.html`, add to the `<style>` block:

```css
    fieldset { border: 1px solid var(--line); border-radius: 4px; margin: .6rem 0 0; padding: .5rem .7rem; }
    legend { font-size: .85rem; color: var(--muted); }
    .window { display: flex; flex-wrap: wrap; gap: .4rem .8rem; align-items: center; padding: .3rem 0; border-top: 1px dotted var(--line); }
    .window:first-of-type { border-top: 0; }
    .days label { margin-right: .35rem; font-size: .85rem; }
    .overlap { color: var(--warn); font-size: .85rem; margin: .2rem 0; }
    .live { font-size: .85rem; margin: .4rem 0 0; }
```

Add above `buildCard`:

```js
    const WEEKDAYS = [[1, 'Mo'], [2, 'Di'], [3, 'Mi'], [4, 'Do'], [5, 'Fr'], [6, 'Sa'], [7, 'So']];

    const playlistName = (id) => {
      const list = PLAYLISTS.find((p) => p.id === id);
      return list ? list.name : `#${id}`;
    };

    const playlistSelect = (selected, withNone, onchange) => el('select', { onchange },
      withNone ? el('option', { value: '', text: '(keine)' }) : null,
      ...PLAYLISTS.map((list) => el('option', {
        value: String(list.id),
        text: `${list.name} (${list.items})`,
        selected: selected === list.id,
      })));

    // A time input cannot hold 24:00; the server reads an end of 00:00 as the
    // end of the day and answers 24:00, so the two are shown as one.
    const toInput = (hhmm) => (hhmm === '24:00' ? '00:00' : hhmm);

    // The timetable of one card: its default, its windows in priority order, and
    // what the server last said about overlaps and what is live.
    function scheduleEditor(schedule, markDirty) {
      const defaultSelect = playlistSelect(schedule.default_playlist_id, true, markDirty);
      const rows = el('div', {});
      const hints = el('div', {});

      const rowNodes = () => [...rows.children];

      function addRow(entry) {
        const days = el('span', { class: 'days' }, ...WEEKDAYS.map(([iso, label]) => el('label', {},
          el('input', { type: 'checkbox', checked: entry.weekdays.includes(iso), onchange: markDirty }),
          document.createTextNode(label))));
        const from = el('input', { type: 'time', value: toInput(entry.from), oninput: markDirty });
        const to = el('input', { type: 'time', value: toInput(entry.to), oninput: markDirty });
        const list = playlistSelect(entry.playlist_id, false, markDirty);
        const row = el('div', { class: 'window' }, days,
          el('span', { text: 'von' }), from, el('span', { text: 'bis' }), to,
          el('span', { text: '→' }), list,
          el('button', { type: 'button', text: '↑', title: 'höhere Priorität', onclick: () => {
            if (row.previousElementSibling) { rows.insertBefore(row, row.previousElementSibling); markDirty(); }
          } }),
          el('button', { type: 'button', text: '↓', title: 'niedrigere Priorität', onclick: () => {
            if (row.nextElementSibling) { rows.insertBefore(row.nextElementSibling, row); markDirty(); }
          } }),
          el('button', { type: 'button', text: 'Entfernen', onclick: () => { row.remove(); markDirty(); } }));
        row._read = () => ({
          weekdays: [...days.querySelectorAll('input')]
            .map((box, index) => (box.checked ? index + 1 : null)).filter(Boolean),
          from: from.value,
          to: to.value,
          playlist_id: Number(list.value),
        });
        rows.append(row);
      }

      function showState(state) {
        hints.replaceChildren(
          ...(state.overlaps || []).map(([first, second]) => el('p', {
            class: 'overlap',
            text: `Zeile ${first + 1} verdeckt Zeile ${second + 1}, wo sie sich überschneiden.`,
          })),
          el('p', {
            class: 'live',
            text: state.now && state.now.playlist_id !== null
              ? `Jetzt aktiv: ${playlistName(state.now.playlist_id)} `
                + (state.now.window === null ? '(Standard)' : `(Zeitfenster ${state.now.window + 1})`)
              : 'Jetzt aktiv: nichts – der Schirm zeigt die Leerlauf-Seite.',
          }));
      }

      for (const entry of schedule.windows || []) addRow(entry);
      showState(schedule);

      const root = el('fieldset', {},
        el('legend', { text: 'Zeitplan' }),
        field('Standard-Playlist (wenn kein Zeitfenster passt)', defaultSelect),
        rows,
        el('button', { type: 'button', text: '+ Zeitfenster', onclick: () => {
          if (!PLAYLISTS.length) return;
          addRow({ weekdays: [1, 2, 3, 4, 5], from: '08:00', to: '18:00', playlist_id: PLAYLISTS[0].id });
          markDirty();
        } }),
        el('p', { class: 'note', text: 'Das oberste passende Zeitfenster gewinnt. Bis 00:00 heißt: bis Tagesende.' }),
        hints);

      return {
        root,
        showState,
        read: () => ({
          default_playlist_id: defaultSelect.value === '' ? null : Number(defaultSelect.value),
          windows: rowNodes().map((row) => row._read()),
        }),
      };
    }
```

In `buildCard`, delete the `playlist` select, create the editor instead:

```js
      const schedule = scheduleEditor(display.schedule || { windows: [] }, markDirty);
```

replace the `save` function's request with two — the label, then the timetable — and show the server's answer:

```js
      const save = async () => {
        flash(feedback, 'Speichern…', false);
        saving.add(display.name);
        const path = `/api/displays/${encodeURIComponent(display.name)}`;
        const put = (url, body) => fetch(url, {
          method: 'PUT',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify(body),
        });
        try {
          const labelRes = await put(path, { label: label.value });
          if (!labelRes.ok) {
            flash(feedback, await readError(labelRes, 'Fehler beim Speichern.'), true);
            return;
          }
          const scheduleRes = await put(`${path}/schedule`, schedule.read());
          if (!scheduleRes.ok) {
            flash(feedback, await readError(scheduleRes, 'Fehler beim Speichern des Zeitplans.'), true);
            return;
          }
          schedule.showState(await scheduleRes.json());
        } catch (_) {
          flash(feedback, 'Server nicht erreichbar.', true);
          return;
        } finally {
          saving.delete(display.name);
        }
        dirty.delete(display.name);
        card.classList.remove('dirty');
        flash(feedback, 'Gespeichert.', false);
      };
```

and in `card.append(...)`, replace `field('Playlist', playlist),` with nothing in the grid and append `schedule.root` after the grid (before the "nicht deklariert" note). Update the page intro paragraph to: „Was auf welchem Schirm läuft: eine Standard-Playlist und Zeitfenster nach Wochentag und Uhrzeit. …“ (keep the rest of the sentence).

- [ ] **Step 3: Rebuild and check it in a browser**

Run: `cargo build`, start a dev instance with two declared displays on dead CDP ports so no Chrome is launched:

```bash
./target/debug/miniclientcontrol --managed-cert off --no-launch-browser --display foyer:9,werkstatt:10 \
  --database-path /tmp/dayparting-ui.db --port 3099 --cast-tls-port 3599
```

Open `http://127.0.0.1:3099/displays.html` and create two playlists first on `/playlist.html`. Verify:
- each card shows *Zeitplan* with the Standard-Playlist select;
- *+ Zeitfenster* adds Mo–Fr 08:00–18:00; ↑/↓ reorder; *Entfernen* removes; every change marks the card dirty and survives the 2-second poll;
- two overlapping windows with different playlists show „Zeile 1 verdeckt Zeile 2…“ after *Speichern*;
- a window covering now shows „Jetzt aktiv: … (Zeitfenster N)“;
- an end of `00:00` saves and comes back as `00:00` (stored 24:00);
- `/admin.html` shows the live playlist per screen.

Stop the instance and delete `/tmp/dayparting-ui.db*` afterwards.

- [ ] **Step 4: Commit**

```bash
git add web/displays.html web/admin.html web/playlist.html
git commit -m "Edit a screen's timetable on the displays page"
```

---

### Task 6: Documentation

**Files:**
- Modify: `CLAUDE.md` (*Displays* section)
- Modify: `README.md` (*Displays* API section)
- Modify: `docs/features.md` (*Several screens*)
- Modify: `docs/roadmap.md` (remove the shipped entry)
- Modify: `tests/cast/README.md` (`test_display.py` case range)
- Modify: `docs/superpowers/specs/2026-09-23-dayparting-design.md` (status + two amendments)

- [ ] **Step 1: CLAUDE.md**

In *Displays*, update every mention of `displays.playlist_id` to `displays.default_playlist_id`, and add after the paragraph about the per-pass re-read:

```markdown
**A display plays what its timetable resolves, and `schedule::active_playlist`
is the only resolver.** The timetable is the default playlist plus
`schedule_windows` in priority order; the first window matching the device's
local time wins. Both places that decide what plays — the top of the inner pass
and `is_playlist_item_active_now` — go through it; a third reader of
`default_playlist_id` is how a screen starts ignoring its windows. The per-item
`select!` wakes at `schedule::next_boundary`, recomputed on every pass of its
`while` loop so an edited timetable (whose `PUT` pokes `playlist_signal`) never
leaves a stale timer. A boundary switches immediately, like a reassignment.
Windows name the day they *start*; an end at or before the start crosses
midnight; an end of `00:00` is stored as 1440.

**`PUT /api/displays/{name}/schedule` replaces the whole timetable in one
transaction**, checking each playlist in the statement that writes it.
`PUT /api/displays/{name}` takes only `label`, with `deny_unknown_fields`, so a
script still sending the removed `playlist_id` gets a `422` rather than a silent
`200`.
```

In the *Database* section's list of what arrived through migrations, add: "`displays.playlist_id` was renamed `default_playlist_id` in place (`ALTER TABLE … RENAME COLUMN`), and `schedule_windows` was added."

- [ ] **Step 2: README, features, roadmap, tests README**

`README.md`, *Displays* API list — replace the `PUT /api/displays/{name}` bullet and add the schedule routes:

```markdown
- `PUT /api/displays/{name}` — set `label`; `null` clears it. Anything else, the
  former `playlist_id` included, is refused with `422`
- `GET /api/displays/{name}/schedule` — the screen's timetable:
  `{ default_playlist_id, windows: [{ weekdays, from, to, playlist_id }], overlaps, now }`.
  Weekdays are ISO numbers (Monday = 1), windows are in priority order, `to` may be
  `24:00`, `overlaps` lists index pairs where an earlier window hides a later one,
  `now` says which playlist is live and from which window
- `PUT /api/displays/{name}/schedule` — replace the whole timetable; both fields
  required, `default_playlist_id: null` means none. A bad row is a `400` naming it,
  and nothing is written
```

and in the `GET /api/displays` bullet, replace "plus any row …" wording so it says each entry carries `schedule` (the body above) instead of `playlist_id`.

`docs/features.md`, at the end of *Several screens*:

```markdown
### Dayparting

What a screen plays can follow the clock. Each screen has a **default
playlist** and any number of **time windows** — weekdays, from, to, playlist —
in priority order: the topmost window that matches now wins, and the default
covers the rest. "Mo–Fr 08:00–18:00 Büro, sonst Nacht" is one window and a
default. A window may cross midnight (Fr 22:00–06:00 runs into Saturday
morning), and a window ending at 00:00 runs to the end of the day.

At a window boundary the screen switches straight away, mid-item, exactly as it
does when an operator reassigns it by hand. Overlapping windows are allowed —
"lunch beats the working day" is a reasonable thing to want — and the displays
page says which row hides which.
```

`docs/roadmap.md` — delete the whole *Dayparting* entry (it ships); if *Next* then starts with *Video length from the file*, nothing else changes.

`tests/cast/README.md` — change "`test_display.py` (cases `[70]`-`[77]`)" to "(cases `[70]`-`[79]`)" and add a sentence: "`[79b]` waits for the next full minute on purpose — it is the boundary timer under test — so it takes up to a minute."

- [ ] **Step 3: Spec**

In `docs/superpowers/specs/2026-09-23-dayparting-design.md`: `**Status:** designed` → `**Status:** implemented`, and append:

```markdown
## Amendments made while planning

- The storage reads (`load`, `active_playlist`) live in `src/schedule/mod.rs`
  beside the rules rather than in `display.rs`, and the handlers in
  `src/schedule/api.rs` — the pattern `cast/api.rs` and `webhook/api.rs` set.
  `display::known_display` is the shared "may this name be read or written"
  check.
- A request body that does not deserialise — the removed `playlist_id` on
  `PUT /api/displays/{name}` included — is axum's `422`, not a `400`; the `400`
  with a German sentence is for a body that parses and says something wrong.
- An end of `00:00` is stored as 1440, the end of the day: a time input cannot
  show `24:00`, and "22:00–00:00" means until midnight. `00:00–00:00` is
  therefore a whole day rather than a refused `start == end`.
```

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md README.md docs/features.md docs/roadmap.md tests/cast/README.md docs/superpowers/specs/2026-09-23-dayparting-design.md
git commit -m "Document dayparting"
```
