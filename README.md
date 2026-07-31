# landed

[![Crates.io](https://img.shields.io/crates/v/landed.svg)](https://crates.io/crates/landed)
[![Documentation](https://docs.rs/landed/badge.svg)](https://docs.rs/landed)
[![CI](https://github.com/ampactor-labs/landed/actions/workflows/ci.yml/badge.svg)](https://github.com/ampactor-labs/landed/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/landed.svg)](#license)

Fail-closed Solana transaction execution: plan it, gate it, submit it by RPC or Jito bundle, and know what happened.

```toml
[dependencies]
landed = "0.1"
```

```text
assemble ──► gate ──► submit ──► track
 blockhash    balance   RPC or    signature status
 (+ tip)      fee       Jito      bundle status
 sign         simulate  bundle    blockhash expiry
```

Every stage is timed. What comes back is a `Flight`: the outcome (`Landed { slot }`, `Expired`, or `Failed { reason }`) and where the milliseconds went.

## The law

A transaction is submitted only after every gate approves. Gates read a context the pipeline assembles from live chain state (balance, fee, simulation result), and if any of that evidence cannot be fetched, the transaction is rejected rather than submitted with the check skipped. Unknown is not safe.

That rule has a sharp edge, on purpose. `ComputeCeiling` rejects when the RPC reports no compute consumption at all, because "the node didn't tell me" and "it fits in budget" are different facts and only one of them is safe to act on.

## Usage

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

Writing your own gate is one method:

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

## Measured

50 gated self-transfers through the RPC route against a local `solana-test-validator`, polling at 25 ms (`cargo run --release --example demo`):

| stage | p50 ms | p90 ms | p99 ms |
|---|---|---|---|
| assemble | 0.2 | 0.3 | 1.0 |
| gate | 0.6 | 1.0 | 2.8 |
| submit | 0.2 | 0.2 | 1.0 |
| confirm | 427.4 | 429.3 | 512.9 |
| total | 428.4 | 430.7 | 517.8 |

50 landed, 0 expired, 0 failed, 0 rejected.

Read that table for the shape, not the absolute numbers. A local validator measures this crate's own overhead, about a millisecond of assemble, gate, and submit combined across three RPC round trips of which the simulation dominates; `confirm` is just the test validator's slot time. On a real cluster the first three rows grow with your RPC latency and the fourth is the network's business, not this crate's. Point the demo at devnet with your own keypair to see numbers that generalize:

```sh
cargo run --release --example demo -- https://api.devnet.solana.com 25 ~/.config/solana/id.json
```

Poll interval is worth a word, since it is the one knob that can lie to you. The default is 400 ms; at that setting the reported confirm time is the poll granularity rather than the cluster's, which is why the demo drops it to 25 ms before measuring anything.

## Correctness measured in CI

The unit tests cover the gates and the bundle encoding. The integration tests do something more useful: CI starts a real `solana-test-validator` on every push and runs the pipeline against it, landing actual transactions and asserting that each gate rejects before submission when it should.

That design earned its keep on the first run. The gates all failed closed with "fee unavailable: blockhash no longer valid," because `Message::new` leaves the blockhash zeroed and `getFeeForMessage` prices the wire bytes it is handed; the fix is `Message::new_with_blockhash`. A mocked RPC would have returned a cheerful fee and shipped the bug.

## No `solana-client`

This crate talks JSON-RPC directly over the same HTTP client the Jito block engine uses. Seven calls, about 200 lines, in `src/rpc.rs`.

Two reasons. `solana-client` drags the full RPC stack (websockets, transaction-status types, the parsed-transaction tree) into a dependency tree whose job is submitting one transaction. And as of Solana 4.x the stable release resolves `solana-transaction-status-client-types` against `wincode` 0.5 while `solana-sdk`'s own types implement the traits from `wincode` 0.6, so the two do not compile together; only pre-release versions fix it. Speaking the protocol directly sidesteps a class of problem instead of pinning around one instance of it.

## What this is not

There is no strategy here. No signals, no alpha, no opinion about what you should send: it is the execution machinery, extracted and rebuilt from [flowpilot](https://github.com/ampactor-labs/flowpilot), a Solana trading engine I retired when its edge stopped clearing fees. The strategy died on the evidence. The engineering was worth keeping.

## License

MIT or Apache 2.0, at your option.
