use crate::channel::ChannelError;
use alloy_primitives::B256;
use reth_evm::execute::BlockExecutionError;
use reth_provider::ProviderError;
use std::{convert::Infallible, fmt, time::SystemTimeError};

pub(crate) type PipeResult<T> = Result<T, PipeBlockError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PipeBlockContext {
    pub(crate) block_id: B256,
    pub(crate) block_number: u64,
    pub(crate) epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PipeStage {
    Execute,
    Merklize,
    Seal,
    Verify,
    Canonicalize,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "pipe block failed: id={block_id}, number={block_number}, epoch={epoch}, stage={stage:?}: {kind}"
)]
pub(crate) struct PipeBlockError {
    block_id: B256,
    block_number: u64,
    epoch: u64,
    stage: PipeStage,
    #[source]
    kind: PipeErrorKind,
}

impl PipeBlockError {
    pub(crate) fn new(
        context: PipeBlockContext,
        stage: PipeStage,
        kind: impl Into<PipeErrorKind>,
    ) -> Self {
        Self {
            block_id: context.block_id,
            block_number: context.block_number,
            epoch: context.epoch,
            stage,
            kind: kind.into(),
        }
    }

    pub(crate) const fn kind(&self) -> &PipeErrorKind {
        &self.kind
    }

    pub(crate) const fn is_cancellation(&self) -> bool {
        matches!(
            self.kind,
            PipeErrorKind::Channel(ChannelError::Closed) |
                PipeErrorKind::ChannelClosed(PipeChannel::ExecutionResult)
        )
    }
}

pub(crate) trait PipeResultExt<T> {
    fn pipe_context(self, context: PipeBlockContext, stage: PipeStage) -> PipeResult<T>;
}

impl<T, E> PipeResultExt<T> for Result<T, E>
where
    E: Into<PipeErrorKind>,
{
    fn pipe_context(self, context: PipeBlockContext, stage: PipeStage) -> PipeResult<T> {
        self.map_err(|err| PipeBlockError::new(context, stage, err))
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PipeErrorKind {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    BlockExecution(#[from] BlockExecutionError),
    #[error(transparent)]
    Channel(#[from] ChannelError),
    #[error("{0} channel closed")]
    ChannelClosed(PipeChannel),
    #[error(transparent)]
    Order(#[from] PipeOrderError),
    #[error("block hash mismatch: expected={expected}, actual={actual}")]
    HashMismatch { expected: B256, actual: B256 },
    #[error(transparent)]
    InvalidExecutionOutput(#[from] ExecutionOutputError),
}

impl From<Infallible> for PipeErrorKind {
    fn from(never: Infallible) -> Self {
        match never {}
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PipeChannel {
    ExecutionResult,
    CanonicalEvent,
    CanonicalAcknowledgement,
}

impl fmt::Display for PipeChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::ExecutionResult => "execution result",
            Self::CanonicalEvent => "canonical event",
            Self::CanonicalAcknowledgement => "canonical acknowledgement",
        };
        f.write_str(name)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum PipeOrderError {
    #[error("block number must be greater than zero")]
    ZeroBlockNumber,
    #[error("ordered epoch {ordered} does not match barrier epoch {barrier}")]
    EpochMismatch { ordered: u64, barrier: u64 },
    #[error("epoch changed from {actual}, expected {expected}")]
    EpochStateMismatch { actual: u64, expected: u64 },
    #[error("execute height was {actual}, expected {expected}")]
    ExecuteHeightMismatch { actual: u64, expected: u64 },
    #[error("block number {block} does not extend parent {parent}")]
    ParentNumberMismatch { block: u64, parent: u64 },
    #[error("transaction count {transactions} does not match sender count {senders}")]
    TransactionSenderCountMismatch { transactions: usize, senders: usize },
    #[error("transaction emitted epoch {emitted}, expected {expected}")]
    EpochTransitionMismatch { emitted: u64, expected: u64 },
    #[error("epoch cannot advance beyond {current}")]
    EpochOverflow { current: u64 },
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ExecutionOutputError {
    #[error("gas limit {gas_limit} is lower than gas used {gas_used}")]
    GasLimitExceeded { gas_limit: u64, gas_used: u64 },
    #[error("block gas used {block_gas_used} does not match last receipt {receipt_gas_used}")]
    ReceiptGasMismatch { block_gas_used: u64, receipt_gas_used: u64 },
    #[error("execution output gas used {output_gas_used} does not match block {block_gas_used}")]
    OutputGasMismatch { output_gas_used: u64, block_gas_used: u64 },
    #[error("system clock is before the Unix epoch: {0}")]
    Clock(#[from] SystemTimeError),
    #[error("block timestamp {timestamp} is implausibly ahead of current time {now}")]
    InvalidTimestamp { timestamp: u64, now: u64 },
    #[error("expected {expected} system receipts, got {actual}")]
    MissingSystemReceipts { expected: usize, actual: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_error_remains_matchable() {
        let context = PipeBlockContext { block_id: B256::ZERO, block_number: 7, epoch: 2 };
        let error: PipeBlockError = Err::<(), _>(ProviderError::BestBlockNotFound)
            .pipe_context(context, PipeStage::Execute)
            .unwrap_err();

        assert!(matches!(
            error.kind(),
            PipeErrorKind::Provider(source)
                if matches!(source, ProviderError::BestBlockNotFound)
        ));
    }

    #[test]
    fn only_owner_channel_closure_is_cancellation() {
        let context = PipeBlockContext { block_id: B256::ZERO, block_number: 7, epoch: 2 };

        let cancelled = PipeBlockError::new(context, PipeStage::Verify, ChannelError::Closed);
        assert!(cancelled.is_cancellation());

        let canonical_failure = PipeBlockError::new(
            context,
            PipeStage::Canonicalize,
            PipeErrorKind::ChannelClosed(PipeChannel::CanonicalAcknowledgement),
        );
        assert!(!canonical_failure.is_cancellation());
    }
}
