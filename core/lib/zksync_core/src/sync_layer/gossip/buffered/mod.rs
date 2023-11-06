//! Buffered [`BlockStore`] implementation.

use async_trait::async_trait;

use std::{collections::BTreeMap, ops, time::Instant};

#[cfg(test)]
use zksync_concurrency::ctx::channel;
use zksync_concurrency::{
    ctx,
    sync::{self, watch, Mutex},
};
use zksync_consensus_roles::validator::{BlockNumber, FinalBlock};
use zksync_consensus_storage::{BlockStore, StorageError, StorageResult, WriteBlockStore};

#[cfg(test)]
mod tests;

use super::{
    metrics::{BlockResponseKind, METRICS},
    utils::MissingBlockNumbers,
};

/// [`BlockStore`] variation that upholds additional invariants as to how blocks are processed.
///
/// The invariants are as follows:
///
/// - Stored blocks always have contiguous numbers; there are no gaps.
/// - Blocks can be scheduled to be added using [`Self::schedule_next_block()`] only. New blocks do not
///   appear in the store otherwise.
#[async_trait]
pub(super) trait ContiguousBlockStore: BlockStore {
    /// Schedules a block to be added to the store. Unlike [`WriteBlockStore::put_block()`],
    /// there is no expectation that the block is added to the store *immediately*. It's
    /// expected that it will be added to the store eventually, which will be signaled via
    /// a subscriber returned from [`BlockStore::subscribe_to_block_writes()`].
    ///
    /// [`Buffered`] guarantees that this method will only ever be called:
    ///
    /// - with the next block (i.e., one immediately after [`BlockStore::head_block()`])
    /// - sequentially (i.e., multiple blocks cannot be scheduled at once)
    async fn schedule_next_block(&self, ctx: &ctx::Ctx, block: &FinalBlock) -> StorageResult<()>;
}

#[derive(Debug)]
struct BlockBuffer {
    store_block_number: BlockNumber,
    is_block_scheduled: bool, // FIXME: remove in favor of "last / next scheduled block"
    blocks: BTreeMap<BlockNumber, FinalBlock>,
}

impl BlockBuffer {
    fn new(store_block_number: BlockNumber) -> Self {
        Self {
            store_block_number,
            is_block_scheduled: false,
            blocks: BTreeMap::new(),
        }
    }

    fn head_block(&self) -> Option<FinalBlock> {
        self.blocks.values().next_back().cloned()
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn set_store_block(&mut self, store_block_number: BlockNumber) {
        assert_eq!(
            store_block_number,
            self.store_block_number.next(),
            "`ContiguousBlockStore` invariant broken: unexpected new head block number"
        );
        assert!(
            self.is_block_scheduled,
            "`ContiguousBlockStore` invariant broken: unexpected update"
        );

        self.store_block_number = store_block_number;
        self.is_block_scheduled = false;
        let old_len = self.blocks.len();
        self.blocks = self.blocks.split_off(&store_block_number.next());
        // ^ Removes all entries up to and including `store_block_number`
        tracing::debug!("Removed {} blocks from buffer", old_len - self.blocks.len());
        METRICS.buffer_size.set(self.blocks.len());
    }

    fn last_contiguous_block_number(&self) -> BlockNumber {
        // By design, blocks in the underlying store are always contiguous.
        let mut last_number = self.store_block_number;
        for &number in self.blocks.keys() {
            if number > last_number.next() {
                return last_number;
            }
            last_number = number;
        }
        last_number
    }

    fn missing_block_numbers(&self, mut range: ops::Range<BlockNumber>) -> Vec<BlockNumber> {
        // Clamp the range start so we don't produce extra missing blocks.
        range.start = range.start.max(self.store_block_number.next());
        if range.is_empty() {
            return vec![]; // Return early to not trigger panic in `BTreeMap::range()`
        }

        let keys = self.blocks.range(range.clone()).map(|(&num, _)| num);
        MissingBlockNumbers::new(range, keys).collect()
    }

    fn put_block(&mut self, block: FinalBlock) {
        let block_number = block.header.number;
        assert!(block_number > self.store_block_number);
        // ^ Must be checked previously
        self.blocks.insert(block_number, block);
        tracing::debug!(%block_number, "Inserted block in buffer");
        METRICS.buffer_size.set(self.blocks.len());
    }

    fn next_block_for_store(&mut self) -> Option<FinalBlock> {
        if self.is_block_scheduled {
            None
        } else {
            let next_block = self.blocks.get(&self.store_block_number.next()).cloned();
            self.is_block_scheduled = next_block.is_some();
            next_block
        }
    }
}

/// Events emitted by [`Buffered`] storage.
#[cfg(test)]
#[derive(Debug)]
pub(super) enum BufferedStorageEvent {
    /// Update was received from the underlying storage.
    UpdateReceived(BlockNumber),
}

/// [`BlockStore`] with an in-memory buffer for pending blocks.
#[derive(Debug)]
pub(super) struct Buffered<T> {
    inner: T,
    inner_subscriber: watch::Receiver<BlockNumber>,
    block_writes_sender: watch::Sender<BlockNumber>,
    buffer: Mutex<BlockBuffer>,
    #[cfg(test)]
    events_sender: channel::UnboundedSender<BufferedStorageEvent>,
}

impl<T: ContiguousBlockStore> Buffered<T> {
    /// Creates a new buffered storage. The buffer is initially empty.
    pub fn new(store: T) -> Self {
        let inner_subscriber = store.subscribe_to_block_writes();
        let store_block_number = *inner_subscriber.borrow();
        tracing::debug!(
            store_block_number = store_block_number.0,
            "Initialized buffer storage"
        );
        Self {
            inner: store,
            inner_subscriber,
            block_writes_sender: watch::channel(store_block_number).0,
            buffer: Mutex::new(BlockBuffer::new(store_block_number)),
            #[cfg(test)]
            events_sender: channel::unbounded().0,
        }
    }

    #[cfg(test)]
    fn set_events_sender(&mut self, sender: channel::UnboundedSender<BufferedStorageEvent>) {
        self.events_sender = sender;
    }

    pub(super) fn inner(&self) -> &T {
        &self.inner
    }

    #[cfg(test)]
    async fn buffer_len(&self) -> usize {
        self.buffer.lock().await.blocks.len()
    }

    /// Listens to the updates in the underlying storage. This method must be spawned as a background task
    /// which should be running as long at the [`Buffered`] is in use. Otherwise,
    /// `BufferedStorage` will function incorrectly.
    #[tracing::instrument(level = "trace", skip_all, err)]
    pub async fn listen_to_updates(&self, ctx: &ctx::Ctx) -> StorageResult<()> {
        let mut subscriber = self.inner_subscriber.clone();
        loop {
            let store_block_number = *sync::changed(ctx, &mut subscriber).await?;
            tracing::debug!(
                store_block_number = store_block_number.0,
                "Underlying block number updated"
            );

            let next_block_for_store = {
                let mut buffer = sync::lock(ctx, &self.buffer).await?;
                buffer.set_store_block(store_block_number);
                buffer.next_block_for_store()
            };
            if let Some(block) = next_block_for_store {
                self.inner.schedule_next_block(ctx, &block).await?;
                let block_number = block.header.number;
                tracing::debug!(
                    block_number = block_number.0,
                    "Block scheduled in underlying storage"
                );
            }

            #[cfg(test)]
            self.events_sender
                .send(BufferedStorageEvent::UpdateReceived(store_block_number));
        }
    }
}

#[async_trait]
impl<T: ContiguousBlockStore> BlockStore for Buffered<T> {
    async fn head_block(&self, ctx: &ctx::Ctx) -> StorageResult<FinalBlock> {
        let buffered_head_block = sync::lock(ctx, &self.buffer).await?.head_block();
        if let Some(block) = buffered_head_block {
            return Ok(block);
        }
        self.inner.head_block(ctx).await
    }

    async fn first_block(&self, ctx: &ctx::Ctx) -> StorageResult<FinalBlock> {
        // First block is always situated in the underlying store
        self.inner.first_block(ctx).await
    }

    async fn last_contiguous_block_number(&self, ctx: &ctx::Ctx) -> StorageResult<BlockNumber> {
        Ok(sync::lock(ctx, &self.buffer)
            .await?
            .last_contiguous_block_number())
    }

    async fn block(
        &self,
        ctx: &ctx::Ctx,
        number: BlockNumber,
    ) -> StorageResult<Option<FinalBlock>> {
        let started_at = Instant::now();
        {
            let buffer = sync::lock(ctx, &self.buffer).await?;
            if number > buffer.store_block_number {
                let block = buffer.blocks.get(&number).cloned();
                METRICS.get_block_latency[&BlockResponseKind::InMemory]
                    .observe(started_at.elapsed());
                return Ok(block);
            }
        }
        let block = self.inner.block(ctx, number).await?;
        METRICS.get_block_latency[&BlockResponseKind::Persisted].observe(started_at.elapsed());
        Ok(block)
    }

    async fn missing_block_numbers(
        &self,
        ctx: &ctx::Ctx,
        range: ops::Range<BlockNumber>,
    ) -> StorageResult<Vec<BlockNumber>> {
        // By design, the underlying store has no missing blocks.
        Ok(sync::lock(ctx, &self.buffer)
            .await?
            .missing_block_numbers(range))
    }

    fn subscribe_to_block_writes(&self) -> watch::Receiver<BlockNumber> {
        self.block_writes_sender.subscribe()
    }
}

#[async_trait]
impl<T: ContiguousBlockStore> WriteBlockStore for Buffered<T> {
    async fn put_block(&self, ctx: &ctx::Ctx, block: &FinalBlock) -> StorageResult<()> {
        let buffer_block_latency = METRICS.buffer_block_latency.start();
        let next_block_for_store = {
            let mut buffer = sync::lock(ctx, &self.buffer).await?;
            let block_number = block.header.number;
            if block_number <= buffer.store_block_number {
                let err = anyhow::anyhow!(
                    "Cannot replace a block #{block_number} since it is already present in the underlying storage",
                );
                return Err(StorageError::Database(err));
            }
            buffer.put_block(block.clone());
            buffer.next_block_for_store()
        };

        if let Some(block) = next_block_for_store {
            self.inner.schedule_next_block(ctx, &block).await?;
            tracing::debug!(
                block_number = block.header.number.0,
                "Block scheduled in underlying storage"
            );
        }
        self.block_writes_sender.send_replace(block.header.number);
        buffer_block_latency.observe();
        Ok(())
    }
}