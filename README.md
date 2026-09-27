# landed

[![CI](https://github.com/ampactor-labs/landed/actions/workflows/ci.yml/badge.svg)](https://github.com/ampactor-labs/landed/actions/workflows/ci.yml) [![Crates.io](https://img.shields.io/crates/v/landed.svg)](https://crates.io/crates/landed) [![Documentation](https://docs.rs/landed/badge.svg)](https://docs.rs/landed) [![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/landed.svg)](#license)

A Rust crate that sends Solana transactions through fail-closed gates, by RPC or as a Jito bundle, and reports how each one ended. A gate is a check that can stop a transaction before it is sent, and fail-closed means the transaction is not sent when the evidence the gates read (balance, fee, simulation) cannot be fetched. A Jito bundle goes through Jito's block engine to validators and pays a tip for inclusion. Integration tests run in CI against a real local validator.

**Status: working.** Version 0.1.0 is on crates.io, and the API may still change.

Package: https://crates.io/crates/landed · Docs: https://docs.rs/landed

## Usage

```toml
[dependencies]
landed = "0.1"
```

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

`Pipeline::new` starts with no gates. Gates run in the order you add them, and the first rejection ends the run with `Error::Rejected` before anything is sent. Without gates, a pipeline sends any transaction it can sign as long as its RPC calls succeed, even one whose simulation fails. The Jito route needs `with_jito`, and without a block engine URL `run` returns `Error::BundleRejected`.

### Built-in gates

| Gate | Rejects when |
|---|---|
| `SimulationMustPass` | the simulation returned an error |
| `ComputeCeiling(n)` | the simulation used more than `n` compute units (Solana's measure of execution cost), or reported no count at all |
| `FeeCeiling(n)` | the fee reported by `getFeeForMessage` is above `n` lamports (the smallest unit of SOL) |
| `TipCeiling(n)` | the Jito tip is above `n` lamports; the tip is 0 on the RPC route, so this gate always passes there |
| `BalanceFloor(n)` | the payer's balance minus fee and tip would fall below `n` lamports; lamports that the instructions themselves move are not counted |

### Writing a gate

A gate implements `Gate`: a `name` used in rejections and logs, and a `check` over a `GateCtx`. The context holds the signed transaction, the payer and its balance, the fee, the tip and the simulation result.

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

### Configuration

`PipelineConfig`, passed with `with_config`, holds the commitment level (default `Confirmed`), the poll interval (default 400 ms) and the tracking deadline (default 90 s). Commitment is how settled a block must be: `Confirmed` means a supermajority of the cluster has voted on it. The pipeline queries at that level and counts a transaction as landed once it reaches it.

## How it works

```text
assemble ──► gate ──► submit ──► track
 blockhash    balance   RPC or    signature status
 (+ tip)      fee       Jito      bundle status
 sign         simulate  bundle    blockhash expiry
```

`Pipeline::run` in `src/pipeline.rs` takes one transaction through four stages and times each one. It talks to an RPC node, the server that answers Solana's JSON-RPC API for reading chain state and accepting transactions.

1. **Assemble** fetches a recent blockhash. Every Solana transaction must carry the hash of a recent block, and a transaction whose blockhash has expired can never land. On the Jito route this stage appends a tip transfer to one of Jito's eight tip accounts. It then builds the message on that blockhash and signs it.
2. **Gate** fetches the evidence: the payer's balance, the fee for the message, and a simulation, in which the node executes the signed transaction without recording it. Then the gates run.
3. **Submit** calls `sendTransaction` with the node's own preflight simulation turned off, because the gate stage already simulated the transaction. On the Jito route it posts a one-transaction bundle to the block engine (`src/submit.rs`).
4. **Track** polls `getSignatureStatuses` until the transaction reaches the configured commitment. On the Jito route it also asks the block engine for the bundle's status.

`run` returns a `Flight` (`src/track.rs`) with the signature, the bundle id on the Jito route, the time spent in each stage and an outcome. The outcome is `Landed { slot }` with the slot it landed in, `Expired` when the blockhash expired first, or `Failed { reason }` when the cluster or the block engine reports an error or the tracking deadline passes. A slot is the window in which one validator, the leader, may produce a block. `stage_report` in `src/metrics.rs` turns a list of flights into the percentile table under Benchmarks.

### The fail-closed rule

`run` sends a transaction only after every gate approves it. The built-in gates do no I/O; they read a `GateCtx` that the pipeline fills first. If the balance, fee or simulation call fails, `run` returns `Error::Rejected` from a gate named `context` and sends nothing.

`ComputeCeiling` applies the same rule to missing data. It rejects when the simulation reports no compute-unit count, because a missing count does not show that the transaction fits under the ceiling.

### Why there is no `solana-client` dependency

The crate calls the JSON-RPC API directly with `reqwest`, the HTTP library its Jito client also uses. `src/rpc.rs` implements the seven methods the pipeline calls, plus `requestAirdrop` for the tests and the demo, in 259 non-blank, non-comment lines before its tests.

I chose this for two reasons. `solana-client` pulls in websocket, QUIC and UDP clients and the transaction-status types, and a crate that submits one transaction at a time uses none of them. When 0.1.0 shipped in July 2026, the stable `solana-transaction-status-client-types` (4.1.2) also required `wincode` 0.5, a serialization crate, while the `solana-sdk` 4 crates in this lockfile use `wincode` 0.6, so the two did not compile together. `solana-transaction-status-client-types` 4.3.0 (September 2026) moved to `wincode` 0.6, which may clear that conflict; this crate has not been tried against it.

## Benchmarks

The demo in `examples/demo.rs` sends 50 gated self-transfers through the RPC route and prints per-stage latency percentiles. For the table below it ran against a local `solana-test-validator`, a single-node Solana cluster for testing, and polled every 25 ms. The validator and the demo ran on one machine, whose hardware was not recorded. To reproduce it, start `solana-test-validator --quiet --reset` and run:

```sh
cargo run --release --example demo
```

| stage | p50 ms | p90 ms | p99 ms |
|---|---|---|---|
| assemble | 0.2 | 0.3 | 1.0 |
| gate | 0.6 | 1.0 | 2.8 |
| submit | 0.2 | 0.2 | 1.0 |
| confirm | 427.4 | 429.3 | 512.9 |
| total | 428.4 | 430.7 | 517.8 |

The percentiles are nearest-rank, and p50 is the median. The run ended with 50 landed, 0 expired, 0 failed and 0 rejected.

Read the table for its shape. Against a local validator, assemble, gate and submit take about 1 ms together at p50 (0.2 + 0.6 + 0.2 ms), across five RPC calls to the same machine, so those rows are close to the crate's own overhead. The confirm row is mostly the test validator's slot timing. On a real cluster the first three rows grow with your RPC latency, and the confirm row depends on the cluster.

The gate stage is where the design costs time. It makes three RPC calls (balance, fee, simulation) one after another before every send, so against a remote node it adds three network round trips that a bare `sendTransaction` would not pay. A local validator hides that cost.

The poll interval also shapes the confirm row. Tracking sleeps for the poll interval between checks, so at the default 400 ms the confirm time moves in steps of about 400 ms. The demo polls every 25 ms for that reason. For numbers from devnet (Solana's public test cluster), pass the URL, a flight count and a funded keypair:

```sh
cargo run --release --example demo -- https://api.devnet.solana.com 25 ~/.config/solana/id.json
```

## Testing

```sh
cargo test
```

This runs the unit tests, the doctests (code examples compiled as tests, including the two Rust examples in this README, which `src/lib.rs` pulls in with `include_str!`) and `tests/validator.rs`. The validator tests skip unless `LANDED_RPC_URL` is set. To run them, start `solana-test-validator --quiet --reset` in another terminal, wait until it answers, and run:

```sh
LANDED_RPC_URL=http://127.0.0.1:8899 cargo test --test validator -- --nocapture
```

CI (`.github/workflows/ci.yml`) runs two jobs on every push to `master` and on every pull request. The `test` job runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`. The `validator` job installs the Agave toolchain (Anza's Solana validator and command-line tools) from its stable channel, starts `solana-test-validator`, and runs the validator tests against it.

What the tests cover:

- Unit tests: the built-in gates' decisions (for `TipCeiling`, only the zero tip of the RPC route), including `ComputeCeiling` rejecting a missing compute count; bundle encoding and its local limits of one to five signed transactions; the tip transfer going to an official tip account; commitment ordering and parsing; and the percentile math.
- Validator tests: a gated self-transfer lands; `FeeCeiling(0)` and `BalanceFloor(u64::MAX)` reject before submission; `SimulationMustPass` stops an unfunded payer; and the Jito route refuses to run without a block engine.

Nothing tests a public cluster, a bundle sent to a real block engine, the `Expired` and deadline outcomes, or `ComputeCeiling` and `TipCeiling` outside unit tests.

The validator tests caught a real bug on their first CI run. Every transaction was rejected before submission with `fee unavailable: blockhash no longer valid`, because `Message::new` leaves the blockhash zeroed and `getFeeForMessage` returns null for a message whose blockhash the node does not know. The fix was `Message::new_with_blockhash`, and a comment in `src/pipeline.rs` records why. A mocked RPC would have returned a fee and hidden the bug.

## Limitations

The latency table and all integration tests come from a local test validator, so nothing here shows how the crate behaves on devnet or mainnet, Solana's public clusters. The Jito route is the least tested part: no test sends a bundle to a real block engine.

Bundles that `Pipeline::run` sends carry exactly one transaction. `JitoClient::build_bundle` accepts up to five, the block engine's limit, but `run` appends the tip to the one transaction it signs and sends that alone. Atomic multi-transaction bundles (all of them land, in order, or none do) are not wired up yet.

No gate looks at priority fees on their own. A priority fee is an optional price per compute unit, set with a `ComputeBudget` instruction, and the pipeline never adds one. Agave's `getFeeForMessage` includes it in the fee it reports (see `Bank::get_fee_for_message` in Agave 4.1.1), so `FeeCeiling` caps the base fee and the priority fee together. Capping the price alone takes a custom gate that reads the `ComputeBudget` instructions from `ctx.tx`.

The pipeline never retries. `Expired` means the blockhash expired before the transaction landed, so it can never land, and rebuilding it on a fresh blockhash is up to the caller. `Failed` also covers a flight that reached the tracking deadline while its blockhash was still valid, and that transaction may still land later.

It makes no decisions about what to send. `landed` is the execution code from [flowpilot](https://github.com/ampactor-labs/flowpilot), a Solana trading engine I retired when its strategy stopped earning more than it paid in fees, rebuilt as a standalone crate.

## License

MIT or Apache 2.0, at your option: see [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
