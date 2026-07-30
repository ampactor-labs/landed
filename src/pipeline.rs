//! The pipeline: assemble → gate → submit → track, with each stage timed.

use std::time::{Duration, Instant};

use solana_sdk::hash::Hash;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction;
use tracing::{info, warn};

use crate::error::{Error, Result};
use crate::gate::{Gate, GateCtx, Rejection};
use crate::rpc::{Commitment, RpcClient};
use crate::submit::{tip_instruction, JitoClient, Route};
use crate::track::{Flight, Outcome, Timing};

/// Knobs for polling and confirmation.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Commitment used for blockhash, simulation, and confirmation.
    pub commitment: Commitment,
    /// How often to poll for a terminal outcome.
    pub poll_interval: Duration,
    /// Hard cap on tracking; past it the flight is reported as failed.
    pub deadline: Duration,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            commitment: Commitment::Confirmed,
            poll_interval: Duration::from_millis(400),
            deadline: Duration::from_secs(90),
        }
    }
}

/// Fail-closed execution pipeline for Solana transactions.
///
/// Build one with [`Pipeline::new`], attach gates with
/// [`Pipeline::with_gate`], then call [`Pipeline::run`] per transaction.
pub struct Pipeline {
    rpc: RpcClient,
    jito: Option<JitoClient>,
    gates: Vec<Box<dyn Gate>>,
    config: PipelineConfig,
}

impl Pipeline {
    /// Create a pipeline against an RPC endpoint with default config and no
    /// gates. A pipeline without gates submits everything it can sign; add
    /// gates before trusting it with anything that matters.
    pub fn new(rpc_url: &str) -> Self {
        let config = PipelineConfig::default();
        Self {
            rpc: RpcClient::new(rpc_url, config.commitment),
            jito: None,
            gates: Vec::new(),
            config,
        }
    }

    /// Enable the Jito bundle route via a block engine URL.
    pub fn with_jito(mut self, block_engine_url: &str) -> Self {
        self.jito = Some(JitoClient::new(block_engine_url));
        self
    }

    /// Append a gate. Gates run in insertion order; the first rejection stops
    /// the pipeline before anything is submitted.
    pub fn with_gate(mut self, gate: impl Gate + 'static) -> Self {
        self.gates.push(Box::new(gate));
        self
    }

    /// Replace the default config.
    pub fn with_config(mut self, config: PipelineConfig) -> Self {
        self.rpc = RpcClient::new(self.rpc.url(), config.commitment);
        self.config = config;
        self
    }

    /// The underlying RPC client, for setup chores like airdrops.
    pub fn rpc(&self) -> &RpcClient {
        &self.rpc
    }

    /// Run one transaction through assemble → gate → submit → track.
    ///
    /// On the [`Route::JitoBundle`] route a tip transfer to an official tip
    /// account is appended to `instructions` before signing.
    ///
    /// Gate-context I/O failures reject the transaction (`Error::Rejected`
    /// with gate `context`) rather than skipping checks: unknown is not safe.
    pub async fn run(
        &self,
        mut instructions: Vec<Instruction>,
        payer: &Keypair,
        route: Route,
    ) -> Result<Flight> {
        let mut timing = Timing::default();

        // Assemble: fresh blockhash, optional tip, sign.
        let t = Instant::now();
        let blockhash = self.rpc.latest_blockhash().await?;

        let tip = match route {
            Route::Rpc => 0,
            Route::JitoBundle { tip_lamports } => {
                if self.jito.is_none() {
                    return Err(Error::BundleRejected(
                        "pipeline has no block engine; call with_jito()".into(),
                    ));
                }
                instructions.push(tip_instruction(&payer.pubkey(), tip_lamports));
                tip_lamports
            }
        };

        // The blockhash goes into the message itself, not just the signature:
        // `getFeeForMessage` prices the wire bytes and returns null for a
        // message carrying the default (zero) blockhash.
        let message = Message::new_with_blockhash(&instructions, Some(&payer.pubkey()), &blockhash);
        let mut tx = Transaction::new_unsigned(message.clone());
        tx.try_sign(&[payer], blockhash)
            .map_err(|e| Error::Serialization(format!("signing: {e}")))?;
        timing.assemble = t.elapsed();

        // Gate: assemble the context fail-closed, then run every gate.
        let t = Instant::now();
        let fail_closed = |what: &str, e: Error| {
            Error::Rejected(Rejection {
                gate: "context",
                reason: format!("fail-closed on {what}: {e}"),
            })
        };

        let payer_balance = self
            .rpc
            .balance(&payer.pubkey())
            .await
            .map_err(|e| fail_closed("balance fetch", e))?;
        let fee = self
            .rpc
            .fee_for_message(&message)
            .await
            .map_err(|e| fail_closed("fee calculation", e))?;
        let sim = self
            .rpc
            .simulate(&tx)
            .await
            .map_err(|e| fail_closed("simulation", e))?;

        let ctx = GateCtx {
            tx: &tx,
            payer: payer.pubkey(),
            payer_balance,
            fee,
            tip,
            sim: &sim,
        };
        for gate in &self.gates {
            gate.check(&ctx).map_err(Error::Rejected)?;
        }
        timing.gate = t.elapsed();

        // Submit.
        let t = Instant::now();
        let signature = tx.signatures[0];
        let bundle_id = match &route {
            Route::Rpc => {
                self.rpc.send_transaction(&tx).await?;
                None
            }
            Route::JitoBundle { .. } => {
                let encoded = JitoClient::build_bundle(std::slice::from_ref(&tx))?;
                let jito = self.jito.as_ref().expect("checked during assemble");
                Some(jito.send_bundle(encoded).await?)
            }
        };
        timing.submit = t.elapsed();

        // Track to a terminal outcome.
        let t = Instant::now();
        let outcome = self
            .track(&signature, bundle_id.as_deref(), &blockhash)
            .await?;
        timing.confirm = t.elapsed();

        let flight = Flight {
            signature,
            bundle_id,
            outcome,
            timing,
        };
        info!(
            sig = %flight.signature,
            outcome = ?flight.outcome,
            total_ms = format!("{:.1}", timing.total().as_secs_f64() * 1e3),
            "flight complete"
        );
        Ok(flight)
    }

    async fn track(
        &self,
        signature: &Signature,
        bundle_id: Option<&str>,
        blockhash: &Hash,
    ) -> Result<Outcome> {
        let deadline = Instant::now() + self.config.deadline;

        loop {
            // Signature status is the source of truth for landing: a bundle's
            // transactions confirm like any other once the bundle lands.
            if let Some(status) = self.rpc.signature_status(signature).await? {
                if let Some(err) = status.err {
                    return Ok(Outcome::Failed { reason: err });
                }
                if status.satisfies(self.config.commitment) {
                    return Ok(Outcome::Landed { slot: status.slot });
                }
            }

            // Early terminal states from the block engine, best-effort: the
            // signature path above still lands the flight if this errors.
            if let (Some(id), Some(jito)) = (bundle_id, &self.jito) {
                match jito.bundle_statuses(&[id.to_string()]).await {
                    Ok(statuses) => {
                        if let Some(bs) = statuses.iter().find(|s| s.bundle_id == id) {
                            match bs.status.as_str() {
                                "Landed" => {
                                    if let Some(slot) = bs.landed_slot {
                                        return Ok(Outcome::Landed { slot });
                                    }
                                }
                                "Failed" | "Invalid" => {
                                    return Ok(Outcome::Failed {
                                        reason: format!("block engine: {}", bs.status),
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) => warn!(error = %e, "bundle status poll failed; continuing"),
                }
            }

            // No status and an expired blockhash means the transaction can
            // never land; report it as such, distinctly from failure.
            if !self.rpc.is_blockhash_valid(blockhash).await? {
                return Ok(Outcome::Expired);
            }

            if Instant::now() >= deadline {
                return Ok(Outcome::Failed {
                    reason: format!(
                        "no terminal status within the {:?} tracking deadline",
                        self.config.deadline
                    ),
                });
            }
            tokio::time::sleep(self.config.poll_interval).await;
        }
    }
}
