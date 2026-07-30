//! Integration tests against a live validator.
//!
//! Correctness measured in CI, not asserted: the CI workflow starts a real
//! `solana-test-validator` and runs every test below against it. Locally,
//! set `LANDED_RPC_URL` (usually `http://127.0.0.1:8899`) to run them; when
//! unset, each test skips with a note instead of failing.

use std::time::Duration;

use landed::{
    BalanceFloor, Commitment, Error, FeeCeiling, Outcome, Pipeline, PipelineConfig, Route,
    SimulationMustPass,
};
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use solana_system_interface::instruction as system_instruction;

fn rpc_url() -> Option<String> {
    match std::env::var("LANDED_RPC_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!(
                "skipping: LANDED_RPC_URL not set (start solana-test-validator and export it)"
            );
            None
        }
    }
}

fn test_config() -> PipelineConfig {
    PipelineConfig {
        commitment: Commitment::Confirmed,
        poll_interval: Duration::from_millis(200),
        deadline: Duration::from_secs(60),
    }
}

async fn funded_payer(pipeline: &Pipeline) -> Keypair {
    let payer = Keypair::new();
    pipeline
        .rpc()
        .request_airdrop(&payer.pubkey(), 2_000_000_000)
        .await
        .expect("airdrop request against test validator");
    for _ in 0..100 {
        if pipeline.rpc().balance(&payer.pubkey()).await.unwrap_or(0) > 0 {
            return payer;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("airdrop never confirmed");
}

fn self_transfer(payer: &Keypair) -> solana_sdk::instruction::Instruction {
    system_instruction::transfer(&payer.pubkey(), &payer.pubkey(), 1_000)
}

#[tokio::test]
async fn lands_a_gated_self_transfer() {
    let Some(url) = rpc_url() else { return };
    let pipeline = Pipeline::new(&url)
        .with_config(test_config())
        .with_gate(SimulationMustPass)
        .with_gate(FeeCeiling(20_000))
        .with_gate(BalanceFloor(500_000));
    let payer = funded_payer(&pipeline).await;

    let flight = pipeline
        .run(vec![self_transfer(&payer)], &payer, Route::Rpc)
        .await
        .expect("gated transfer should submit");

    match flight.outcome {
        Outcome::Landed { slot } => assert!(slot > 0, "landed slot should be nonzero"),
        other => panic!("expected Landed, got {other:?}"),
    }
    assert!(flight.timing.total() > Duration::ZERO);
    assert!(flight.bundle_id.is_none());
}

#[tokio::test]
async fn fee_ceiling_rejects_before_submission() {
    let Some(url) = rpc_url() else { return };
    let pipeline = Pipeline::new(&url)
        .with_config(test_config())
        .with_gate(FeeCeiling(0));
    let payer = funded_payer(&pipeline).await;

    let err = pipeline
        .run(vec![self_transfer(&payer)], &payer, Route::Rpc)
        .await
        .expect_err("zero fee ceiling must reject");

    match err {
        Error::Rejected(r) => assert_eq!(r.gate, "fee_ceiling"),
        other => panic!("expected Rejected, got {other}"),
    }
}

#[tokio::test]
async fn balance_floor_rejects_before_submission() {
    let Some(url) = rpc_url() else { return };
    let pipeline = Pipeline::new(&url)
        .with_config(test_config())
        .with_gate(BalanceFloor(u64::MAX));
    let payer = funded_payer(&pipeline).await;

    let err = pipeline
        .run(vec![self_transfer(&payer)], &payer, Route::Rpc)
        .await
        .expect_err("impossible balance floor must reject");

    match err {
        Error::Rejected(r) => assert_eq!(r.gate, "balance_floor"),
        other => panic!("expected Rejected, got {other}"),
    }
}

#[tokio::test]
async fn unfunded_payer_fails_closed_at_simulation() {
    let Some(url) = rpc_url() else { return };
    let pipeline = Pipeline::new(&url)
        .with_config(test_config())
        .with_gate(SimulationMustPass);
    // Deliberately no airdrop: the payer does not exist on chain.
    let payer = Keypair::new();

    let err = pipeline
        .run(vec![self_transfer(&payer)], &payer, Route::Rpc)
        .await
        .expect_err("an unfunded payer must never reach submission");

    match err {
        Error::Rejected(r) => assert_eq!(r.gate, "simulation"),
        other => panic!("expected Rejected, got {other}"),
    }
}

#[tokio::test]
async fn jito_route_without_block_engine_is_refused() {
    let Some(url) = rpc_url() else { return };
    let pipeline = Pipeline::new(&url).with_config(test_config());
    let payer = funded_payer(&pipeline).await;

    let err = pipeline
        .run(
            vec![self_transfer(&payer)],
            &payer,
            Route::JitoBundle {
                tip_lamports: 1_000,
            },
        )
        .await
        .expect_err("bundle route without with_jito() must refuse");

    assert!(err.to_string().contains("no block engine"));
}
