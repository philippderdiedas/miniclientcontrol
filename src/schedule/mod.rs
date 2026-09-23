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
