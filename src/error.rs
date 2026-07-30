use crate::gate::Rejection;

/// Errors produced by the pipeline.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A gate rejected the transaction before submission. This is the
    /// fail-closed path working as designed, not an infrastructure fault.
    #[error("rejected by gate `{}`: {}", .0.gate, .0.reason)]
    Rejected(Rejection),

    /// An RPC call failed.
    #[error("rpc: {0}")]
    Rpc(String),

    /// An HTTP call to the block engine failed.
    #[error("http: {0}")]
    Http(String),

    /// Encoding or decoding failed.
    #[error("serialization: {0}")]
    Serialization(String),

    /// The block engine refused a bundle before it entered an auction.
    #[error("bundle rejected: {0}")]
    BundleRejected(String),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;
