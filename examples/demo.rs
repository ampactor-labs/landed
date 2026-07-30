//! Measured demo: run N self-transfers through the pipeline and print
//! per-stage latency percentiles.
//!
//! ```sh
//! # against a local test validator (default url, ephemeral airdropped payer)
//! solana-test-validator --quiet --reset &
//! cargo run --release --example demo
//!
//! # against devnet with your own keypair
//! cargo run --release --example demo -- https://api.devnet.solana.com 25 ~/.config/solana/id.json
//! ```

use std::time::Duration;

use landed::{
    stage_report, BalanceFloor, ComputeCeiling, FeeCeiling, Outcome, Pipeline, PipelineConfig,
    Route, SimulationMustPass,
};
use solana_sdk::signature::{read_keypair_file, Keypair};
use solana_sdk::signer::Signer;
use solana_system_interface::instruction as system_instruction;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let rpc_url = args
        .first()
        .map(String::as_str)
        .unwrap_or("http://127.0.0.1:8899");
    let n: usize = args
        .get(1)
        .map(|a| a.parse().expect("N must be an integer"))
        .unwrap_or(50);

    // Poll faster than the 400 ms default: at the default interval the
    // reported confirm time is the poll granularity, not the network's.
    let config = PipelineConfig {
        poll_interval: Duration::from_millis(25),
        ..PipelineConfig::default()
    };

    let pipeline = Pipeline::new(rpc_url)
        .with_config(config)
        .with_gate(SimulationMustPass)
        .with_gate(ComputeCeiling(200_000))
        .with_gate(FeeCeiling(20_000))
        .with_gate(BalanceFloor(500_000));

    let payer = match args.get(2) {
        Some(path) => read_keypair_file(path).expect("readable keypair file"),
        None => {
            let kp = Keypair::new();
            println!("ephemeral payer {}, requesting airdrop…", kp.pubkey());
            pipeline
                .rpc()
                .request_airdrop(&kp.pubkey(), 2_000_000_000)
                .await
                .expect("airdrop request (is a test validator running, or use a keypair?)");
            for _ in 0..60 {
                if pipeline.rpc().balance(&kp.pubkey()).await.unwrap_or(0) > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            assert!(
                pipeline.rpc().balance(&kp.pubkey()).await.unwrap_or(0) > 0,
                "airdrop never arrived"
            );
            kp
        }
    };

    println!("running {n} self-transfers against {rpc_url}");
    let mut flights = Vec::with_capacity(n);
    let (mut landed_ct, mut expired, mut failed, mut rejected) = (0u32, 0u32, 0u32, 0u32);

    for i in 0..n {
        let ix = system_instruction::transfer(&payer.pubkey(), &payer.pubkey(), 1_000);
        match pipeline.run(vec![ix], &payer, Route::Rpc).await {
            Ok(flight) => {
                match &flight.outcome {
                    Outcome::Landed { .. } => landed_ct += 1,
                    Outcome::Expired => expired += 1,
                    Outcome::Failed { reason } => {
                        failed += 1;
                        eprintln!("flight {i} failed: {reason}");
                    }
                }
                flights.push(flight);
            }
            Err(e) => {
                rejected += 1;
                eprintln!("flight {i} rejected: {e}");
            }
        }
    }

    println!();
    println!(
        "outcomes: {landed_ct} landed, {expired} expired, {failed} failed, {rejected} rejected"
    );
    println!();
    print!("{}", stage_report(&flights));
    println!();
    println!("note: a local test validator measures pipeline overhead, not network");
    println!("latency; devnet/mainnet numbers are the ones that generalize.");
}
