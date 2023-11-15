use anyhow::anyhow;
use async_trait::async_trait;
use log::trace;
use solana_lite_rpc_core::{
    commitment_utils::Commitment, structures::produced_block::ProducedBlock,
    traits::block_storage_interface::BlockStorageInterface,
};
use solana_sdk::slot_history::Slot;
use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use tokio::sync::RwLock;

pub struct InmemoryBlockStore {
    block_storage: RwLock<BTreeMap<Slot, ProducedBlock>>,
    number_of_blocks_to_store: usize,
}

impl InmemoryBlockStore {
    pub fn new(number_of_blocks_to_store: usize) -> Self {
        Self {
            number_of_blocks_to_store,
            block_storage: RwLock::new(BTreeMap::new()),
        }
    }

    pub async fn save(&self, block: ProducedBlock) -> anyhow::Result<()> {
        trace!("Saving block {} to memory storage...", block.slot);
        self.store(block).await;
        Ok(())
    }

    pub async fn store(&self, block: ProducedBlock) {
        let slot = block.slot;
        let mut block_storage = self.block_storage.write().await;
        let min_slot = match block_storage.first_key_value() {
            Some((slot, _)) => *slot,
            None => 0,
        };
        if slot >= min_slot {
            // overwrite block only if confirmation has changed
            match block_storage.get_mut(&slot) {
                Some(x) => {
                    let commitment_store = Commitment::from(x.commitment_config);
                    let commitment_block = Commitment::from(block.commitment_config);
                    let overwrite = commitment_block > commitment_store;
                    if overwrite {
                        *x = block.clone();
                    }
                }
                None => {
                    block_storage.insert(slot, block.clone());
                }
            }
            if block_storage.len() > self.number_of_blocks_to_store {
                block_storage.remove(&min_slot);
            }
        }
    }
}

#[async_trait]
impl BlockStorageInterface for InmemoryBlockStore {

    async fn get(&self, slot: Slot) -> anyhow::Result<ProducedBlock> {
        self.block_storage
            .read()
            .await
            .get(&slot)
            .cloned()
            .ok_or(anyhow!("Block {} not found in in-memory storage", slot))
    }

    async fn get_slot_range(&self) -> RangeInclusive<Slot> {
        let storage = self.block_storage.read().await;
        let first = storage.keys().min();
        let last = storage.keys().max();

        assert_eq!(first.is_some(), last.is_some());

        match (first, last) {
            (Some(a), Some(b)) => RangeInclusive::new(*a, *b),
            _ => RangeInclusive::new(1, 0),
        }
    }
}
