//! Arithmetic over recorded quota history: how fast a window is being spent,
//! when it would run out at that pace, and a sparkline of its recent shape.
//!
//! Pure functions over `(fetched_at_unix, used_percent)` samples so every
//! rule here is testable without a cache directory.

use crate::cache::HistorySample;

/// Samples older than this do not describe the current pace.
const PACE_LOOKBACK_SECONDS: u64 = 6 * 60 * 60;
/// Two samples closer than this say nothing reliable about a rate.
const PACE_MIN_SPAN_SECONDS: u64 = 10 * 60;
/// A drop in used percent at least this large is a window reset; samples
/// before it belong to the previous period.
const RESET_DROP_PERCENT: f64 = 5.0;

/// How the tightest window is being spent right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pace {
    /// Percentage points consumed per hour over the recent samples.
    pub percent_per_hour: f64,
    /// Seconds until the window is empty at that rate. `None` when nothing
    /// is being consumed.
    pub empties_in_seconds: Option<u64>,
}

/// The pace over the last few hours of one window, or `None` when the
/// history is too thin to say.
///
/// Only samples since the last reset count: after a window rolls over, the
/// previous period's slope would describe spending that no longer exists.
pub fn pace(samples: &[HistorySample], now_unix: u64, remaining_percent: f64) -> Option<Pace> {
    let since = now_unix.saturating_sub(PACE_LOOKBACK_SECONDS);
    let recent: Vec<HistorySample> = samples
        .iter()
        .copied()
        .filter(|(at, _)| *at >= since && *at <= now_unix)
        .collect();
    let start = recent
        .windows(2)
        .rposition(|pair| pair[0].1 - pair[1].1 >= RESET_DROP_PERCENT)
        .map(|index| index + 1)
        .unwrap_or(0);
    let recent = &recent[start..];
    let (first, last) = (recent.first()?, recent.last()?);
    let span = last.0.saturating_sub(first.0);
    if span < PACE_MIN_SPAN_SECONDS {
        return None;
    }
    let consumed = (last.1 - first.1).max(0.0);
    let percent_per_hour = consumed / (span as f64 / 3600.0);
    let empties_in_seconds = (percent_per_hour > 0.0)
        .then(|| (remaining_percent.max(0.0) / percent_per_hour * 3600.0).round() as u64);
    Some(Pace {
        percent_per_hour,
        empties_in_seconds,
    })
}

/// The eight block heights a terminal cell can take, lowest first.
const SPARK_LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// One column of a sparkline: the remaining percent it shows and its glyph.
/// Columns with no sample in their span are `None`, so a gap in collection
/// shows as a gap rather than as a flat line.
pub type SparkColumn = Option<(f64, char)>;

/// A column with no sample repeats the previous one for up to this long:
/// a quota that nobody read for an hour was still that quota. Longer
/// silences stay visible as gaps.
const SPARK_CARRY_SECONDS: u64 = 2 * 60 * 60;

/// `columns` cells covering the last `span_seconds`, each showing the last
/// sample that fell inside it as remaining percent.
pub fn sparkline(
    samples: &[HistorySample],
    now_unix: u64,
    span_seconds: u64,
    columns: usize,
) -> Vec<SparkColumn> {
    let columns = columns.max(1);
    let start = now_unix.saturating_sub(span_seconds);
    let bucket = (span_seconds as f64 / columns as f64).max(1.0);
    let mut cells: Vec<SparkColumn> = vec![None; columns];
    let mut last_at: Vec<Option<u64>> = vec![None; columns];
    for (at, used) in samples {
        if *at < start || *at > now_unix {
            continue;
        }
        let index = (((*at - start) as f64) / bucket) as usize;
        let index = index.min(columns - 1);
        let remaining = (100.0 - used).clamp(0.0, 100.0);
        let level = ((remaining / 100.0) * (SPARK_LEVELS.len() - 1) as f64).round() as usize;
        cells[index] = Some((remaining, SPARK_LEVELS[level.min(SPARK_LEVELS.len() - 1)]));
        last_at[index] = Some(*at);
    }
    let mut carried: Option<(u64, (f64, char))> = None;
    for (index, cell) in cells.iter_mut().enumerate() {
        match (*cell, last_at[index]) {
            (Some(value), Some(at)) => carried = Some((at, value)),
            _ => {
                let column_start = start + (index as f64 * bucket) as u64;
                if let Some((at, value)) = carried {
                    if column_start.saturating_sub(at) <= SPARK_CARRY_SECONDS {
                        *cell = Some(value);
                    }
                }
            }
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pace_needs_a_ten_minute_span_and_reports_time_to_empty() {
        assert_eq!(pace(&[], 10_000, 50.0), None);
        assert_eq!(pace(&[(9_900, 10.0), (10_000, 12.0)], 10_000, 88.0), None);
        // 2 points per 20 minutes = 6 points per hour; 88 left lasts ~14.7h.
        let found = pace(&[(8_800, 10.0), (10_000, 12.0)], 10_000, 88.0).unwrap();
        assert!((found.percent_per_hour - 6.0).abs() < 1e-9);
        assert_eq!(found.empties_in_seconds, Some(52_800));
    }

    #[test]
    fn a_flat_window_has_a_pace_of_zero_and_never_empties() {
        let found = pace(&[(0, 40.0), (3_600, 40.0)], 3_600, 60.0).unwrap();
        assert_eq!(found.percent_per_hour, 0.0);
        assert_eq!(found.empties_in_seconds, None);
    }

    #[test]
    fn samples_before_a_reset_do_not_shape_the_current_pace() {
        // 90% used, then the window reset to 2% and climbed slowly.
        let samples = [(0, 90.0), (1_800, 2.0), (3_600, 3.0), (5_400, 4.0)];
        let found = pace(&samples, 5_400, 96.0).unwrap();
        assert!((found.percent_per_hour - 2.0).abs() < 1e-9, "{found:?}");
    }

    #[test]
    fn samples_older_than_the_lookback_are_ignored() {
        let now = 100_000;
        let samples = [(now - 10 * 3_600, 0.0), (now - 600, 50.0), (now, 51.0)];
        let found = pace(&samples, now, 49.0).unwrap();
        assert!((found.percent_per_hour - 6.0).abs() < 1e-9, "{found:?}");
    }

    #[test]
    fn sparkline_buckets_the_span_and_carries_short_gaps_forward() {
        let cells = sparkline(&[(0, 0.0), (500, 50.0), (999, 100.0)], 1_000, 1_000, 4);
        assert_eq!(cells.len(), 4);
        assert_eq!(cells[0], Some((100.0, '█')));
        // A 250-second silence still shows the last known value.
        assert_eq!(cells[1], Some((100.0, '█')));
        assert_eq!(cells[2], Some((50.0, '▅')));
        assert_eq!(cells[3], Some((0.0, '▁')));
    }

    #[test]
    fn sparkline_leaves_long_silences_as_gaps() {
        let day = 86_400;
        let cells = sparkline(&[(0, 20.0), (day - 1, 40.0)], day, day, 8);
        assert_eq!(cells[0], Some((80.0, '▇')));
        assert_eq!(cells[1], None, "{cells:?}");
        assert_eq!(cells[6], None, "{cells:?}");
        assert_eq!(cells[7], Some((60.0, '▅')));
    }
}
