//! Calendar days and the activity bars. No clock here: `now` and the zone
//! offset are passed in.

const DAY: i64 = 86_400;
const BARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// How many calendar days before `now` the time `then` was, in the zone
/// `offset` seconds east of UTC. 0 for today and for a time in the future.
pub fn days_ago(now: i64, then: i64, offset: i64) -> i64 {
    let day = |t: i64| (t + offset).div_euclid(DAY);
    (day(now) - day(then)).max(0)
}

/// The section of the projects list for a row last touched `then`: "today",
/// "this week" (the six days before), "this month" (up to 30 days), "older".
/// None when it never was.
pub fn section(now: i64, then: Option<i64>, offset: i64) -> Option<&'static str> {
    Some(match days_ago(now, then?, offset) {
        0 => "today",
        1..7 => "this week",
        7..30 => "this month",
        _ => "older",
    })
}

/// Sessions per day over the last `days` days, oldest first and today last.
/// Each item is a session's first and last time: it counts on the day it
/// started and on the day it was last used.
pub fn per_day(
    spans: impl Iterator<Item = (Option<i64>, Option<i64>)>,
    now: i64,
    offset: i64,
    days: usize,
) -> Vec<u32> {
    let mut counts = vec![0; days];
    let slot = |t: Option<i64>| {
        let back = usize::try_from(days_ago(now, t?, offset)).ok()?;
        (back < days).then(|| days - 1 - back)
    };
    for (first, last) in spans {
        let (first, last) = (slot(first), slot(last));
        for at in [first, last.filter(|&l| Some(l) != first)]
            .into_iter()
            .flatten()
        {
            counts[at] += 1;
        }
    }
    counts
}

/// One bar per count on a fixed scale, so one row compares with another:
/// blank for 0, then 1, 2, 3, up to 5, up to 8, up to 12, up to 20, more.
pub fn bars(counts: &[u32]) -> String {
    counts
        .iter()
        .map(|&c| {
            BARS[match c {
                0..4 => c as usize,
                4..6 => 4,
                6..9 => 5,
                9..13 => 6,
                13..21 => 7,
                _ => 8,
            }]
        })
        .collect()
}

/// One bar per count, the largest count drawn as the full bar.
pub fn bars_scaled(counts: &[u32]) -> String {
    let max = counts.iter().copied().max().unwrap_or(0).max(1) as u64;
    counts
        .iter()
        .map(|&c| BARS[(c as u64 * 8).div_ceil(max) as usize])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2001-09-09 01:46:40 UTC.
    const NOW: i64 = 1_000_000_000;
    const HOUR: i64 = 3600;

    #[test]
    fn days_ago_counts_midnights_in_the_given_zone() {
        assert_eq!(days_ago(NOW, NOW - 60, 0), 0);
        assert_eq!(days_ago(NOW, NOW - 2 * HOUR, 0), 1, "23:46 the day before");
        assert_eq!(days_ago(NOW, NOW - 26 * HOUR, 0), 2);
        // Six hours west it is still 19:46 on the 8th.
        assert_eq!(days_ago(NOW, NOW - 2 * HOUR, -6 * HOUR), 0);
        assert_eq!(days_ago(NOW, NOW - 20 * HOUR, -6 * HOUR), 1);
        assert_eq!(days_ago(NOW, NOW + 3 * DAY, 0), 0, "the future is today");
    }

    #[test]
    fn sections_split_at_one_seven_and_thirty_days() {
        let at = |days: i64| section(NOW, Some(NOW - days * DAY), 0);
        assert_eq!(at(0), Some("today"));
        assert_eq!(at(1), Some("this week"));
        assert_eq!(at(6), Some("this week"));
        assert_eq!(at(7), Some("this month"));
        assert_eq!(at(29), Some("this month"));
        assert_eq!(at(30), Some("older"));
        assert_eq!(section(NOW, None, 0), None);
    }

    #[test]
    fn per_day_counts_a_session_on_its_first_and_last_day() {
        let spans = [
            // Started three days ago, used again today.
            (Some(NOW - 3 * DAY), Some(NOW - 60)),
            // Started and ended yesterday: once, not twice.
            (Some(NOW - DAY), Some(NOW - DAY + 60)),
            // Only a last time.
            (None, Some(NOW - DAY)),
            // Before the window.
            (Some(NOW - 9 * DAY), Some(NOW - 9 * DAY)),
            (None, None),
        ];
        assert_eq!(per_day(spans.into_iter(), NOW, 0, 5), [0, 1, 0, 2, 1]);
        assert_eq!(per_day(std::iter::empty(), NOW, 0, 3), [0, 0, 0]);
    }

    #[test]
    fn bars_use_one_fixed_scale() {
        let counts = [0, 1, 2, 3, 4, 5, 6, 8, 9, 12, 13, 20, 21, 100];
        assert_eq!(bars(&counts), " ▁▂▃▄▄▅▅▆▆▇▇██");
    }

    #[test]
    fn scaled_bars_fill_up_to_the_busiest_day() {
        assert_eq!(bars_scaled(&[0, 1, 4, 8]), " ▁▄█");
        assert_eq!(bars_scaled(&[10, 5, 1]), "█▄▁");
        assert_eq!(bars_scaled(&[3]), "█");
        assert_eq!(bars_scaled(&[0, 0]), "  ");
    }
}
