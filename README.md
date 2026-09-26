# landed

[![Crates.io](https://img.shields.io/crates/v/landed.svg)](https://crates.io/crates/landed)
[![Documentation](https://docs.rs/landed/badge.svg)](https://docs.rs/landed)
[![CI](https://github.com/ampactor-labs/landed/actions/workflows/ci.yml/badge.svg)](https://github.com/ampactor-labs/landed/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/landed.svg)](#license)

A Rust library that sends a Solana blockchain transaction only after live checks approve it, then reports whether it landed and how long each stage took. It submits through an RPC node (a Solana server that takes JSON requests over HTTP) or as a Jito bundle (a tipped submission relayed to the validators that produce blocks). A check that cannot get its data rejects the transaction. It is built on solana-sdk 4 and tokio, and CI tests it against a local validator.

**Status: working.** The API may change before 1.0, priority fees are not a gate input yet, and an expired transaction is left to the caller to rebuild.

Package: https://crates.io/crates/landed · Docs: https://docs.rs/landed

## Usage

```toml
[dependencies]
landed = "0.1"
```

Build a `Pipeline` from an RPC URL, add gates (the checks), and call `run` with the transaction's instructions (the program calls it makes) and the payer's keypair. `run` returns a `Flight`: the outcome (`Landed { slot }`, `Expired` or `Failed { reason }`) and the time spent in each stage. When a gate rejects, `run` returns `Err(Error::Rejected(..))` with the gate's name and reason, and nothing is sent. Amounts are in lamports (the smallest unit of SOL, Solana's currency).

```rust
use landed::{BalanceFloor, ComputeCeiling, FeeCeiling, Pipeline, Route, SimulationMustPass};
use solana_sdk::{instruction::Instruction, signature::Keypair};

# async fn demo(instructions: Vec<Instruction>, payer: Keypair) -> landed::Result<()> {
let pipeline = Pipeline::new("https://api.mainnet-beta.solana.com")
    .with_jito("https://mainnet.block-engine.jito.wtf")
    .with_gate(SimulationMustPass)
    .with_gate(ComputeCeiling(200_000))
    .with_gate(FeeCeiling(20_000))
    .with_gate(BalanceFloor(10_000_000));

// Plain RPC.
let flight = pipeline.run(instructions.clone(), &payer, Route::Rpc).await?;

// Or as a Jito bundle; the tip transfer is appended before signing.
let flight = pipeline
    .run(instructions, &payer, Route::JitoBundle { tip_lamports: 100_000 })
    .await?;

println!("{:?} after {:?}", flight.outcome, flight.timing.total());
# Ok(())
# }
```

### Built-in gates

Gates run in the order you add them, and the first rejection stops the run. A pipeline with no gates submits anything it can sign.

| Gate                 | Rejects when                                                                                                    |
| -------------------- | --------------------------------------------------------------------------------------------------------------- |
| `SimulationMustPass` | the simulation run before submission returned an error                                                          |
| `ComputeCeiling(n)`  | the simulation used more than `n` compute units (Solana's measure of a transaction's work), or reported no usage |
| `FeeCeiling(n)`      | the network fee for the message is above `n` lamports                                                           |
| `TipCeiling(n)`      | the Jito tip is above `n` lamports (a tip of 0, as on the RPC route, always passes)                              |
| `BalanceFloor(n)`    | paying the fee and the tip would leave the payer with less than `n` lamports                                    |

### Writing a gate

A gate is a trait with two methods:

```rust
struct MarketHours;

impl landed::Gate for MarketHours {
    fn name(&self) -> &'static str { "market_hours" }
    fn check(&self, _ctx: &landed::GateCtx<'_>) -> Result<(), landed::Rejection> {
        // Return Err(Rejection { .. }) and nothing is submitted.
        Ok(())
    }
}
```

`GateCtx` holds the signed transaction, the payer, its balance, the fee, the tip and the simulation result (error, compute units and logs).

### Settings

`Pipeline::with_config` takes a `PipelineConfig` with three fields. `commitment` is how final a transaction must be before it counts as landed (`Processed`, `Confirmed` or `Finalized`; default `Confirmed`). `poll_interval` is how often tracking polls (default 400 ms). `deadline` caps tracking (default 90 s); past it the flight is `Failed`.

## How it works

```text
assemble ──► gate ──► submit ──► track
 blockhash    balance   RPC or    signature status
 (+ tip)      fee       Jito      bundle status
 sign         simulate  bundle    blockhash expiry
```

`Pipeline::run` takes one transaction through four timed stages:

1. **Assemble** fetches a recent blockhash (the hash of a recent block, which every transaction must carry and which expires soon after), appends the tip transfer on the Jito route, and signs. The tip goes to one of Jito's eight official tip accounts, picked by a timestamp hash to spread load.
2. **Gate** fetches the payer's balance, prices the message with `getFeeForMessage`, simulates the signed transaction, and runs every gate over the result.
3. **Submit** calls `sendTransaction` with the node's own preflight simulation skipped (the gate stage already simulated), or encodes a bundle and posts it to the block engine's `sendBundle`.
4. **Track** polls the signature status until it reaches the configured commitment (`Landed`) or reports an error (`Failed`). On the bundle route it also asks the block engine for an early `Landed`, `Failed` or `Invalid`, and carries on if that call errors. If the blockhash expires first the outcome is `Expired`, and past the deadline it is `Failed`.

### Fail-closed gates

A transaction is submitted only after every gate approves. Gates are pure functions over a `GateCtx` that the pipeline builds from live chain state before any gate runs. If the balance, the fee or the simulation cannot be fetched, the pipeline rejects the transaction (gate name `context`) and runs no gates. I chose this because an unknown value is not a safe value.

`ComputeCeiling` shows the sharp edge of that rule. It rejects when the node reports no compute usage at all, because "the node did not say" and "it fits the budget" are different facts, and only the second is safe to act on.

### Why it speaks JSON-RPC directly

`src/rpc.rs` is a small JSON-RPC client: the seven methods the pipeline calls, plus `requestAirdrop` for tests, in about 250 lines of code. The Jito client in `src/submit.rs` uses the same HTTP client (`reqwest`). I left out `solana-client` for two reasons. It brings the full RPC stack (websockets, transaction-status types, the parsed-transaction tree) into a crate whose job is to submit one transaction. And when I wrote this (July 2026), its stable 4.x release resolved `solana-transaction-status-client-types` against version 0.5 of the `wincode` serialization crate, while `solana-sdk`'s own types implement the traits from `wincode` 0.6, so the two did not compile together. `solana-client` 4.3.0 (September 2026) resolves `wincode` 0.6; whether it now builds alongside this crate is untested.

## Benchmarks

`examples/demo.rs` sends 50 gated self-transfers (the payer sends 1,000 lamports to itself) through the RPC route to a local `solana-test-validator`, polls every 25 ms, and prints per-stage percentiles.

```sh
solana-test-validator --quiet --reset &
cargo run --release --example demo
```

The table shows two runs, in milliseconds. The earlier run is from the first version of this README, and its hardware was not recorded. The rerun is from 2026-09-26, in a cloud container with 4 vCPUs (Intel Xeon at 2.8 GHz), using `solana-test-validator` from Agave 4.2.2 (the Solana validator software). Both runs landed all 50 transactions, with none expired, failed or rejected.

| stage    | earlier p50 | earlier p90 | earlier p99 | rerun p50 | rerun p90 | rerun p99 |
| -------- | ----------- | ----------- | ----------- | --------- | --------- | --------- |
| assemble | 0.2         | 0.3         | 1.0         | 0.3       | 0.5       | 0.7       |
| gate     | 0.6         | 1.0         | 2.8         | 1.0       | 1.8       | 5.4       |
| submit   | 0.2         | 0.2         | 1.0         | 0.3       | 0.5       | 2.5       |
| confirm  | 427.4       | 429.3       | 512.9       | 435.3     | 491.0     | 518.4     |
| total    | 428.4       | 430.7       | 517.8       | 437.0     | 492.8     | 520.6     |

Read this table for its shape. A local validator measures this crate's own overhead: assemble, gate and submit together take one to two milliseconds at the median, across five RPC calls (three of them in the gate stage). `confirm` is mostly the test validator's slot time (a slot is Solana's block interval). On a real cluster the first three rows grow with your RPC latency, and the confirm row depends on the network. To see numbers that generalize, point the demo at devnet (Solana's public test network) with your own funded keypair:

```sh
cargo run --release --example demo -- https://api.devnet.solana.com 25 ~/.config/solana/id.json
```

The poll interval can distort the confirm row. At the 400 ms default, that row measures the poll granularity, so the demo sets the interval to 25 ms before measuring.

## Project layout

```text
src/pipeline.rs     the four stages and the tracking loop
src/gate.rs         Gate trait, GateCtx and the built-in gates
src/rpc.rs          JSON-RPC client for the Solana node
src/submit.rs       Route, the Jito block engine client and tip accounts
src/track.rs        Flight, Outcome and Timing
src/metrics.rs      percentile report used by the demo
examples/demo.rs    the latency run behind Benchmarks
tests/validator.rs  integration tests against a live validator
```

## Testing

```sh
cargo test
```

This runs 23 unit tests (gates, bundle encoding, tip accounts, commitment handling, timing and percentiles). It also compiles the example in `src/lib.rs` and every Rust block in this README, which `src/lib.rs` includes as doctests. The integration tests skip themselves unless `LANDED_RPC_URL` is set.

The integration tests in `tests/validator.rs` run the pipeline against a real validator:

```sh
solana-test-validator --quiet --reset &
LANDED_RPC_URL=http://127.0.0.1:8899 cargo test --test validator -- --nocapture
```

They land a gated self-transfer, check that `FeeCeiling` and `BalanceFloor` reject before submission, check that an unfunded payer is stopped at simulation, and check that the bundle route refuses to run without a block engine.

CI (`.github/workflows/ci.yml`) runs on every push to master and every pull request. One job runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`. A second job installs the Agave toolchain, starts `solana-test-validator` and runs the integration tests against it.

Nothing tests the Jito route against a real or mocked block engine, so `sendBundle` and `getBundleStatuses` are unexercised. The `Expired` outcome and the tracking deadline have no test, and the `TipCeiling` unit test covers only the zero tip of the RPC route.

### A bug the validator caught

On the first run against the validator, every gate failed closed with "fee unavailable: blockhash no longer valid". `Message::new` leaves the blockhash zeroed, and `getFeeForMessage` prices the exact bytes it is given, so the node returned no fee. The fix was `Message::new_with_blockhash`. A mocked RPC would have returned a fee and let the bug ship.

## Limitations

The latency numbers come from a local test validator, so they describe this library's overhead and say nothing about Solana's public networks. The Jito route puts one transaction in each bundle and has not been tested against a real block engine. Priority fees (the optional extra fee that buys earlier inclusion) pass the fee gate unchecked. When a transaction expires, the pipeline stops and leaves the rebuild to the caller.

- **One transaction per bundle.** `JitoClient::build_bundle` accepts up to the block engine's limit of five transactions, but `Pipeline::run` submits one signed transaction with its tip. Multi-transaction bundles, where several transactions land together or not at all, are not wired up.
- **Priority fees are not a gate input.** `FeeCeiling` sees the base fee for the message. A compute-unit price set through a `ComputeBudget` instruction passes unexamined.
- **`Expired` ends the run.** The outcome means the blockhash died and the transaction can never land. The pipeline does not retry on a fresh blockhash.
- **`BalanceFloor` counts only the fee and the tip.** Lamports that the instructions themselves move are not modeled, so the floor has to cover them.
- **No strategy.** The crate holds no trading signals and makes no decision about what to send. This is the execution code from [flowpilot](https://github.com/ampactor-labs/flowpilot), a Solana trading engine I retired when its edge stopped covering fees, extracted and rebuilt.

## License

Licensed under either the [MIT license](LICENSE-MIT) or the [Apache License 2.0](LICENSE-APACHE), at your option. `Cargo.toml` declares `MIT OR Apache-2.0`.
