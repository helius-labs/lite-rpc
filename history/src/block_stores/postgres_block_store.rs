use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use itertools::Itertools;
use log::{info, warn};
use solana_lite_rpc_core::{
    structures::{epoch::EpochCache, produced_block::ProducedBlock},
    traits::block_storage_interface::BlockStorageInterface,
};
use solana_rpc_client_api::config::RpcBlockConfig;
use solana_sdk::{slot_history::Slot, stake_history::Epoch};
use tokio::sync::RwLock;
use tokio_postgres::error::{DbError, SqlState};

use crate::postgres::{
    postgres_block::PostgresBlock, postgres_session::PostgresSessionCache,
    postgres_transaction::PostgresTransaction,
};

#[derive(Default, Clone, Copy)]
pub struct PostgresData {
    from_slot: Slot,
    to_slot: Slot,
    current_epoch: Epoch,
}

pub struct PostgresBlockStore {
    session_cache: PostgresSessionCache,
    epoch_cache: EpochCache,
    postgres_data: Arc<RwLock<PostgresData>>,
}

impl PostgresBlockStore {

    pub async fn new(epoch_cache: EpochCache) -> Self {
        let session_cache = PostgresSessionCache::new().await.unwrap();
        let postgres_data = Arc::new(RwLock::new(PostgresData::default()));
        Self {
            session_cache,
            epoch_cache,
            postgres_data,
        }
    }

    async fn start_new_epoch(&self, schema: &String) -> Result<()> {
        // create schema for new epoch
        let session = self
            .session_cache
            .get_session()
            .await
            .expect("should get new postgres session");

        let statement = format!("CREATE SCHEMA {} AUTHORIZATION CURRENT_ROLE;", schema);
        // note: requires GRANT CREATE ON DATABASE xyz
        let result_create_schema = session.execute(&statement, &[]).await;
        if let Err(err) = result_create_schema {
            if err.code().map(|sqlstate| sqlstate == &SqlState::DUPLICATE_SCHEMA).unwrap_or_default() {
                // TODO: do we want to allow this; continuing with existing epoch schema might lead to inconsistent data in blocks and transactions table
                warn!("Schema {} already exists - data will be appended", schema);
                return Ok(());
            } else {
                return Err(err).context("create schema for new epoch");
            }
        }

        // Create blocks table
        let statement = PostgresBlock::build_create_table_statement(schema);
        session.execute(&statement, &[]).await
            .context("create blocks table for new epoch")?;

        // create transaction table
        let statement = PostgresTransaction::build_create_table_statement(schema);
        session.execute(&statement, &[]).await
            .context("create transaction table for new epoch")?;

        // add foreign key constraint between transactions and blocks
        let statement = PostgresTransaction::build_foreign_key_statement(schema);
        session.execute(&statement, &[]).await
            .context("create foreign key constraint between transactions and blocks")?;

        Ok(())
    }
}

#[async_trait]
impl BlockStorageInterface for PostgresBlockStore {
    async fn save(&self, block: ProducedBlock) -> Result<()> {
        let PostgresData { current_epoch, .. } = { *self.postgres_data.read().await };

        let slot = block.slot;
        let transactions = block
            .transactions
            .iter()
            .map(|x| PostgresTransaction::new(x, slot))
            .collect_vec();
        let postgres_block = PostgresBlock::from(&block);

        let epoch = self.epoch_cache.get_epoch_at_slot(slot);
        let schema = format!("lite_rpc_epoch_{}", epoch.epoch);
        if current_epoch == 0 || current_epoch < epoch.epoch {
            self.postgres_data.write().await.current_epoch = epoch.epoch;
            self.start_new_epoch(&schema).await?;
        }

        const NUMBER_OF_TRANSACTION: usize = 20;

        // save transaction
        let chunks = transactions.chunks(NUMBER_OF_TRANSACTION);
        let session = self
            .session_cache
            .get_session()
            .await
            .expect("should get new postgres session");
        for chunk in chunks {
            PostgresTransaction::save_transactions(&session, &schema, chunk).await?;
        }
        postgres_block.save(&session, &schema).await?;
        Ok(())
    }

    async fn get(&self, slot: Slot, _config: RpcBlockConfig) -> Result<ProducedBlock> {
        let range = self.get_slot_range().await;
        if range.contains(&slot) {}
        todo!()
    }

    async fn get_slot_range(&self) -> std::ops::Range<Slot> {
        let lk = self.postgres_data.read().await;
        lk.from_slot..lk.to_slot + 1
    }
}



#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use anyhow::Context;
    use solana_sdk::commitment_config::CommitmentConfig;
    use solana_sdk::signature::Signature;
    use tokio_postgres::NoTls;
    use solana_lite_rpc_core::structures::produced_block::TransactionInfo;
    use super::*;

    #[tokio::test]
    async fn test_connection() {
        std::env::set_var("PG_CONFIG", "host=localhost dbname=literpc3 user=literpc_app password=litelitesecret sslmode=disable");
        let pg_config = std::env::var("PG_CONFIG").context("env PG_CONFIG not found").unwrap();
        let pg_config = pg_config.parse::<tokio_postgres::Config>().unwrap();

        println!("use connection {:?}", pg_config);

        let (client, _connection) = pg_config.connect(NoTls).await.unwrap();


        let _user_row = client.execute("SELECT CURRENT_USER", &[]).await.unwrap();

        // println!("user_row {:?}", user_row);




    }



    #[tokio::test]
    #[ignore]
    async fn test_save_block() {
        tracing_subscriber::fmt::init();

        std::env::set_var("PG_CONFIG", "host=localhost dbname=literpc3 user=literpc_app password=litelitesecret sslmode=disable");
        let _postgres_session_cache = PostgresSessionCache::new().await.unwrap();
        let epoch_cache = EpochCache::new_for_tests();

        let postgres_block_store = PostgresBlockStore::new(epoch_cache.clone()).await;

        postgres_block_store.save(create_test_block()).await.unwrap();


    }

    fn create_test_block() -> ProducedBlock {

        let sig1 = Signature::from_str("5VBroA4MxsbZdZmaSEb618WRRwhWYW9weKhh3md1asGRx7nXDVFLua9c98voeiWdBE7A9isEoLL7buKyaVRSK1pV").unwrap();
        let sig2 = Signature::from_str("3d9x3rkVQEoza37MLJqXyadeTbEJGUB6unywK4pjeRLJc16wPsgw3dxPryRWw3UaLcRyuxEp1AXKGECvroYxAEf2").unwrap();

        ProducedBlock {
            block_height: 42,
            blockhash: "blockhash".to_string(),
            previous_blockhash: "previous_blockhash".to_string(),
            parent_slot: 666,
            slot: 667,
            transactions: vec![
                create_test_tx(sig1),
                create_test_tx(sig2),
            ],
            // TODO double if this is unix millis or seconds
            block_time: 1699260872000,
            commitment_config: CommitmentConfig::finalized(),
            leader_id: None,
            rewards: None,
        }
    }

    fn create_test_tx(signature: Signature) -> TransactionInfo {
        TransactionInfo {
            signature: signature.to_string(),
            err: None,
            cu_requested: Some(40000),
            prioritization_fees: Some(5000),
            cu_consumed: Some(32000),
            recent_blockhash: "recent_blockhash".to_string(),
            message: "some message".to_string(),
        }
    }
}


