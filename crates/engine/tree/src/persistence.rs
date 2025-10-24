use crate::metrics::PersistenceMetrics;
use alloy_consensus::BlockHeader;
use alloy_eips::BlockNumHash;
use reth_chain_state::ExecutedBlockWithTrieUpdates;
use reth_errors::ProviderError;
use reth_ethereum_primitives::EthPrimitives;
use reth_primitives_traits::NodePrimitives;
use reth_provider::{
    providers::ProviderNodeTypes, writer::UnifiedStorageWriter, BlockHashReader,
    ChainStateBlockWriter, DatabaseProviderFactory, ProviderFactory, StaticFileProviderFactory,
    PERSIST_BLOCK_CACHE,
};
use reth_prune::{PrunerError, PrunerOutput, PrunerWithFactory};
use reth_stages_api::{MetricEvent, MetricEventsSender};
use std::{
    sync::mpsc::{Receiver, SendError, Sender},
    time::Instant,
};
use thiserror::Error;
use tokio::sync::oneshot;
use tracing::{debug, error};

/// 打印性能统计摘要
fn print_perf_stats(stats: &reth_db::PerfStats) {
    if stats.upsert_count == 0 {
        eprintln!("  [WARN] No upsert calls detected!");
        return;
    }
    
    // 计算总耗时
    let total_ns = stats.get_cf_handle_ns 
        + stats.key_encode_ns 
        + stats.compress_value_ns 
        + stats.encode_dupsort_key_ns 
        + stats.batch_put_cf_ns
        + stats.db_put_cf_ns;
    let total_ms = total_ns as f64 / 1_000_000.0;
    
    // 准备数据（名称，耗时ns，调用次数）
    let mut items = vec![
        ("get_cf_handle", stats.get_cf_handle_ns, stats.upsert_count),
        ("key_encode", stats.key_encode_ns, stats.upsert_count),
        ("compress_value", stats.compress_value_ns, stats.upsert_count),
        ("encode_dupsort_key", stats.encode_dupsort_key_ns, stats.upsert_count),
        ("batch_put_cf", stats.batch_put_cf_ns, stats.upsert_count),
        ("db_put_cf", stats.db_put_cf_ns, stats.upsert_count),
    ];
    
    // 按耗时降序排序
    items.sort_by(|a, b| b.1.cmp(&a.1));
    
    eprintln!("  {:<20} | {:>6} | {:>10} | {:>8} | {:>10}",
        "Operation", "Count", "Total(ms)", "Avg(us)", "of_total%");
    eprintln!("  {}", "-".repeat(70));
    
    for (name, duration_ns, count) in items {       
        let total_ms = duration_ns as f64 / 1_000_000.0;
        let avg_us = duration_ns as f64 / count as f64 / 1000.0;
        let percentage = (duration_ns as f64 / total_ns as f64) * 100.0;
        
        let percentage_str = if percentage > 50.0 {
            format!("{:>5.1}% [HOT]", percentage)
        } else if percentage > 25.0 {
            format!("{:>5.1}% [WARN]", percentage)
        } else {
            format!("{:>5.1}%", percentage)
        };
        
        eprintln!("  {:<20} | {:>6} | {:>10.3} | {:>8.2} | {}",
            name, count, total_ms, avg_us, percentage_str);
    }
    
    eprintln!("  {}", "-".repeat(70));
    eprintln!("  {:<20} | {:>6} | {:>10.3} | {:>8.2} |",
        "TOTAL", stats.upsert_count, total_ms, total_ns as f64 / stats.upsert_count as f64 / 1000.0);
    
    // 打印 key/value 平均长度
    let avg_key_bytes = stats.total_key_bytes as f64 / stats.upsert_count as f64;
    let avg_value_bytes = stats.total_value_bytes as f64 / stats.upsert_count as f64;
    let total_data_mb = (stats.total_key_bytes + stats.total_value_bytes) as f64 / 1024.0 / 1024.0;
    
    eprintln!("\n  Data Size Stats:");
    eprintln!("    Avg Key Size:    {:>8.2} bytes", avg_key_bytes);
    eprintln!("    Avg Value Size:  {:>8.2} bytes", avg_value_bytes);
    eprintln!("    Total Data:      {:>8.2} MB ({} KB keys + {} KB values)",
        total_data_mb,
        stats.total_key_bytes / 1024,
        stats.total_value_bytes / 1024);
}

/// Writes parts of reth's in memory tree state to the database and static files.
///
/// This is meant to be a spawned service that listens for various incoming persistence operations,
/// performing those actions on disk, and returning the result in a channel.
///
/// This should be spawned in its own thread with [`std::thread::spawn`], since this performs
/// blocking I/O operations in an endless loop.
#[derive(Debug)]
pub struct PersistenceService<N>
where
    N: ProviderNodeTypes,
{
    /// The provider factory to use
    provider: ProviderFactory<N>,
    /// Incoming requests
    incoming: Receiver<PersistenceAction<N::Primitives>>,
    /// The pruner
    pruner: PrunerWithFactory<ProviderFactory<N>>,
    /// metrics
    metrics: PersistenceMetrics,
    /// Sender for sync metrics - we only submit sync metrics for persisted blocks
    sync_metrics_tx: MetricEventsSender,
}

impl<N> PersistenceService<N>
where
    N: ProviderNodeTypes,
{
    /// Create a new persistence service
    pub fn new(
        provider: ProviderFactory<N>,
        incoming: Receiver<PersistenceAction<N::Primitives>>,
        pruner: PrunerWithFactory<ProviderFactory<N>>,
        sync_metrics_tx: MetricEventsSender,
    ) -> Self {
        Self { provider, incoming, pruner, metrics: PersistenceMetrics::default(), sync_metrics_tx }
    }

    /// Prunes block data before the given block hash according to the configured prune
    /// configuration.
    fn prune_before(&mut self, block_num: u64) -> Result<PrunerOutput, PrunerError> {
        debug!(target: "engine::persistence", ?block_num, "Running pruner");
        let start_time = Instant::now();
        // TODO: doing this properly depends on pruner segment changes
        let result = self.pruner.run(block_num);
        self.metrics.prune_before_duration_seconds.record(start_time.elapsed());
        result
    }
}

impl<N> PersistenceService<N>
where
    N: ProviderNodeTypes,
{
    /// This is the main loop, that will listen to database events and perform the requested
    /// database actions
    pub fn run(mut self) -> Result<(), PersistenceError> {
        // If the receiver errors then senders have disconnected, so the loop should then end.
        while let Ok(action) = self.incoming.recv() {
            match action {
                PersistenceAction::RemoveBlocksAbove(new_tip_num, sender) => {
                    let result = self.on_remove_blocks_above(new_tip_num)?;
                    // send new sync metrics based on removed blocks
                    let _ =
                        self.sync_metrics_tx.send(MetricEvent::SyncHeight { height: new_tip_num });
                    // we ignore the error because the caller may or may not care about the result
                    let _ = sender.send(result);
                }
                PersistenceAction::SaveBlocks(blocks, sender) => {
                    let result = self.on_save_blocks(blocks)?;
                    let result_number = result.map(|r| r.number);

                    // we ignore the error because the caller may or may not care about the result
                    let _ = sender.send(result);

                    if let Some(block_number) = result_number {
                        // send new sync metrics based on saved blocks
                        let _ = self
                            .sync_metrics_tx
                            .send(MetricEvent::SyncHeight { height: block_number });

                        if self.pruner.is_pruning_needed(block_number) {
                            // We log `PrunerOutput` inside the `Pruner`
                            let _ = self.prune_before(block_number)?;
                        }
                    }
                }
                PersistenceAction::SaveFinalizedBlock(finalized_block) => {
                    let provider = self.provider.database_provider_rw()?;
                    provider.save_finalized_block_number(finalized_block)?;
                    provider.commit()?;
                }
                PersistenceAction::SaveSafeBlock(safe_block) => {
                    let provider = self.provider.database_provider_rw()?;
                    provider.save_safe_block_number(safe_block)?;
                    provider.commit()?;
                }
            }
        }
        Ok(())
    }

    fn on_remove_blocks_above(
        &self,
        new_tip_num: u64,
    ) -> Result<Option<BlockNumHash>, PersistenceError> {
        debug!(target: "engine::persistence", ?new_tip_num, "Removing blocks");
        let start_time = Instant::now();
        let provider_rw = self.provider.database_provider_rw()?;
        let sf_provider = self.provider.static_file_provider();

        let new_tip_hash = provider_rw.block_hash(new_tip_num)?;
        UnifiedStorageWriter::from(&provider_rw, &sf_provider).remove_blocks_above(new_tip_num)?;
        UnifiedStorageWriter::commit_unwind(provider_rw)?;

        debug!(target: "engine::persistence", ?new_tip_num, ?new_tip_hash, "Removed blocks from disk");
        self.metrics.remove_blocks_above_duration_seconds.record(start_time.elapsed());
        Ok(new_tip_hash.map(|hash| BlockNumHash { hash, number: new_tip_num }))
    }

    fn on_save_blocks(
        &self,
        blocks: Vec<ExecutedBlockWithTrieUpdates<N::Primitives>>,
    ) -> Result<Option<BlockNumHash>, PersistenceError> {
        debug!(target: "engine::persistence", first=?blocks.first().map(|b| b.recovered_block.num_hash()), last=?blocks.last().map(|b| b.recovered_block.num_hash()), "Saving range of blocks");
        let start_time = Instant::now();
        let last_block_hash_num = blocks.last().map(|block| BlockNumHash {
            hash: block.recovered_block().hash(),
            number: block.recovered_block().header().number(),
        });

        let num_blocks = blocks.len();
        if let Some(last_block_hn) = &last_block_hash_num {
            let provider_rw = self.provider.database_provider_batch()?;
            let static_file_provider = self.provider.static_file_provider();

            // 开始追踪这次 save_blocks 调用
            // 重置性能统计
            reth_db::PerfStats::reset();
            
            UnifiedStorageWriter::from(&provider_rw, &static_file_provider).save_blocks(blocks)?;
            
            // save_blocks 完成，获取性能统计
            let perf_stats = reth_db::PerfStats::get();
            
            if perf_stats.upsert_count > 0 {
                eprintln!("\n=== save_blocks trace (block={}, count={}) ===", 
                    last_block_hn.number, num_blocks);
                print_perf_stats(&perf_stats);
                eprintln!("=====================================\n");
            }
            
            let start_time = Instant::now();
            UnifiedStorageWriter::commit(provider_rw)?;
            self.metrics
                .persist_commit_duration_seconds
                .record(start_time.elapsed().as_secs_f64() / num_blocks as f64);
            PERSIST_BLOCK_CACHE.persist_tip(last_block_hn.number);
        }
        let elapsed = start_time.elapsed();
        self.metrics.save_blocks_duration_seconds.record(elapsed);
        self.metrics
            .save_duration_per_block_seconds
            .record(elapsed.as_secs_f64() / num_blocks as f64);
        Ok(last_block_hash_num)
    }
}

/// One of the errors that can happen when using the persistence service.
#[derive(Debug, Error)]
pub enum PersistenceError {
    /// A pruner error
    #[error(transparent)]
    PrunerError(#[from] PrunerError),

    /// A provider error
    #[error(transparent)]
    ProviderError(#[from] ProviderError),
}

/// A signal to the persistence service that part of the tree state can be persisted.
#[derive(Debug)]
pub enum PersistenceAction<N: NodePrimitives = EthPrimitives> {
    /// The section of tree state that should be persisted. These blocks are expected in order of
    /// increasing block number.
    ///
    /// First, header, transaction, and receipt-related data should be written to static files.
    /// Then the execution history-related data will be written to the database.
    SaveBlocks(Vec<ExecutedBlockWithTrieUpdates<N>>, oneshot::Sender<Option<BlockNumHash>>),

    /// Removes block data above the given block number from the database.
    ///
    /// This will first update checkpoints from the database, then remove actual block data from
    /// static files.
    RemoveBlocksAbove(u64, oneshot::Sender<Option<BlockNumHash>>),

    /// Update the persisted finalized block on disk
    SaveFinalizedBlock(u64),

    /// Update the persisted safe block on disk
    SaveSafeBlock(u64),
}

/// A handle to the persistence service
#[derive(Debug, Clone)]
pub struct PersistenceHandle<N: NodePrimitives = EthPrimitives> {
    /// The channel used to communicate with the persistence service
    sender: Sender<PersistenceAction<N>>,
}

impl<T: NodePrimitives> PersistenceHandle<T> {
    /// Create a new [`PersistenceHandle`] from a [`Sender<PersistenceAction>`].
    pub const fn new(sender: Sender<PersistenceAction<T>>) -> Self {
        Self { sender }
    }

    /// Create a new [`PersistenceHandle`], and spawn the persistence service.
    pub fn spawn_service<N>(
        provider_factory: ProviderFactory<N>,
        pruner: PrunerWithFactory<ProviderFactory<N>>,
        sync_metrics_tx: MetricEventsSender,
    ) -> PersistenceHandle<N::Primitives>
    where
        N: ProviderNodeTypes,
    {
        // create the initial channels
        let (db_service_tx, db_service_rx) = std::sync::mpsc::channel();

        // construct persistence handle
        let persistence_handle = PersistenceHandle::new(db_service_tx);

        // spawn the persistence service
        let db_service =
            PersistenceService::new(provider_factory, db_service_rx, pruner, sync_metrics_tx);
        std::thread::Builder::new()
            .name("Persistence Service".to_string())
            .spawn(|| {
                if let Err(err) = db_service.run() {
                    error!(target: "engine::persistence", ?err, "Persistence service failed");
                }
            })
            .unwrap();

        persistence_handle
    }

    /// Sends a specific [`PersistenceAction`] in the contained channel. The caller is responsible
    /// for creating any channels for the given action.
    pub fn send_action(
        &self,
        action: PersistenceAction<T>,
    ) -> Result<(), SendError<PersistenceAction<T>>> {
        self.sender.send(action)
    }

    /// Tells the persistence service to save a certain list of finalized blocks. The blocks are
    /// assumed to be ordered by block number.
    ///
    /// This returns the latest hash that has been saved, allowing removal of that block and any
    /// previous blocks from in-memory data structures. This value is returned in the receiver end
    /// of the sender argument.
    ///
    /// If there are no blocks to persist, then `None` is sent in the sender.
    pub fn save_blocks(
        &self,
        blocks: Vec<ExecutedBlockWithTrieUpdates<T>>,
        tx: oneshot::Sender<Option<BlockNumHash>>,
    ) -> Result<(), SendError<PersistenceAction<T>>> {
        self.send_action(PersistenceAction::SaveBlocks(blocks, tx))
    }

    /// Persists the finalized block number on disk.
    pub fn save_finalized_block_number(
        &self,
        finalized_block: u64,
    ) -> Result<(), SendError<PersistenceAction<T>>> {
        self.send_action(PersistenceAction::SaveFinalizedBlock(finalized_block))
    }

    /// Persists the finalized block number on disk.
    pub fn save_safe_block_number(
        &self,
        safe_block: u64,
    ) -> Result<(), SendError<PersistenceAction<T>>> {
        self.send_action(PersistenceAction::SaveSafeBlock(safe_block))
    }

    /// Tells the persistence service to remove blocks above a certain block number. The removed
    /// blocks are returned by the service.
    ///
    /// When the operation completes, the new tip hash is returned in the receiver end of the sender
    /// argument.
    pub fn remove_blocks_above(
        &self,
        block_num: u64,
        tx: oneshot::Sender<Option<BlockNumHash>>,
    ) -> Result<(), SendError<PersistenceAction<T>>> {
        self.send_action(PersistenceAction::RemoveBlocksAbove(block_num, tx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use reth_chain_state::test_utils::TestBlockBuilder;
    use reth_exex_types::FinishedExExHeight;
    use reth_provider::test_utils::create_test_provider_factory;
    use reth_prune::Pruner;
    use tokio::sync::mpsc::unbounded_channel;

    fn default_persistence_handle() -> PersistenceHandle<EthPrimitives> {
        let provider = create_test_provider_factory();

        let (_finished_exex_height_tx, finished_exex_height_rx) =
            tokio::sync::watch::channel(FinishedExExHeight::NoExExs);

        let pruner =
            Pruner::new_with_factory(provider.clone(), vec![], 5, 0, None, finished_exex_height_rx);

        let (sync_metrics_tx, _sync_metrics_rx) = unbounded_channel();
        PersistenceHandle::<EthPrimitives>::spawn_service(provider, pruner, sync_metrics_tx)
    }

    #[tokio::test]
    async fn test_save_blocks_empty() {
        reth_tracing::init_test_tracing();
        let persistence_handle = default_persistence_handle();

        let blocks = vec![];
        let (tx, rx) = oneshot::channel();

        persistence_handle.save_blocks(blocks, tx).unwrap();

        let hash = rx.await.unwrap();
        assert_eq!(hash, None);
    }

    #[tokio::test]
    async fn test_save_blocks_single_block() {
        reth_tracing::init_test_tracing();
        let persistence_handle = default_persistence_handle();
        let block_number = 0;
        let mut test_block_builder = TestBlockBuilder::eth();
        let executed =
            test_block_builder.get_executed_block_with_number(block_number, B256::random());
        let block_hash = executed.recovered_block().hash();

        let blocks = vec![executed];
        let (tx, rx) = oneshot::channel();

        persistence_handle.save_blocks(blocks, tx).unwrap();

        let BlockNumHash { hash: actual_hash, number: _ } =
            tokio::time::timeout(std::time::Duration::from_secs(10), rx)
                .await
                .expect("test timed out")
                .expect("channel closed unexpectedly")
                .expect("no hash returned");

        assert_eq!(block_hash, actual_hash);
    }

    #[tokio::test]
    async fn test_save_blocks_multiple_blocks() {
        reth_tracing::init_test_tracing();
        let persistence_handle = default_persistence_handle();

        let mut test_block_builder = TestBlockBuilder::eth();
        let blocks = test_block_builder.get_executed_blocks(0..5).collect::<Vec<_>>();
        let last_hash = blocks.last().unwrap().recovered_block().hash();
        let (tx, rx) = oneshot::channel();

        persistence_handle.save_blocks(blocks, tx).unwrap();
        let BlockNumHash { hash: actual_hash, number: _ } = rx.await.unwrap().unwrap();
        assert_eq!(last_hash, actual_hash);
    }

    #[tokio::test]
    async fn test_save_blocks_multiple_calls() {
        reth_tracing::init_test_tracing();
        let persistence_handle = default_persistence_handle();

        let ranges = [0..1, 1..2, 2..4, 4..5];
        let mut test_block_builder = TestBlockBuilder::eth();
        for range in ranges {
            let blocks = test_block_builder.get_executed_blocks(range).collect::<Vec<_>>();
            let last_hash = blocks.last().unwrap().recovered_block().hash();
            let (tx, rx) = oneshot::channel();

            persistence_handle.save_blocks(blocks, tx).unwrap();

            let BlockNumHash { hash: actual_hash, number: _ } = rx.await.unwrap().unwrap();
            assert_eq!(last_hash, actual_hash);
        }
    }
}
