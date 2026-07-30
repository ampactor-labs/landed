//! Fail-closed pre-submission gates.
//!
//! The law: a transaction is submitted only after every gate returns `Ok`.
//! Gates are pure functions over a [`GateCtx`] the pipeline assembles — all
//! I/O (balance, fee, simulation) happens before the gates run, so any I/O
//! failure rejects the transaction instead of skipping a check. Unknown is
//! not safe. No certain signal, no submission.

use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;

/// Why a gate refused a transaction.
#[derive(Debug, Clone)]
pub struct Rejection {
    /// Name of the gate that rejected.
    pub gate: &'static str,
    /// Human-readable reason, specific enough to act on.
    pub reason: String,
}

/// Result of the pipeline's one pre-submission simulation.
#[derive(Debug, Clone, Default)]
pub struct SimSummary {
    /// Error string if the simulation failed; `None` means it succeeded.
    pub err: Option<String>,
    /// Compute units the simulation consumed, when the RPC reports them.
    pub units_consumed: Option<u64>,
    /// Program logs from the simulation.
    pub logs: Vec<String>,
}

/// Everything a gate may inspect. Assembled once per transaction by the
/// pipeline, with every field populated from live RPC state.
#[derive(Debug)]
pub struct GateCtx<'a> {
    /// The fully signed transaction under consideration.
    pub tx: &'a Transaction,
    /// Fee payer.
    pub payer: Pubkey,
    /// Payer balance in lamports at assembly time.
    pub payer_balance: u64,
    /// Network fee for this message in lamports.
    pub fee: u64,
    /// Jito tip in lamports (0 on the plain RPC route).
    pub tip: u64,
    /// Simulation outcome.
    pub sim: &'a SimSummary,
}

/// A pre-submission check. Return `Err(Rejection)` to stop the pipeline.
pub trait Gate: Send + Sync {
    /// Stable name used in rejections and logs.
    fn name(&self) -> &'static str;
    /// Check the transaction; `Ok(())` approves.
    fn check(&self, ctx: &GateCtx<'_>) -> Result<(), Rejection>;
}

/// Rejects when the network fee exceeds a ceiling.
pub struct FeeCeiling(pub u64);

impl Gate for FeeCeiling {
    fn name(&self) -> &'static str {
        "fee_ceiling"
    }
    fn check(&self, ctx: &GateCtx<'_>) -> Result<(), Rejection> {
        if ctx.fee > self.0 {
            return Err(Rejection {
                gate: self.name(),
                reason: format!("fee {} lamports exceeds ceiling {}", ctx.fee, self.0),
            });
        }
        Ok(())
    }
}

/// Rejects when the Jito tip exceeds a ceiling. A tip of 0 always passes,
/// so the gate is a no-op on the plain RPC route.
pub struct TipCeiling(pub u64);

impl Gate for TipCeiling {
    fn name(&self) -> &'static str {
        "tip_ceiling"
    }
    fn check(&self, ctx: &GateCtx<'_>) -> Result<(), Rejection> {
        if ctx.tip > self.0 {
            return Err(Rejection {
                gate: self.name(),
                reason: format!("tip {} lamports exceeds ceiling {}", ctx.tip, self.0),
            });
        }
        Ok(())
    }
}

/// Rejects when paying fee + tip would drop the payer below a floor.
/// Program-level transfers inside the transaction are not modeled; set the
/// floor high enough to cover what the instructions themselves may spend.
pub struct BalanceFloor(pub u64);

impl Gate for BalanceFloor {
    fn name(&self) -> &'static str {
        "balance_floor"
    }
    fn check(&self, ctx: &GateCtx<'_>) -> Result<(), Rejection> {
        let spend = ctx.fee.saturating_add(ctx.tip);
        let after = ctx.payer_balance.saturating_sub(spend);
        if ctx.payer_balance < spend || after < self.0 {
            return Err(Rejection {
                gate: self.name(),
                reason: format!(
                    "balance {} minus fee+tip {} leaves {}, below floor {}",
                    ctx.payer_balance, spend, after, self.0
                ),
            });
        }
        Ok(())
    }
}

/// Rejects when the pre-submission simulation failed.
pub struct SimulationMustPass;

impl Gate for SimulationMustPass {
    fn name(&self) -> &'static str {
        "simulation"
    }
    fn check(&self, ctx: &GateCtx<'_>) -> Result<(), Rejection> {
        if let Some(err) = &ctx.sim.err {
            return Err(Rejection {
                gate: self.name(),
                reason: format!("simulation failed: {err}"),
            });
        }
        Ok(())
    }
}

/// Rejects when simulated compute consumption exceeds a ceiling, and — the
/// fail-closed half — when the RPC did not report consumption at all.
pub struct ComputeCeiling(pub u64);

impl Gate for ComputeCeiling {
    fn name(&self) -> &'static str {
        "compute_ceiling"
    }
    fn check(&self, ctx: &GateCtx<'_>) -> Result<(), Rejection> {
        match ctx.sim.units_consumed {
            Some(units) if units > self.0 => Err(Rejection {
                gate: self.name(),
                reason: format!("simulated {units} compute units exceeds ceiling {}", self.0),
            }),
            Some(_) => Ok(()),
            None => Err(Rejection {
                gate: self.name(),
                reason: "simulation reported no compute consumption; failing closed".into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::hash::Hash;
    use solana_sdk::message::Message;
    use solana_sdk::signature::{Keypair, Signer};
    use solana_system_interface::instruction as system_instruction;

    fn signed_tx(payer: &Keypair) -> Transaction {
        let ix = system_instruction::transfer(&payer.pubkey(), &payer.pubkey(), 1);
        let msg = Message::new(&[ix], Some(&payer.pubkey()));
        let mut tx = Transaction::new_unsigned(msg);
        tx.sign(&[payer], Hash::new_unique());
        tx
    }

    fn ctx<'a>(tx: &'a Transaction, payer: Pubkey, sim: &'a SimSummary) -> GateCtx<'a> {
        GateCtx {
            tx,
            payer,
            payer_balance: 1_000_000,
            fee: 5_000,
            tip: 0,
            sim,
        }
    }

    #[test]
    fn fee_ceiling_rejects_above_and_passes_below() {
        let kp = Keypair::new();
        let tx = signed_tx(&kp);
        let sim = SimSummary::default();
        let c = ctx(&tx, kp.pubkey(), &sim);
        assert!(FeeCeiling(4_999).check(&c).is_err());
        assert!(FeeCeiling(5_000).check(&c).is_ok());
    }

    #[test]
    fn tip_ceiling_ignores_rpc_route() {
        let kp = Keypair::new();
        let tx = signed_tx(&kp);
        let sim = SimSummary::default();
        let c = ctx(&tx, kp.pubkey(), &sim);
        // tip is 0 on the RPC route; even a zero ceiling passes.
        assert!(TipCeiling(0).check(&c).is_ok());
    }

    #[test]
    fn balance_floor_counts_fee_and_tip() {
        let kp = Keypair::new();
        let tx = signed_tx(&kp);
        let sim = SimSummary::default();
        let mut c = ctx(&tx, kp.pubkey(), &sim);
        c.tip = 10_000;
        // 1_000_000 - 15_000 = 985_000
        assert!(BalanceFloor(985_000).check(&c).is_ok());
        assert!(BalanceFloor(985_001).check(&c).is_err());
    }

    #[test]
    fn balance_floor_rejects_insolvent_payer() {
        let kp = Keypair::new();
        let tx = signed_tx(&kp);
        let sim = SimSummary::default();
        let mut c = ctx(&tx, kp.pubkey(), &sim);
        c.payer_balance = 1_000;
        assert!(BalanceFloor(0).check(&c).is_err());
    }

    #[test]
    fn simulation_gate_propagates_failure() {
        let kp = Keypair::new();
        let tx = signed_tx(&kp);
        let sim = SimSummary {
            err: Some("custom program error".into()),
            ..Default::default()
        };
        let c = ctx(&tx, kp.pubkey(), &sim);
        let err = SimulationMustPass.check(&c).unwrap_err();
        assert!(err.reason.contains("custom program error"));
    }

    #[test]
    fn compute_ceiling_fails_closed_on_missing_data() {
        let kp = Keypair::new();
        let tx = signed_tx(&kp);
        let sim = SimSummary::default(); // units_consumed: None
        let c = ctx(&tx, kp.pubkey(), &sim);
        let err = ComputeCeiling(200_000).check(&c).unwrap_err();
        assert!(err.reason.contains("failing closed"));

        let sim_ok = SimSummary {
            units_consumed: Some(150),
            ..Default::default()
        };
        let c2 = ctx(&tx, kp.pubkey(), &sim_ok);
        assert!(ComputeCeiling(200_000).check(&c2).is_ok());
        assert!(ComputeCeiling(100).check(&c2).is_err());
    }
}
