use std::time::Duration;

use anyhow::Context;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcBlockConfig;
use solana_sdk::{
    borsh::try_from_slice_unchecked,
    commitment_config::CommitmentConfig,
    compute_budget::{self, ComputeBudgetInstruction},
    slot_history::Slot,
    transaction::TransactionError,
};
use solana_transaction_status::{
    option_serializer::OptionSerializer, RewardType, TransactionDetails, UiTransactionEncoding,
    UiTransactionStatusMeta,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::slot_clock::AVERAGE_SLOT_CHANGE_TIME_IN_MILLIS;

#[derive(Debug)]
pub struct TransactionInfo {
    pub signature: String,
    pub err: Option<TransactionError>,
    pub status: Result<(), TransactionError>,
    pub cu_requested: Option<u32>,
    pub prioritization_fees: Option<u64>,
    pub cu_consumed: Option<u64>,
}

#[derive(Default, Debug)]
pub struct ProcessedBlock {
    pub txs: Vec<TransactionInfo>,
    pub leader_id: Option<String>,
    pub blockhash: String,
    pub block_height: u64,
    pub slot: Slot,
    pub parent_slot: Slot,
    pub block_time: u64,
    pub commitment_config: CommitmentConfig,
}

pub enum BlockProcessorError {
    Incomplete,
}

pub struct JsonRpcClient;

impl JsonRpcClient {
    pub async fn process(
        rpc_client: &RpcClient,
        slot: Slot,
        commitment_config: CommitmentConfig,
    ) -> anyhow::Result<Result<ProcessedBlock, BlockProcessorError>> {
        let block = rpc_client
            .get_block_with_config(
                slot,
                RpcBlockConfig {
                    transaction_details: Some(TransactionDetails::Full),
                    commitment: Some(commitment_config),
                    max_supported_transaction_version: Some(0),
                    encoding: Some(UiTransactionEncoding::Base64),
                    rewards: Some(true),
                },
            )
            .await
            .context("failed to get block")?;

        // println!("slot {slot:?} block: {:?}", block);

        let Some(block_height) = block.block_height else {
            return Ok(Err(BlockProcessorError::Incomplete));
        };

        let Some(txs) = block.transactions else {
            return Ok(Err(BlockProcessorError::Incomplete));
         };

        let blockhash = block.blockhash;
        let parent_slot = block.parent_slot;

        let txs = txs.into_iter().filter_map(|tx| {
            let Some(UiTransactionStatusMeta { err, status, compute_units_consumed ,.. }) = tx.meta else {
                log::info!("Tx with no meta");
                return None;
            };

            let Some(tx) = tx.transaction.decode() else {
                log::info!("Tx could not be decoded");
                return None;
            };

            let signature = tx.signatures[0].to_string();
            let cu_consumed = match compute_units_consumed {
                OptionSerializer::Some(cu_consumed) => Some(cu_consumed),
                _ => None,
            };

            let legacy_compute_budget = tx.message.instructions().iter().find_map(|i| {
                if i.program_id(tx.message.static_account_keys())
                    .eq(&compute_budget::id())
                {
                    if let Ok(ComputeBudgetInstruction::RequestUnitsDeprecated {
                        units,
                        additional_fee,
                    }) = try_from_slice_unchecked(i.data.as_slice())
                    {
                        return Some((units, additional_fee));
                    }
                }
                None
            });

            let mut cu_requested = tx.message.instructions().iter().find_map(|i| {
                if i.program_id(tx.message.static_account_keys())
                    .eq(&compute_budget::id())
                {
                    if let Ok(ComputeBudgetInstruction::SetComputeUnitLimit(limit)) =
                        try_from_slice_unchecked(i.data.as_slice())
                    {
                        return Some(limit);
                    }
                }
                None
            });

            let mut prioritization_fees = tx.message.instructions().iter().find_map(|i| {
                if i.program_id(tx.message.static_account_keys())
                    .eq(&compute_budget::id())
                {
                    if let Ok(ComputeBudgetInstruction::SetComputeUnitPrice(price)) =
                        try_from_slice_unchecked(i.data.as_slice())
                    {
                        return Some(price);
                    }
                }

                None
            });

            if let Some((units, additional_fee)) = legacy_compute_budget {
                cu_requested = Some(units);
                if additional_fee > 0 {
                    prioritization_fees = Some(((units * 1000) / additional_fee).into())
                }
            };

            Some(TransactionInfo {
                signature,
                err,
                status,
                cu_requested,
                prioritization_fees,
                cu_consumed,
            })
        }).collect();

        let leader_id = if let Some(rewards) = block.rewards {
            rewards
                .iter()
                .find(|reward| Some(RewardType::Fee) == reward.reward_type)
                .map(|leader_reward| leader_reward.pubkey.clone())
        } else {
            None
        };

        let block_time = block.block_time.unwrap_or(0) as u64;

        Ok(Ok(ProcessedBlock {
            txs,
            block_height,
            leader_id,
            blockhash,
            slot,
            parent_slot,
            block_time,
            commitment_config,
        }))
    }

    pub async fn poll_slots(
        rpc_client: &RpcClient,
        slot_tx: UnboundedSender<Slot>,
        commitment_config: CommitmentConfig,
    ) -> anyhow::Result<()> {
        let mut poll_frequency = tokio::time::interval(Duration::from_millis(
            AVERAGE_SLOT_CHANGE_TIME_IN_MILLIS - 100,
        ));

        let mut last_slot = 0;

        loop {
            let slot = rpc_client
                .get_slot_with_commitment(commitment_config)
                .await
                .context("Error getting slot")?;

            // send if slot is greater than last slot
            if slot > last_slot {
                slot_tx.send(slot).context("Error sending slot")?;
            }

            // overwrite last slot
            last_slot = slot;

            // wait for next poll i.e at least 50ms
            poll_frequency.tick().await;
        }
    }
}