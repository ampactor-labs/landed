//! Percentiles over stage timings. Nearest-rank, no interpolation, no
//! dependencies; this is a reporting aid, not a statistics library.

use std::time::Duration;

use crate::track::Flight;

/// Nearest-rank percentiles over a set of duration samples.
#[derive(Debug, Clone, Copy)]
pub struct Percentiles {
    /// Sample count.
    pub n: usize,
    /// 50th percentile.
    pub p50: Duration,
    /// 90th percentile.
    pub p90: Duration,
    /// 99th percentile.
    pub p99: Duration,
    /// Largest sample.
    pub max: Duration,
}

impl Percentiles {
    /// Compute from unsorted samples. Returns `None` on an empty set —
    /// there is no honest percentile of nothing.
    pub fn from_samples(samples: &[Duration]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let rank = |p: f64| -> Duration {
            let idx = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
            sorted[idx.clamp(1, sorted.len()) - 1]
        };
        Some(Self {
            n: sorted.len(),
            p50: rank(50.0),
            p90: rank(90.0),
            p99: rank(99.0),
            max: *sorted.last().unwrap(),
        })
    }
}

fn ms(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1e3)
}

/// A named stage and the accessor that pulls its duration off a flight.
type Stage = (&'static str, fn(&Flight) -> Duration);

/// Render a per-stage percentile table (milliseconds) over a set of flights.
pub fn stage_report(flights: &[Flight]) -> String {
    let stages: [Stage; 5] = [
        ("assemble", |f| f.timing.assemble),
        ("gate", |f| f.timing.gate),
        ("submit", |f| f.timing.submit),
        ("confirm", |f| f.timing.confirm),
        ("total", |f| f.timing.total()),
    ];
    let mut out = String::from("stage      p50 ms   p90 ms   p99 ms   max ms\n");
    for (name, pick) in stages {
        let samples: Vec<Duration> = flights.iter().map(pick).collect();
        match Percentiles::from_samples(&samples) {
            Some(p) => {
                out.push_str(&format!(
                    "{name:<9} {:>7} {:>8} {:>8} {:>8}\n",
                    ms(p.p50),
                    ms(p.p90),
                    ms(p.p99),
                    ms(p.max)
                ));
            }
            None => out.push_str(&format!("{name:<9} (no samples)\n")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_has_no_percentiles() {
        assert!(Percentiles::from_samples(&[]).is_none());
    }

    #[test]
    fn single_sample_is_every_percentile() {
        let p = Percentiles::from_samples(&[Duration::from_millis(7)]).unwrap();
        assert_eq!(p.n, 1);
        assert_eq!(p.p50, Duration::from_millis(7));
        assert_eq!(p.p99, Duration::from_millis(7));
        assert_eq!(p.max, Duration::from_millis(7));
    }

    #[test]
    fn nearest_rank_on_hundred_samples() {
        let samples: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        let p = Percentiles::from_samples(&samples).unwrap();
        assert_eq!(p.p50, Duration::from_millis(50));
        assert_eq!(p.p90, Duration::from_millis(90));
        assert_eq!(p.p99, Duration::from_millis(99));
        assert_eq!(p.max, Duration::from_millis(100));
    }

    #[test]
    fn order_does_not_matter() {
        let a = Percentiles::from_samples(&[
            Duration::from_millis(3),
            Duration::from_millis(1),
            Duration::from_millis(2),
        ])
        .unwrap();
        assert_eq!(a.p50, Duration::from_millis(2));
        assert_eq!(a.max, Duration::from_millis(3));
    }
}
