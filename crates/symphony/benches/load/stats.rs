//! Latency summaries and small numeric helpers.

use std::time::Duration;

use serde_json::{Value, json};

/// Order statistics of a set of durations, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Summary {
    pub n: usize,
    pub min: f64,
    pub mean: f64,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
}

/// Nearest-rank percentile of a sorted slice (`q` in 0..=1); 0 for an empty slice.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

impl Summary {
    /// Summary of `values` (milliseconds).
    pub fn of_ms(values: &[f64]) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        Self {
            n: sorted.len(),
            min: sorted[0],
            mean: sorted.iter().sum::<f64>() / sorted.len() as f64,
            p50: percentile(&sorted, 0.50),
            p90: percentile(&sorted, 0.90),
            p99: percentile(&sorted, 0.99),
            max: sorted[sorted.len() - 1],
        }
    }

    /// Summary of `durations`.
    pub fn of(durations: &[Duration]) -> Self {
        let values: Vec<f64> = durations.iter().map(|d| ms(*d)).collect();
        Self::of_ms(&values)
    }

    /// JSON form (milliseconds, three decimals).
    pub fn json(&self) -> Value {
        json!({
            "n": self.n,
            "min_ms": round3(self.min),
            "mean_ms": round3(self.mean),
            "p50_ms": round3(self.p50),
            "p90_ms": round3(self.p90),
            "p99_ms": round3(self.p99),
            "max_ms": round3(self.max),
        })
    }

    /// `p50 / p99 / max` for tables.
    pub fn brief(&self) -> String {
        if self.n == 0 {
            return "-".to_owned();
        }
        format!(
            "{} / {} / {}",
            fmt_ms(self.p50),
            fmt_ms(self.p99),
            fmt_ms(self.max)
        )
    }
}

/// A duration in milliseconds.
pub fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

/// Rounds to three decimals (keeps result files readable).
pub fn round3(value: f64) -> f64 {
    (value * 1_000.0).round() / 1_000.0
}

/// Milliseconds with a precision that fits the magnitude.
pub fn fmt_ms(value: f64) -> String {
    if value >= 100.0 {
        format!("{value:.0}")
    } else if value >= 10.0 {
        format!("{value:.1}")
    } else if value >= 1.0 {
        format!("{value:.2}")
    } else {
        format!("{value:.3}")
    }
}

/// Bytes as mebibytes.
pub fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Least-squares slope of `points` (`x`, `y`); 0 with fewer than two points.
pub fn slope(points: &[(f64, f64)]) -> f64 {
    if points.len() < 2 {
        return 0.0;
    }
    let n = points.len() as f64;
    let mean_x = points.iter().map(|p| p.0).sum::<f64>() / n;
    let mean_y = points.iter().map(|p| p.1).sum::<f64>() / n;
    let covariance: f64 = points.iter().map(|p| (p.0 - mean_x) * (p.1 - mean_y)).sum();
    let variance: f64 = points.iter().map(|p| (p.0 - mean_x).powi(2)).sum();
    if variance == 0.0 {
        0.0
    } else {
        covariance / variance
    }
}
