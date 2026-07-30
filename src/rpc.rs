//! A small JSON-RPC client covering exactly the seven calls this pipeline
//! needs.
//!
//! `landed` deliberately does not depend on `solana-client`. That crate
//! drags in the full RPC stack (websockets, transaction-status types, the
//! whole parsed-transaction tree) to submit one transaction, and as of
//! Solana 4.x its stable release resolves to a `wincode` version that
//! conflicts with the one `solana-sdk`'s own types implement. Speaking
//! JSON-RPC directly costs about 200 lines, keeps the dependency tree small
//! enough to audit, and cannot break that way. The Jito block engine speaks
//! the same protocol, so one HTTP client serves both routes.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use solana_sdk::hash::Hash;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;

use crate::error::{Error, Result};
use crate::gate::SimSummary;

/// How much confirmation a query or a landing requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Commitment {
    /// Seen by the queried node.
    Processed,
    /// Voted on by a supermajority.
    Confirmed,
    /// Rooted; irreversible short of a deep fork.
    Finalized,
}

impl Commitment {
    /// The string the RPC protocol uses.
    pub fn as_str(&self) -> &'static str {
        match self {
            Commitment::Processed => "processed",
            Commitment::Confirmed => "confirmed",
            Commitment::Finalized => "finalized",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "processed" => Some(Commitment::Processed),
            "confirmed" => Some(Commitment::Confirmed),
            "finalized" => Some(Commitment::Finalized),
            _ => None,
        }
    }
}

/// Confirmation state of a submitted signature.
#[derive(Debug, Clone)]
pub struct SignatureStatus {
    /// Slot the transaction was processed in.
    pub slot: u64,
    /// Execution error, stringified, when the transaction failed on chain.
    pub err: Option<String>,
    /// Commitment the cluster reports for it, when it reports one.
    pub confirmation: Option<Commitment>,
}

impl SignatureStatus {
    /// Whether this status meets or exceeds `wanted`.
    pub fn satisfies(&self, wanted: Commitment) -> bool {
        self.confirmation.map(|c| c >= wanted).unwrap_or(false)
    }
}

#[derive(Serialize)]
struct Request<'a, P> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: P,
}

#[derive(Deserialize)]
struct Response<R> {
    result: Option<R>,
    error: Option<RpcErrorBody>,
}

#[derive(Deserialize)]
struct RpcErrorBody {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct Ctx<T> {
    value: T,
}

#[derive(Deserialize)]
struct BlockhashValue {
    blockhash: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimValue {
    err: Option<serde_json::Value>,
    logs: Option<Vec<String>>,
    units_consumed: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusValue {
    slot: u64,
    err: Option<serde_json::Value>,
    confirmation_status: Option<String>,
}

/// JSON-RPC client for a Solana node.
pub struct RpcClient {
    http: Client,
    url: String,
    commitment: Commitment,
}

impl RpcClient {
    /// Create a client for an RPC endpoint.
    pub fn new(url: &str, commitment: Commitment) -> Self {
        Self {
            http: Client::new(),
            url: url.trim_end_matches('/').to_string(),
            commitment,
        }
    }

    /// The endpoint this client talks to.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The commitment level applied to every call.
    pub fn commitment(&self) -> Commitment {
        self.commitment
    }

    async fn call<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: P) -> Result<R> {
        let response = self
            .http
            .post(&self.url)
            .json(&Request {
                jsonrpc: "2.0",
                id: 1,
                method,
                params,
            })
            .send()
            .await
            .map_err(|e| Error::Rpc(format!("{method}: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Rpc(format!("{method}: HTTP {status}: {body}")));
        }

        let parsed: Response<R> = response
            .json()
            .await
            .map_err(|e| Error::Serialization(format!("{method} response: {e}")))?;

        if let Some(err) = parsed.error {
            return Err(Error::Rpc(format!(
                "{method}: {} (code {})",
                err.message, err.code
            )));
        }
        parsed
            .result
            .ok_or_else(|| Error::Rpc(format!("{method}: response had neither result nor error")))
    }

    fn commitment_param(&self) -> serde_json::Value {
        serde_json::json!({ "commitment": self.commitment.as_str() })
    }

    /// Latest blockhash at this client's commitment.
    pub async fn latest_blockhash(&self) -> Result<Hash> {
        let ctx: Ctx<BlockhashValue> = self
            .call(
                "getLatestBlockhash",
                serde_json::json!([self.commitment_param()]),
            )
            .await?;
        ctx.value
            .blockhash
            .parse()
            .map_err(|e| Error::Serialization(format!("blockhash parse: {e}")))
    }

    /// Whether a blockhash is still usable. A `false` here means a pending
    /// transaction built on it can never land.
    pub async fn is_blockhash_valid(&self, blockhash: &Hash) -> Result<bool> {
        let ctx: Ctx<bool> = self
            .call(
                "isBlockhashValid",
                serde_json::json!([blockhash.to_string(), self.commitment_param()]),
            )
            .await?;
        Ok(ctx.value)
    }

    /// Account balance in lamports.
    pub async fn balance(&self, pubkey: &Pubkey) -> Result<u64> {
        let ctx: Ctx<u64> = self
            .call(
                "getBalance",
                serde_json::json!([pubkey.to_string(), self.commitment_param()]),
            )
            .await?;
        Ok(ctx.value)
    }

    /// Network fee in lamports for a message, as the cluster prices it now.
    ///
    /// A `None` from the node means the blockhash is already gone; that is
    /// surfaced as an error so the caller fails closed rather than treating
    /// an unknown fee as zero.
    pub async fn fee_for_message(&self, message: &Message) -> Result<u64> {
        let encoded = BASE64.encode(message.serialize());
        let ctx: Ctx<Option<u64>> = self
            .call(
                "getFeeForMessage",
                serde_json::json!([encoded, self.commitment_param()]),
            )
            .await?;
        ctx.value
            .ok_or_else(|| Error::Rpc("fee unavailable: blockhash no longer valid".into()))
    }

    /// Simulate a signed transaction, with signature verification on.
    pub async fn simulate(&self, tx: &Transaction) -> Result<SimSummary> {
        let encoded = encode_tx(tx)?;
        let ctx: Ctx<SimValue> = self
            .call(
                "simulateTransaction",
                serde_json::json!([
                    encoded,
                    {
                        "encoding": "base64",
                        "sigVerify": true,
                        "commitment": self.commitment.as_str(),
                    }
                ]),
            )
            .await?;
        Ok(SimSummary {
            err: ctx.value.err.map(|e| e.to_string()),
            units_consumed: ctx.value.units_consumed,
            logs: ctx.value.logs.unwrap_or_default(),
        })
    }

    /// Submit a signed transaction. Preflight is skipped because the
    /// pipeline already simulated it at the gate stage.
    pub async fn send_transaction(&self, tx: &Transaction) -> Result<Signature> {
        let encoded = encode_tx(tx)?;
        let sig: String = self
            .call(
                "sendTransaction",
                serde_json::json!([
                    encoded,
                    {
                        "encoding": "base64",
                        "skipPreflight": true,
                        "preflightCommitment": self.commitment.as_str(),
                    }
                ]),
            )
            .await?;
        sig.parse()
            .map_err(|e| Error::Serialization(format!("signature parse: {e}")))
    }

    /// Confirmation status of one signature, if the cluster knows it yet.
    pub async fn signature_status(&self, signature: &Signature) -> Result<Option<SignatureStatus>> {
        let ctx: Ctx<Vec<Option<StatusValue>>> = self
            .call(
                "getSignatureStatuses",
                serde_json::json!([
                    [signature.to_string()],
                    { "searchTransactionHistory": false }
                ]),
            )
            .await?;
        Ok(ctx
            .value
            .into_iter()
            .next()
            .flatten()
            .map(|s| SignatureStatus {
                slot: s.slot,
                err: s.err.map(|e| e.to_string()),
                confirmation: s
                    .confirmation_status
                    .as_deref()
                    .and_then(Commitment::from_str),
            }))
    }

    /// Request an airdrop, for test validators and devnet. Returns the
    /// funding transaction's signature.
    pub async fn request_airdrop(&self, pubkey: &Pubkey, lamports: u64) -> Result<Signature> {
        let sig: String = self
            .call(
                "requestAirdrop",
                serde_json::json!([pubkey.to_string(), lamports, self.commitment_param()]),
            )
            .await?;
        sig.parse()
            .map_err(|e| Error::Serialization(format!("signature parse: {e}")))
    }
}

fn encode_tx(tx: &Transaction) -> Result<String> {
    let bytes = bincode::serialize(tx)
        .map_err(|e| Error::Serialization(format!("serialize transaction: {e}")))?;
    Ok(BASE64.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commitment_orders_weakest_to_strongest() {
        assert!(Commitment::Processed < Commitment::Confirmed);
        assert!(Commitment::Confirmed < Commitment::Finalized);
    }

    #[test]
    fn status_satisfies_at_or_above_requested() {
        let confirmed = SignatureStatus {
            slot: 1,
            err: None,
            confirmation: Some(Commitment::Confirmed),
        };
        assert!(confirmed.satisfies(Commitment::Processed));
        assert!(confirmed.satisfies(Commitment::Confirmed));
        assert!(!confirmed.satisfies(Commitment::Finalized));
    }

    #[test]
    fn status_without_confirmation_satisfies_nothing() {
        let unknown = SignatureStatus {
            slot: 1,
            err: None,
            confirmation: None,
        };
        assert!(!unknown.satisfies(Commitment::Processed));
    }

    #[test]
    fn commitment_round_trips_through_protocol_strings() {
        for c in [
            Commitment::Processed,
            Commitment::Confirmed,
            Commitment::Finalized,
        ] {
            assert_eq!(Commitment::from_str(c.as_str()), Some(c));
        }
        assert_eq!(Commitment::from_str("bogus"), None);
    }

    #[test]
    fn client_normalizes_trailing_slash() {
        let c = RpcClient::new("http://127.0.0.1:8899/", Commitment::Confirmed);
        assert_eq!(c.url(), "http://127.0.0.1:8899");
    }
}
