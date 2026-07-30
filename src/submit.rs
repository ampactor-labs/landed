//! Submission routes: plain RPC or a Jito bundle.
//!
//! The Jito client is a clean port of the one in flowpilot (the author's
//! retired trading engine): sendBundle / getBundleStatuses over the block
//! engine's JSON-RPC, the eight official tip accounts, and strict local
//! validation before anything leaves the machine.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;
use solana_system_interface::instruction as system_instruction;
use tracing::{debug, info};

use crate::error::{Error, Result};

/// How a transaction reaches the cluster.
#[derive(Debug, Clone)]
pub enum Route {
    /// `sendTransaction` straight to an RPC node.
    Rpc,
    /// A single-transaction Jito bundle with a tip appended to the
    /// transaction's instruction list.
    JitoBundle {
        /// Tip in lamports, transferred to one of the official tip accounts.
        tip_lamports: u64,
    },
}

/// All 8 official Jito tip accounts.
/// See <https://docs.jito.wtf/lowlatencytxnsend/#tip-accounts>
pub const TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4bPg2DU3cyGJEMa7njiVFJn",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSLECiCpW5R8nFYasck",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

/// Build a tip transfer to one of the official Jito tip accounts.
///
/// Rotation is a cheap timestamp hash — load spreading, not security.
pub fn tip_instruction(payer: &Pubkey, tip_lamports: u64) -> Instruction {
    let idx = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize)
        % TIP_ACCOUNTS.len();
    let tip_account: Pubkey = TIP_ACCOUNTS[idx].parse().expect("valid tip account pubkey");
    system_instruction::transfer(payer, &tip_account, tip_lamports)
}

/// Status of a submitted Jito bundle.
#[derive(Debug, Clone, Deserialize)]
pub struct BundleStatus {
    /// Unique bundle identifier.
    pub bundle_id: String,
    /// Current status: "Pending", "Landed", "Failed", "Invalid".
    pub status: String,
    /// Slot the bundle landed in, when it did.
    pub landed_slot: Option<u64>,
}

#[derive(Debug, Serialize)]
struct RpcRequest<P: Serialize> {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: P,
}

#[derive(Debug, Deserialize)]
struct RpcResponse<R> {
    result: Option<R>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct StatusesResult {
    value: Vec<BundleStatus>,
}

/// Minimal Jito block engine client: submit bundles, poll their status.
pub struct JitoClient {
    http: Client,
    base: String,
}

impl JitoClient {
    /// Create a client for a block engine URL, e.g.
    /// `https://mainnet.block-engine.jito.wtf`.
    pub fn new(block_engine_url: &str) -> Self {
        Self {
            http: Client::new(),
            base: block_engine_url.trim_end_matches('/').to_string(),
        }
    }

    /// Encode signed transactions as a bundle, enforcing the block engine's
    /// hard limits locally: 1–5 transactions, every one fully signed.
    pub fn build_bundle(transactions: &[Transaction]) -> Result<Vec<String>> {
        if transactions.is_empty() {
            return Err(Error::BundleRejected(
                "bundle must contain at least one transaction".into(),
            ));
        }
        if transactions.len() > 5 {
            return Err(Error::BundleRejected(
                "bundle cannot contain more than 5 transactions".into(),
            ));
        }
        for (i, tx) in transactions.iter().enumerate() {
            let unsigned = tx
                .signatures
                .first()
                .map(|s| s == &Signature::default())
                .unwrap_or(true);
            if unsigned {
                return Err(Error::BundleRejected(format!(
                    "transaction at index {i} is unsigned"
                )));
            }
        }
        transactions
            .iter()
            .map(|tx| {
                let bytes = bincode::serialize(tx)
                    .map_err(|e| Error::Serialization(format!("serialize transaction: {e}")))?;
                Ok(bs58::encode(bytes).into_string())
            })
            .collect()
    }

    /// Submit an encoded bundle. Returns the bundle id.
    pub async fn send_bundle(&self, encoded_transactions: Vec<String>) -> Result<String> {
        let request = RpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "sendBundle",
            params: vec![encoded_transactions],
        };
        let url = format!("{}/api/v1/bundles", self.base);
        debug!(url = %url, "submitting bundle");

        let response = self
            .http
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::Http(format!("bundle submission: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::BundleRejected(format!("HTTP {status}: {body}")));
        }

        let parsed: RpcResponse<String> = response
            .json()
            .await
            .map_err(|e| Error::Serialization(format!("parse bundle response: {e}")))?;

        if let Some(err) = parsed.error {
            return Err(Error::BundleRejected(err.message));
        }
        let bundle_id = parsed
            .result
            .ok_or_else(|| Error::BundleRejected("no bundle id in response".into()))?;
        info!(bundle_id = %bundle_id, "bundle submitted");
        Ok(bundle_id)
    }

    /// Fetch statuses for up to five bundle ids. Bundles the engine does not
    /// know yet are omitted from the result.
    pub async fn bundle_statuses(&self, bundle_ids: &[String]) -> Result<Vec<BundleStatus>> {
        let request = RpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "getBundleStatuses",
            params: vec![bundle_ids.to_vec()],
        };
        let url = format!("{}/api/v1/bundles", self.base);

        let response = self
            .http
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::Http(format!("bundle status: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Http(format!("bundle status HTTP {status}: {body}")));
        }

        let parsed: RpcResponse<StatusesResult> = response
            .json()
            .await
            .map_err(|e| Error::Serialization(format!("parse bundle statuses: {e}")))?;

        if let Some(err) = parsed.error {
            return Err(Error::Http(format!("bundle status error: {}", err.message)));
        }
        Ok(parsed.result.map(|r| r.value).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::hash::Hash;
    use solana_sdk::message::Message;
    use solana_sdk::signature::{Keypair, Signer};

    fn signed_transfer(payer: &Keypair, n: u64) -> Transaction {
        let ix = system_instruction::transfer(&payer.pubkey(), &payer.pubkey(), n);
        let msg = Message::new(&[ix], Some(&payer.pubkey()));
        let mut tx = Transaction::new_unsigned(msg);
        tx.sign(&[payer], Hash::new_unique());
        tx
    }

    #[test]
    fn build_bundle_rejects_empty() {
        assert!(JitoClient::build_bundle(&[]).is_err());
    }

    #[test]
    fn build_bundle_rejects_more_than_five() {
        let kp = Keypair::new();
        let txs: Vec<Transaction> = (0..6).map(|i| signed_transfer(&kp, i + 1)).collect();
        assert!(JitoClient::build_bundle(&txs).is_err());
    }

    #[test]
    fn build_bundle_rejects_unsigned() {
        let kp = Keypair::new();
        let ix = system_instruction::transfer(&kp.pubkey(), &kp.pubkey(), 1);
        let msg = Message::new(&[ix], Some(&kp.pubkey()));
        let tx = Transaction::new_unsigned(msg);
        let err = JitoClient::build_bundle(&[tx]).unwrap_err();
        assert!(err.to_string().contains("unsigned"));
    }

    #[test]
    fn build_bundle_encodes_base58() {
        let kp = Keypair::new();
        let bundle = JitoClient::build_bundle(&[signed_transfer(&kp, 1)]).unwrap();
        assert_eq!(bundle.len(), 1);
        assert!(bundle[0].chars().all(|c| c.is_alphanumeric()));
    }

    #[test]
    fn tip_instruction_targets_official_account() {
        let known: Vec<Pubkey> = TIP_ACCOUNTS.iter().map(|s| s.parse().unwrap()).collect();
        let payer = Pubkey::new_unique();
        for _ in 0..20 {
            let ix = tip_instruction(&payer, 1_000);
            assert_eq!(ix.program_id, solana_system_interface::program::id());
            assert!(known.contains(&ix.accounts[1].pubkey));
            assert_eq!(ix.accounts[0].pubkey, payer);
        }
    }

    #[test]
    fn client_normalizes_trailing_slash() {
        let c = JitoClient::new("https://example.com/");
        assert_eq!(c.base, "https://example.com");
    }
}
