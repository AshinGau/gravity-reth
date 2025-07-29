//! This crate defines abstractions to create and update payloads (blocks)

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

<<<<<<<< HEAD:crates/rpc/rpc-convert/src/lib.rs
mod fees;
mod rpc;
pub mod transaction;

pub use fees::{CallFees, CallFeesError};
pub use rpc::*;
pub use transaction::{
    EthTxEnvError, IntoRpcTx, RpcConvert, RpcConverter, TransactionConversionError, TryIntoSimTx,
    TxInfoMapper,
};

#[cfg(feature = "op")]
pub use transaction::op::*;
========
mod events;
pub use crate::events::{Events, PayloadEvents};

pub use reth_payload_primitives::PayloadBuilderError;
>>>>>>>> v1.6.0:crates/payload/builder-primitives/src/lib.rs
