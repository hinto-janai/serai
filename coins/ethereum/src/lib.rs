use thiserror::Error;

pub use alloy_core;
pub use alloy_consensus;

pub use alloy_rpc_types;
pub use alloy_simple_request_transport;
pub use alloy_rpc_client;
pub use alloy_provider;

pub mod crypto;

pub(crate) mod abi;

pub mod erc20;
pub mod deployer;
pub mod router;

pub mod machine;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Error)]
pub enum Error {
  #[error("failed to verify Schnorr signature")]
  InvalidSignature,
  #[error("couldn't make call/send TX")]
  ConnectionError,
}
