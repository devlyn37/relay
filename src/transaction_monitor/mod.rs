use anyhow::Context;
use ethers::{
    providers::{Middleware, StreamExt},
    types::{
        transaction::eip2718::TypedTransaction, BlockId, Eip1559TransactionRequest, TxHash, H256,
        U256,
    },
};
use futures_util::lock::Mutex;
use std::{cmp::max, pin::Pin, sync::Arc};
use tracing::{info, trace};
use uuid::Uuid;

use tokio::{
    spawn,
    time::{sleep, Duration},
};
type WatcherFuture<'a> = Pin<Box<dyn futures_util::stream::Stream<Item = H256> + Send + 'a>>;

#[derive(Debug)]
pub enum Status {
    Pending,
    Complete,
}

#[derive(Debug)]
pub struct TransactionMonitor<M> {
    pub provider: Arc<M>,
    pub txs: Arc<Mutex<Vec<(TxHash, Eip1559TransactionRequest, Option<BlockId>, Uuid)>>>, // Is the mutex really necessary here, we're only gonna have two tasks sharing this
    pub block_frequency: u8,
}

impl<M> Clone for TransactionMonitor<M> {
    fn clone(&self) -> Self {
        TransactionMonitor {
            provider: self.provider.clone(),
            txs: self.txs.clone(),
            block_frequency: self.block_frequency.clone(),
        }
    }
}

impl<M> TransactionMonitor<M>
where
    M: Middleware + 'static,
{
    pub fn new(provider: M, block_frequency: u8) -> Self {
        let this = Self {
            provider: Arc::new(provider),
            txs: Arc::new(Mutex::new(Vec::new())),
            block_frequency,
        };

        {
            let this2 = this.clone();
            spawn(async move {
                this2.monitor().await.unwrap();
            });
        }

        this
    }

    pub async fn send_monitored_transaction(
        &self,
        tx: Eip1559TransactionRequest,
        block: Option<BlockId>,
    ) -> Result<Uuid, anyhow::Error> {
        let mut with_gas = tx.clone();
        if with_gas.max_fee_per_gas.is_none() || with_gas.max_priority_fee_per_gas.is_none() {
            let (estimate_max_fee, estimate_max_priority_fee) = self
                .provider
                .estimate_eip1559_fees(None)
                .await
                .with_context(|| "error estimating gas")?;
            with_gas.max_fee_per_gas = Some(estimate_max_fee);
            with_gas.max_priority_fee_per_gas = Some(estimate_max_priority_fee);
        }
        let mut filled: TypedTransaction = with_gas.clone().into();
        self.provider
            .fill_transaction(&mut filled, None)
            .await
            .with_context(|| "error while filling transaction")?;

        info!("Filled Transaction {:?}", filled);

        let pending_tx = self
            .provider
            .send_transaction(filled.clone(), block)
            .await
            .with_context(|| "error sending transaction")?;

        let id = Uuid::new_v4();

        // insert the tx in the pending txs
        let mut lock = self.txs.lock().await;
        lock.push((*pending_tx, filled.clone().into(), block, id));

        Ok(id)
    }

    // TODO improve this XD
    pub async fn get_transaction_status(&self, id: Uuid) -> Status {
        let lock = self.txs.lock().await;
        info!("here's the current txs {:?}", lock);
        match lock.iter().find(|(_, _, _, entry_id)| id == *entry_id) {
            None => Status::Complete,
            Some(_) => Status::Pending,
        }
    }

    pub async fn monitor(&self) -> Result<(), anyhow::Error> {
        info!("Monitoring for escalation!");
        let mut watcher: WatcherFuture = Box::pin(
            self.provider
                .watch_blocks()
                .await
                .with_context(|| "Block streaming failure")?
                .map(|hash| (hash)),
        );
        let mut block_count = 0;

        while let Some(block_hash) = watcher.next().await {
            // We know the block exists at this point
            info!("Block {:?} has been mined", block_hash);
            block_count = block_count + 1;

            let block = self
                .provider
                .get_block_with_txs(block_hash)
                .await
                .with_context(|| "error while fetching block")?
                .unwrap();
            sleep(Duration::from_secs(1)).await; // to avoid rate limiting

            let (estimate_max_fee, estimate_max_priority_fee) = self
                .provider
                .estimate_eip1559_fees(None)
                .await
                .with_context(|| "error estimating gas prices")?;

            let mut txs = self.txs.lock().await;
            let len = txs.len();

            for _ in 0..len {
                // this must never panic as we're explicitly within bounds
                let (tx_hash, mut replacement_tx, priority, id) =
                    txs.pop().expect("should have element in vector");

                let tx_has_been_included = block
                    .transactions
                    .iter()
                    .find(|tx| tx.hash == tx_hash)
                    .is_some();
                info!("checking if transaction {:?} was included", tx_hash);

                if tx_has_been_included {
                    info!("transaction {:?} was included", tx_hash);
                    continue;
                }

                if block_count % self.block_frequency != 0 {
                    info!(
                        "transaction {:?} was not included, not sending replacement yet",
                        tx_hash
                    );
                    txs.push((tx_hash, replacement_tx, priority, id));
                    continue;
                }

                match self
                    .rebroadcast(
                        &mut replacement_tx,
                        estimate_max_fee,
                        estimate_max_priority_fee,
                        priority,
                    )
                    .await?
                {
                    Some(new_txhash) => {
                        info!("Transaction {:?} replaced with {:?}", tx_hash, new_txhash);
                        txs.push((new_txhash, replacement_tx, priority, id));
                        sleep(Duration::from_secs(1)).await; // to avoid rate limiting TODO add retries
                    }
                    None => {}
                }
            }
        }

        Ok(())
    }

    async fn rebroadcast(
        &self,
        tx: &mut Eip1559TransactionRequest,
        estimate_max_fee: U256,
        estimate_max_priority_fee: U256,
        priority: Option<BlockId>,
    ) -> Result<Option<H256>, anyhow::Error> {
        self.bump_transaction(tx, estimate_max_fee, estimate_max_priority_fee);

        match self.provider.send_transaction(tx.clone(), priority).await {
            Ok(new_tx_hash) => {
                return Ok(Some(*new_tx_hash));
            }
            Err(err) => {
                // ignore "nonce too low" errors because they
                // may happen if we try to broadcast a higher
                // gas price tx when one of the previous ones
                // was already mined (meaning we also do not
                // push it back to the pending txs vector)
                if err.to_string().contains("nonce too low") {
                    info!("transaction has already been included");
                    return Ok(None);
                }

                return Err(anyhow::anyhow!(err));
            }
        };
    }

    fn bump_transaction(
        &self,
        tx: &mut Eip1559TransactionRequest,
        estimate_max_fee: U256,
        estimate_max_priority_fee: U256,
    ) {
        // We should never risk getting gas too low errors because we set these vals in send_monitored_transaction
        let prev_max_priority_fee = tx
            .max_priority_fee_per_gas
            .unwrap_or(estimate_max_priority_fee);
        let prev_max_fee = tx.max_fee_per_gas.unwrap_or(estimate_max_fee);

        let new_max_priority_fee = max(
            estimate_max_priority_fee,
            self.increase_by_minimum(prev_max_priority_fee),
        );

        let estimate_base_fee = estimate_max_fee - estimate_max_priority_fee;
        let prev_base_fee = prev_max_fee - prev_max_priority_fee;
        let new_base_fee = max(estimate_base_fee, self.increase_by_minimum(prev_base_fee));
        let new_max_fee = new_base_fee + new_max_priority_fee;

        tx.max_fee_per_gas = Some(new_max_fee);
        tx.max_priority_fee_per_gas = Some(new_max_priority_fee);
    }

    // Rule: both the tip and the max fee must
    // be bumped by a minimum of 10%
    // https://github.com/ethereum/go-ethereum/issues/23616#issuecomment-924657965
    fn increase_by_minimum(&self, value: U256) -> U256 {
        let increase = (value * 10) / 100u64;
        value + increase + 1 // add 1 here for rounding purposes
    }
}