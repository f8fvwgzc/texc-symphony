//! Token throughput (E.5.3) and the 10-minute sparkline (E.5.4; implemented and tested like in
//! Elixir, not shown in the current frame).
//!
//! Samples are `(monotonic_ms, total_tokens)` pairs, newest first.

/// Rolling throughput window.
pub const THROUGHPUT_WINDOW_MS: i64 = 5_000;
/// Sparkline window.
pub const GRAPH_WINDOW_MS: i64 = 10 * 60 * 1000;
/// Sparkline columns.
pub const GRAPH_COLUMNS: i64 = 24;
const SPARKLINE: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// A token sample: `(timestamp_ms, total_tokens)`.
pub type Sample = (i64, i64);

/// Samples within the 5 s throughput window.
pub fn prune_samples(samples: &[Sample], now_ms: i64) -> Vec<Sample> {
    let min = now_ms - THROUGHPUT_WINDOW_MS;
    samples
        .iter()
        .copied()
        .filter(|(ts, _)| *ts >= min)
        .collect()
}

/// Samples within the 10 min graph window.
pub fn prune_graph_samples(samples: &[Sample], now_ms: i64) -> Vec<Sample> {
    let min = now_ms - THROUGHPUT_WINDOW_MS.max(GRAPH_WINDOW_MS);
    samples
        .iter()
        .copied()
        .filter(|(ts, _)| *ts >= min)
        .collect()
}

/// `update_token_samples/3`: prepend `(now, total)` and keep the graph window.
pub fn update_token_samples(samples: &[Sample], now_ms: i64, total_tokens: i64) -> Vec<Sample> {
    let mut next = Vec::with_capacity(samples.len() + 1);
    next.push((now_ms, total_tokens));
    next.extend_from_slice(samples);
    prune_graph_samples(&next, now_ms)
}

/// Tokens per second over the last 5 s, measured from the oldest sample in the window.
pub fn rolling_tps(samples: &[Sample], now_ms: i64, current_tokens: i64) -> f64 {
    let mut all = Vec::with_capacity(samples.len() + 1);
    all.push((now_ms, current_tokens));
    all.extend_from_slice(samples);
    let window = prune_samples(&all, now_ms);
    if window.len() <= 1 {
        return 0.0;
    }
    let Some(&(start_ms, start_tokens)) = window.last() else {
        return 0.0;
    };
    let elapsed_ms = now_ms - start_ms;
    let delta = (current_tokens - start_tokens).max(0);
    if elapsed_ms <= 0 {
        0.0
    } else {
        delta as f64 / (elapsed_ms as f64 / 1000.0)
    }
}

/// Recomputes [`rolling_tps`] at most once per wall second (`throttled_tps/5`).
pub fn throttled_tps(
    last_second: Option<i64>,
    last_value: Option<f64>,
    now_ms: i64,
    samples: &[Sample],
    current_tokens: i64,
) -> (i64, f64) {
    let second = now_ms.div_euclid(1000);
    match (last_second, last_value) {
        (Some(last), Some(value)) if last == second => (second, value),
        _ => (second, rolling_tps(samples, now_ms, current_tokens)),
    }
}

/// 24-column sparkline of the last 10 minutes (`tps_graph/3`).
pub fn tps_graph(samples: &[Sample], now_ms: i64, current_tokens: i64) -> String {
    let bucket_ms = GRAPH_WINDOW_MS / GRAPH_COLUMNS;
    let active_start = now_ms.div_euclid(bucket_ms) * bucket_ms;
    let window_start = active_start - (GRAPH_COLUMNS - 1) * bucket_ms;

    let mut all = Vec::with_capacity(samples.len() + 1);
    all.push((now_ms, current_tokens));
    all.extend_from_slice(samples);
    let mut points = prune_graph_samples(&all, now_ms);
    points.sort_by_key(|(ts, _)| *ts);
    let rates: Vec<(i64, f64)> = points
        .windows(2)
        .map(|pair| {
            let ((start_ms, start_tokens), (end_ms, end_tokens)) = (pair[0], pair[1]);
            let elapsed = end_ms - start_ms;
            let delta = (end_tokens - start_tokens).max(0);
            let tps = if elapsed <= 0 {
                0.0
            } else {
                delta as f64 / (elapsed as f64 / 1000.0)
            };
            (end_ms, tps)
        })
        .collect();

    let buckets: Vec<f64> = (0..GRAPH_COLUMNS)
        .map(|idx| {
            let start = window_start + idx * bucket_ms;
            let end = start + bucket_ms;
            let last = idx == GRAPH_COLUMNS - 1;
            let values: Vec<f64> = rates
                .iter()
                .filter(|(ts, _)| *ts >= start && if last { *ts <= end } else { *ts < end })
                .map(|(_, tps)| *tps)
                .collect();
            if values.is_empty() {
                0.0
            } else {
                values.iter().sum::<f64>() / values.len() as f64
            }
        })
        .collect();
    let max = buckets.iter().copied().fold(0.0_f64, f64::max);
    buckets
        .iter()
        .map(|value| {
            let index = if max <= 0.0 {
                0
            } else {
                // In 0..=7 by construction (value <= max).
                (value / max * 7.0).round() as usize
            };
            SPARKLINE.get(index).copied().unwrap_or(SPARKLINE[0])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_five_second_throughput() {
        assert_eq!(rolling_tps(&[], 10_000, 0), 0.0);
        assert_eq!(rolling_tps(&[(9_000, 20)], 10_000, 40), 20.0);
        assert_eq!(rolling_tps(&[(4_900, 10)], 10_000, 90), 0.0);
        assert_eq!(
            rolling_tps(&[(9_500, 10), (9_000, 40), (8_000, 80)], 10_000, 95),
            7.5
        );
    }

    #[test]
    fn throttles_tps_updates_to_once_per_second() {
        let (first_second, first_tps) = throttled_tps(None, None, 10_000, &[(9_000, 20)], 40);
        let (same_second, same_tps) = throttled_tps(
            Some(first_second),
            Some(first_tps),
            10_500,
            &[(9_000, 20)],
            200,
        );
        assert_eq!(same_second, first_second);
        assert_eq!(same_tps, first_tps);
        let (next_second, next_tps) = throttled_tps(
            Some(same_second),
            Some(same_tps),
            11_000,
            &[(10_500, 200)],
            260,
        );
        assert_eq!(next_second, 11);
        assert_ne!(next_tps, same_tps);
    }

    #[test]
    fn graph_for_steady_throughput() {
        let samples: Vec<Sample> = (0..=23)
            .rev()
            .map(|i| {
                let ts = i * 25_000;
                (ts, ts / 100)
            })
            .collect();
        assert_eq!(
            tps_graph(&samples, 600_000, 6_000),
            "████████████████████████"
        );
    }

    fn graph_samples_from_rates(rates: &[i64]) -> (i64, Vec<Sample>) {
        let bucket_ms = 25_000;
        let (mut ts, mut tokens, mut samples) = (0_i64, 0_i64, Vec::new());
        for rate in rates {
            samples.insert(0, (ts, tokens));
            ts += bucket_ms;
            tokens += rate * bucket_ms / 1000;
        }
        samples.insert(0, (ts, tokens));
        (tokens, samples)
    }

    #[test]
    fn graph_for_ramping_throughput() {
        let rates: Vec<i64> = (1..=24).map(|n| n * 2).collect();
        let (current, samples) = graph_samples_from_rates(&rates);
        assert_eq!(
            tps_graph(&samples, 600_000, current),
            "▁▂▂▂▃▃▃▃▄▄▄▅▅▅▆▆▆▆▇▇▇██▅"
        );
    }

    #[test]
    fn historical_bars_are_stable_within_the_active_bucket() {
        let now_ms = 600_000;
        let rate_for = |ts: i64| {
            (1..=24)
                .map(|n| n * 5)
                .nth(((ts.max(0) / 25_000).min(23)) as usize)
                .unwrap_or(0)
        };
        let mut tokens = 0;
        let mut samples = Vec::new();
        let mut ts = 0;
        while ts <= now_ms - 1_000 {
            tokens += rate_for(ts);
            samples.insert(0, (ts, tokens));
            ts += 1_000;
        }
        let at_now = tps_graph(&samples, now_ms, 74_400);
        let next = tps_graph(&samples, now_ms + 1_000, 74_520);
        let changes = at_now
            .chars()
            .zip(next.chars())
            .take(23)
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(changes, 0);
    }

    #[test]
    fn update_keeps_the_graph_window() {
        let samples = update_token_samples(&[(0, 1), (500_000, 2)], 700_000, 3);
        assert_eq!(samples, vec![(700_000, 3), (500_000, 2)]);
    }
}
