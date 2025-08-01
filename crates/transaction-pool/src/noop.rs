//! A transaction pool implementation that does nothing.
//!
//! This is useful for wiring components together that don't require an actual pool but still need
//! to be generic over it.

use crate::{
    blobstore::BlobStoreError,
    error::{InvalidPoolTransactionError, PoolError},
    pool::TransactionListenerKind,
    traits::{BestTransactionsAttributes, GetPooledTransactionLimit, NewBlobSidecar},
    validate::ValidTransaction,
    AddedTransactionOutcome, AllPoolTransactions, AllTransactionsEvents, BestTransactions,
    BlockInfo, EthPoolTransaction, EthPooledTransaction, NewTransactionEvent, PoolResult, PoolSize,
    PoolTransaction, PropagatedTransactions, TransactionEvents, TransactionOrigin, TransactionPool,
    TransactionValidationOutcome, TransactionValidator, ValidPoolTransaction,
};
use alloy_eips::{
    eip1559::ETHEREUM_BLOCK_GAS_LIMIT_30M,
    eip4844::{BlobAndProofV1, BlobAndProofV2},
    eip7594::BlobTransactionSidecarVariant,
};
use alloy_primitives::{Address, TxHash, B256, U256};
use reth_eth_wire_types::HandleMempoolData;
use reth_primitives_traits::Recovered;
use std::{collections::HashSet, marker::PhantomData, sync::Arc};
use tokio::sync::{mpsc, mpsc::Receiver};

/// A [`TransactionPool`] implementation that does nothing.
///
/// All transactions are rejected and no events are emitted.
/// This type will never hold any transactions and is only useful for wiring components together.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NoopTransactionPool<T: EthPoolTransaction = EthPooledTransaction> {
    /// Type marker
    _marker: PhantomData<T>,
}

impl<T: EthPoolTransaction> NoopTransactionPool<T> {
    /// Creates a new [`NoopTransactionPool`].
    pub fn new() -> Self {
        Self { _marker: Default::default() }
    }
}

impl Default for NoopTransactionPool<EthPooledTransaction> {
    fn default() -> Self {
        Self { _marker: Default::default() }
    }
}

impl<T: EthPoolTransaction> TransactionPool for NoopTransactionPool<T> {
    type Transaction = T;

    fn pool_size(&self) -> PoolSize {
        Default::default()
    }

    fn block_info(&self) -> BlockInfo {
        BlockInfo {
            block_gas_limit: ETHEREUM_BLOCK_GAS_LIMIT_30M,
            last_seen_block_hash: Default::default(),
            last_seen_block_number: 0,
            pending_basefee: 0,
            pending_blob_fee: None,
        }
    }

    async fn add_transaction_and_subscribe(
        &self,
        _origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<TransactionEvents> {
        let hash = *transaction.hash();
        Err(PoolError::other(hash, Box::new(NoopInsertError::new(transaction))))
    }

    async fn add_transaction(
        &self,
        _origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<AddedTransactionOutcome> {
        let hash = *transaction.hash();
        Err(PoolError::other(hash, Box::new(NoopInsertError::new(transaction))))
    }

    async fn add_transactions(
        &self,
        _origin: TransactionOrigin,
        transactions: Vec<Self::Transaction>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        transactions
            .into_iter()
            .map(|transaction| {
                let hash = *transaction.hash();
                Err(PoolError::other(hash, Box::new(NoopInsertError::new(transaction))))
            })
            .collect()
    }

    fn transaction_event_listener(&self, _tx_hash: TxHash) -> Option<TransactionEvents> {
        None
    }

    fn all_transactions_event_listener(&self) -> AllTransactionsEvents<Self::Transaction> {
        AllTransactionsEvents::new(mpsc::channel(1).1)
    }

    fn pending_transactions_listener_for(
        &self,
        _kind: TransactionListenerKind,
    ) -> Receiver<TxHash> {
        mpsc::channel(1).1
    }

    fn new_transactions_listener(&self) -> Receiver<NewTransactionEvent<Self::Transaction>> {
        mpsc::channel(1).1
    }

    fn blob_transaction_sidecars_listener(&self) -> Receiver<NewBlobSidecar> {
        mpsc::channel(1).1
    }

    fn new_transactions_listener_for(
        &self,
        _kind: TransactionListenerKind,
    ) -> Receiver<NewTransactionEvent<Self::Transaction>> {
        mpsc::channel(1).1
    }

    fn pooled_transaction_hashes(&self) -> Vec<TxHash> {
        vec![]
    }

    fn pooled_transaction_hashes_max(&self, _max: usize) -> Vec<TxHash> {
        vec![]
    }

    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn pooled_transactions_max(
        &self,
        _max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn get_pooled_transaction_elements(
        &self,
        _tx_hashes: Vec<TxHash>,
        _limit: GetPooledTransactionLimit,
    ) -> Vec<<Self::Transaction as PoolTransaction>::Pooled> {
        vec![]
    }

    fn get_pooled_transaction_element(
        &self,
        _tx_hash: TxHash,
    ) -> Option<Recovered<<Self::Transaction as PoolTransaction>::Pooled>> {
        None
    }

    fn best_transactions(
        &self,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        Box::new(std::iter::empty())
    }

    fn best_transactions_with_attributes(
        &self,
        _: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        Box::new(std::iter::empty())
    }

    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn pending_transactions_max(
        &self,
        _max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        (0, 0)
    }

    fn all_transactions(&self) -> AllPoolTransactions<Self::Transaction> {
        AllPoolTransactions::default()
    }

    fn remove_transactions(
        &self,
        _hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn remove_transactions_and_descendants(
        &self,
        _hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn remove_transactions_by_sender(
        &self,
        _sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn retain_unknown<A>(&self, _announcement: &mut A)
    where
        A: HandleMempoolData,
    {
    }

    fn get(&self, _tx_hash: &TxHash) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        None
    }

    fn get_all(&self, _txs: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn on_propagated(&self, _txs: PropagatedTransactions) {}

    fn get_transactions_by_sender(
        &self,
        _sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn get_pending_transactions_with_predicate(
        &self,
        _predicate: impl FnMut(&ValidPoolTransaction<Self::Transaction>) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn get_pending_transactions_by_sender(
        &self,
        _sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn get_queued_transactions_by_sender(
        &self,
        _sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn get_highest_transaction_by_sender(
        &self,
        _sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        None
    }

    fn get_highest_consecutive_transaction_by_sender(
        &self,
        _sender: Address,
        _on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        None
    }

    fn get_transaction_by_sender_and_nonce(
        &self,
        _sender: Address,
        _nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        None
    }

    fn get_transactions_by_origin(
        &self,
        _origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn get_pending_transactions_by_origin(
        &self,
        _origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        vec![]
    }

    fn unique_senders(&self) -> HashSet<Address> {
        Default::default()
    }

    fn get_blob(
        &self,
        _tx_hash: TxHash,
    ) -> Result<Option<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        Ok(None)
    }

    fn get_all_blobs(
        &self,
        _tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<(TxHash, Arc<BlobTransactionSidecarVariant>)>, BlobStoreError> {
        Ok(vec![])
    }

    fn get_all_blobs_exact(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        if tx_hashes.is_empty() {
            return Ok(vec![])
        }
        Err(BlobStoreError::MissingSidecar(tx_hashes[0]))
    }

    fn get_blobs_for_versioned_hashes_v1(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV1>>, BlobStoreError> {
        Ok(vec![None; versioned_hashes.len()])
    }

    fn get_blobs_for_versioned_hashes_v2(
        &self,
        _versioned_hashes: &[B256],
    ) -> Result<Option<Vec<BlobAndProofV2>>, BlobStoreError> {
        Ok(None)
    }
}

/// A [`TransactionValidator`] that does nothing.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MockTransactionValidator<T> {
    propagate_local: bool,
    return_invalid: bool,
    _marker: PhantomData<T>,
}

impl<T: EthPoolTransaction> TransactionValidator for MockTransactionValidator<T> {
    type Transaction = T;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        mut transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        if self.return_invalid {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidPoolTransactionError::Underpriced,
            );
        }
        let maybe_sidecar = transaction.take_blob().maybe_sidecar().cloned();
        // we return `balance: U256::MAX` to simulate a valid transaction which will never go into
        // overdraft
        TransactionValidationOutcome::Valid {
            balance: U256::MAX,
            state_nonce: 0,
            bytecode_hash: None,
            transaction: ValidTransaction::new(transaction, maybe_sidecar),
            propagate: match origin {
                TransactionOrigin::External => true,
                TransactionOrigin::Local => self.propagate_local,
                TransactionOrigin::Private => false,
            },
            authorities: None,
        }
    }
}

impl<T> MockTransactionValidator<T> {
    /// Creates a new [`MockTransactionValidator`] that does not allow local transactions to be
    /// propagated.
    pub fn no_propagate_local() -> Self {
        Self { propagate_local: false, return_invalid: false, _marker: Default::default() }
    }
    /// Creates a new [`MockTransactionValidator`] that always return a invalid outcome.
    pub fn return_invalid() -> Self {
        Self { propagate_local: false, return_invalid: true, _marker: Default::default() }
    }
}

impl<T> Default for MockTransactionValidator<T> {
    fn default() -> Self {
        Self { propagate_local: true, return_invalid: false, _marker: Default::default() }
    }
}

/// An error that contains the transaction that failed to be inserted into the noop pool.
#[derive(Debug, Clone, thiserror::Error)]
#[error("can't insert transaction into the noop pool that does nothing")]
pub struct NoopInsertError<T: EthPoolTransaction = EthPooledTransaction> {
    tx: T,
}

impl<T: EthPoolTransaction> NoopInsertError<T> {
    const fn new(tx: T) -> Self {
        Self { tx }
    }

    /// Returns the transaction that failed to be inserted.
    pub fn into_inner(self) -> T {
        self.tx
    }
}
