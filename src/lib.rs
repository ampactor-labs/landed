//! Fail-closed Solana transaction execution.
//!
//! `landed` runs one transaction at a time through four timed stages:
//!
//! ```text
//! assemble ──► gate ──► submit ──► track
//!  blockhash    fee       RPC or     signature status
//!  (+ tip)      balance   Jito       bundle status
//!  sign         simulate  bundle     blockhash expiry
//! ```
//!
//! and hands back a [`Flight`]: what happened ([`Outcome::Landed`] with the
//! slot, [`Outcome::Expired`], or [`Outcome::Failed`] with the cluster's
//! reason) and how long each stage took.
//!
//! The law is fail-closed: every gate must approve, and an I/O error while
//! gathering gate evidence rejects the transaction instead of skipping the
//! check. Unknown is not safe.
//!
//! ```no_run
//! use landed::{BalanceFloor, FeeCeiling, Pipeline, Route, SimulationMustPass};
//! use solana_sdk::signature::Keypair;
//! use solana_system_interface::instruction as system_instruction;
//! use solana_sdk::signer::Signer;
//!
//! # async fn demo() -> landed::Result<()> {
//! let payer = Keypair::new();
//! let pipeline = Pipeline::new("http://127.0.0.1:8899")
//!     .with_gate(SimulationMustPass)
//!     .with_gate(FeeCeiling(20_000))
//!     .with_gate(BalanceFloor(500_000));
//!
//! let ix = system_instruction::transfer(&payer.pubkey(), &payer.pubkey(), 1_000);
//! let flight = pipeline.run(vec![ix], &payer, Route::Rpc).await?;
//! println!("{:?} in {:?}", flight.outcome, flight.timing.total());
//! # Ok(())
//! # }
//! ```
//!
//! This is execution machinery, not a trading system: no strategy, no
//! signals, no alpha. It is the engineering extracted from a retired one.

#![warn(missing_docs)]

mod error;
pub mod gate;
pub mod metrics;
mod pipeline;
pub mod rpc;
pub mod submit;
mod track;

pub use error::{Error, Result};
pub use gate::{
    BalanceFloor, ComputeCeiling, FeeCeiling, Gate, GateCtx, Rejection, SimSummary,
    SimulationMustPass, TipCeiling,
};
pub use metrics::{stage_report, Percentiles};
pub use pipeline::{Pipeline, PipelineConfig};
pub use rpc::{Commitment, RpcClient, SignatureStatus};
pub use submit::{tip_instruction, BundleStatus, JitoClient, Route, TIP_ACCOUNTS};
pub use track::{Flight, Outcome, Timing};
