//! What happened to a submission, and how long each stage took.

use std::time::Duration;

use solana_sdk::signature::Signature;

/// Terminal state of one submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Confirmed on chain.
    Landed {
        /// Slot the transaction or bundle landed in.
        slot: u64,
    },
    /// The blockhash expired before confirmation; the transaction can no
    /// longer land and is safe to rebuild and retry.
    Expired,
    /// The cluster or block engine reported a terminal failure.
    Failed {
        /// What the cluster said.
        reason: String,
    },
}

/// Wall-clock duration of each pipeline stage.
#[derive(Debug, Clone, Copy, Default)]
pub struct Timing {
    /// Blockhash fetch + message build + signing.
    pub assemble: Duration,
    /// Gate context I/O (balance, fee, simulation) + gate checks.
    pub gate: Duration,
    /// The submit call itself.
    pub submit: Duration,
    /// Submission until terminal outcome.
    pub confirm: Duration,
}

impl Timing {
    /// Sum of all stages.
    pub fn total(&self) -> Duration {
        self.assemble + self.gate + self.submit + self.confirm
    }
}

/// One transaction's trip through the pipeline.
#[derive(Debug, Clone)]
pub struct Flight {
    /// Transaction signature (always present; bundles carry signed txs too).
    pub signature: Signature,
    /// Bundle id when the Jito route was used.
    pub bundle_id: Option<String>,
    /// Terminal outcome.
    pub outcome: Outcome,
    /// Per-stage timing.
    pub timing: Timing,
}

impl Flight {
    /// True when the outcome is [`Outcome::Landed`].
    pub fn landed(&self) -> bool {
        matches!(self.outcome, Outcome::Landed { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_total_sums_stages() {
        let t = Timing {
            assemble: Duration::from_millis(10),
            gate: Duration::from_millis(20),
            submit: Duration::from_millis(5),
            confirm: Duration::from_millis(400),
        };
        assert_eq!(t.total(), Duration::from_millis(435));
    }

    #[test]
    fn landed_predicate() {
        let f = Flight {
            signature: Signature::default(),
            bundle_id: None,
            outcome: Outcome::Landed { slot: 42 },
            timing: Timing::default(),
        };
        assert!(f.landed());
        let g = Flight {
            outcome: Outcome::Expired,
            ..f
        };
        assert!(!g.landed());
    }
}
