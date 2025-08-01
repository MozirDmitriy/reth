//! Transactions management for the p2p network.

/// Aggregation on configurable parameters for [`TransactionsManager`].
pub mod config;
/// Default and spec'd bounds.
pub mod constants;
/// Component responsible for fetching transactions from [`NewPooledTransactionHashes`].
pub mod fetcher;
/// Defines the [`TransactionPolicies`] trait for aggregating transaction-related policies.
pub mod policy;

pub use self::constants::{
    tx_fetcher::DEFAULT_SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESP_ON_PACK_GET_POOLED_TRANSACTIONS_REQ,
    SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESPONSE,
};
use config::{AnnouncementAcceptance, StrictEthAnnouncementFilter, TransactionPropagationKind};
pub use config::{
    AnnouncementFilteringPolicy, TransactionFetcherConfig, TransactionPropagationMode,
    TransactionPropagationPolicy, TransactionsManagerConfig,
};
use policy::{NetworkPolicies, TransactionPolicies};

pub(crate) use fetcher::{FetchEvent, TransactionFetcher};

use self::constants::{tx_manager::*, DEFAULT_SOFT_LIMIT_BYTE_SIZE_TRANSACTIONS_BROADCAST_MESSAGE};
use crate::{
    budget::{
        DEFAULT_BUDGET_TRY_DRAIN_NETWORK_TRANSACTION_EVENTS,
        DEFAULT_BUDGET_TRY_DRAIN_PENDING_POOL_IMPORTS, DEFAULT_BUDGET_TRY_DRAIN_POOL_IMPORTS,
        DEFAULT_BUDGET_TRY_DRAIN_STREAM,
    },
    cache::LruCache,
    duration_metered_exec, metered_poll_nested_stream_with_budget,
    metrics::{
        AnnouncedTxTypesMetrics, TransactionsManagerMetrics, NETWORK_POOL_TRANSACTIONS_SCOPE,
    },
    NetworkHandle, TxTypesCounter,
};
use alloy_primitives::{TxHash, B256};
use constants::SOFT_LIMIT_COUNT_HASHES_IN_NEW_POOLED_TRANSACTIONS_BROADCAST_MESSAGE;
use futures::{stream::FuturesUnordered, Future, StreamExt};
use reth_eth_wire::{
    DedupPayload, EthNetworkPrimitives, EthVersion, GetPooledTransactions, HandleMempoolData,
    HandleVersionedMempoolData, NetworkPrimitives, NewPooledTransactionHashes,
    NewPooledTransactionHashes66, NewPooledTransactionHashes68, PooledTransactions,
    RequestTxHashes, Transactions, ValidAnnouncementData,
};
use reth_ethereum_primitives::{TransactionSigned, TxType};
use reth_metrics::common::mpsc::UnboundedMeteredReceiver;
use reth_network_api::{
    events::{PeerEvent, SessionInfo},
    NetworkEvent, NetworkEventListenerProvider, PeerKind, PeerRequest, PeerRequestSender, Peers,
};
use reth_network_p2p::{
    error::{RequestError, RequestResult},
    sync::SyncStateProvider,
};
use reth_network_peers::PeerId;
use reth_network_types::ReputationChangeKind;
use reth_primitives_traits::SignedTransaction;
use reth_tokio_util::EventStream;
use reth_transaction_pool::{
    error::{PoolError, PoolResult},
    AddedTransactionOutcome, GetPooledTransactionLimit, PoolTransaction, PropagateKind,
    PropagatedTransactions, TransactionPool, ValidPoolTransaction,
};
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, oneshot::error::RecvError};
use tokio_stream::wrappers::{ReceiverStream, UnboundedReceiverStream};
use tracing::{debug, trace};

/// The future for importing transactions into the pool.
///
/// Resolves with the result of each transaction import.
pub type PoolImportFuture =
    Pin<Box<dyn Future<Output = Vec<PoolResult<AddedTransactionOutcome>>> + Send + 'static>>;

/// Api to interact with [`TransactionsManager`] task.
///
/// This can be obtained via [`TransactionsManager::handle`] and can be used to manually interact
/// with the [`TransactionsManager`] task once it is spawned.
///
/// For example [`TransactionsHandle::get_peer_transaction_hashes`] returns the transaction hashes
/// known by a specific peer.
#[derive(Debug, Clone)]
pub struct TransactionsHandle<N: NetworkPrimitives = EthNetworkPrimitives> {
    /// Command channel to the [`TransactionsManager`]
    manager_tx: mpsc::UnboundedSender<TransactionsCommand<N>>,
}

/// Implementation of the `TransactionsHandle` API for use in testnet via type
/// [`PeerHandle`](crate::test_utils::PeerHandle).
impl<N: NetworkPrimitives> TransactionsHandle<N> {
    fn send(&self, cmd: TransactionsCommand<N>) {
        let _ = self.manager_tx.send(cmd);
    }

    /// Fetch the [`PeerRequestSender`] for the given peer.
    async fn peer_handle(
        &self,
        peer_id: PeerId,
    ) -> Result<Option<PeerRequestSender<PeerRequest<N>>>, RecvError> {
        let (tx, rx) = oneshot::channel();
        self.send(TransactionsCommand::GetPeerSender { peer_id, peer_request_sender: tx });
        rx.await
    }

    /// Manually propagate the transaction that belongs to the hash.
    pub fn propagate(&self, hash: TxHash) {
        self.send(TransactionsCommand::PropagateHash(hash))
    }

    /// Manually propagate the transaction hash to a specific peer.
    ///
    /// Note: this only propagates if the pool contains the transaction.
    pub fn propagate_hash_to(&self, hash: TxHash, peer: PeerId) {
        self.propagate_hashes_to(Some(hash), peer)
    }

    /// Manually propagate the transaction hashes to a specific peer.
    ///
    /// Note: this only propagates the transactions that are known to the pool.
    pub fn propagate_hashes_to(&self, hash: impl IntoIterator<Item = TxHash>, peer: PeerId) {
        let hashes = hash.into_iter().collect::<Vec<_>>();
        if hashes.is_empty() {
            return
        }
        self.send(TransactionsCommand::PropagateHashesTo(hashes, peer))
    }

    /// Request the active peer IDs from the [`TransactionsManager`].
    pub async fn get_active_peers(&self) -> Result<HashSet<PeerId>, RecvError> {
        let (tx, rx) = oneshot::channel();
        self.send(TransactionsCommand::GetActivePeers(tx));
        rx.await
    }

    /// Manually propagate full transaction hashes to a specific peer.
    ///
    /// Do nothing if transactions are empty.
    pub fn propagate_transactions_to(&self, transactions: Vec<TxHash>, peer: PeerId) {
        if transactions.is_empty() {
            return
        }
        self.send(TransactionsCommand::PropagateTransactionsTo(transactions, peer))
    }

    /// Manually propagate the given transaction hashes to all peers.
    ///
    /// It's up to the [`TransactionsManager`] whether the transactions are sent as hashes or in
    /// full.
    pub fn propagate_transactions(&self, transactions: Vec<TxHash>) {
        if transactions.is_empty() {
            return
        }
        self.send(TransactionsCommand::PropagateTransactions(transactions))
    }

    /// Manually propagate the given transactions to all peers.
    ///
    /// It's up to the [`TransactionsManager`] whether the transactions are sent as hashes or in
    /// full.
    pub fn broadcast_transactions(
        &self,
        transactions: impl IntoIterator<Item = N::BroadcastedTransaction>,
    ) {
        let transactions =
            transactions.into_iter().map(PropagateTransaction::new).collect::<Vec<_>>();
        if transactions.is_empty() {
            return
        }
        self.send(TransactionsCommand::BroadcastTransactions(transactions))
    }

    /// Request the transaction hashes known by specific peers.
    pub async fn get_transaction_hashes(
        &self,
        peers: Vec<PeerId>,
    ) -> Result<HashMap<PeerId, HashSet<TxHash>>, RecvError> {
        if peers.is_empty() {
            return Ok(Default::default())
        }
        let (tx, rx) = oneshot::channel();
        self.send(TransactionsCommand::GetTransactionHashes { peers, tx });
        rx.await
    }

    /// Request the transaction hashes known by a specific peer.
    pub async fn get_peer_transaction_hashes(
        &self,
        peer: PeerId,
    ) -> Result<HashSet<TxHash>, RecvError> {
        let res = self.get_transaction_hashes(vec![peer]).await?;
        Ok(res.into_values().next().unwrap_or_default())
    }

    /// Requests the transactions directly from the given peer.
    ///
    /// Returns `None` if the peer is not connected.
    ///
    /// **Note**: this returns the response from the peer as received.
    pub async fn get_pooled_transactions_from(
        &self,
        peer_id: PeerId,
        hashes: Vec<B256>,
    ) -> Result<Option<Vec<N::PooledTransaction>>, RequestError> {
        let Some(peer) = self.peer_handle(peer_id).await? else { return Ok(None) };

        let (tx, rx) = oneshot::channel();
        let request = PeerRequest::GetPooledTransactions { request: hashes.into(), response: tx };
        peer.try_send(request).ok();

        rx.await?.map(|res| Some(res.0))
    }
}

/// Manages transactions on top of the p2p network.
///
/// This can be spawned to another task and is supposed to be run as background service.
/// [`TransactionsHandle`] can be used as frontend to programmatically send commands to it and
/// interact with it.
///
/// The [`TransactionsManager`] is responsible for:
///    - handling incoming eth messages for transactions.
///    - serving transaction requests.
///    - propagate transactions
///
/// This type communicates with the [`NetworkManager`](crate::NetworkManager) in both directions.
///   - receives incoming network messages.
///   - sends messages to dispatch (responses, propagate tx)
///
/// It is directly connected to the [`TransactionPool`] to retrieve requested transactions and
/// propagate new transactions over the network.
///
/// It can be configured with different policies for transaction propagation and announcement
/// filtering. See [`NetworkPolicies`] and [`TransactionPolicies`] for more details.
///
/// ## Network Transaction Processing
///
/// ### Message Types
///
/// - **`Transactions`**: Full transaction broadcasts (rejects blob transactions)
/// - **`NewPooledTransactionHashes`**: Hash announcements
///
/// ### Peer Tracking
///
/// - Maintains per-peer transaction cache (default: 10,240 entries)
/// - Prevents duplicate imports and enables efficient propagation
///
/// ### Bad Transaction Handling
///
/// Caches and rejects transactions with consensus violations (gas, signature, chain ID).
/// Penalizes peers sending invalid transactions.
///
/// ### Import Management
///
/// Limits concurrent pool imports and backs off when approaching capacity.
///
/// ### Transaction Fetching
///
/// For announced transactions: filters known → queues unknown → fetches → imports
///
/// ### Propagation Rules
///
/// Based on: origin (Local/External/Private), peer capabilities, and network state.
/// Disabled during initial sync.
///
/// ### Security
///
/// Rate limiting via reputation, bad transaction isolation, peer scoring.
#[derive(Debug)]
#[must_use = "Manager does nothing unless polled."]
pub struct TransactionsManager<
    Pool,
    N: NetworkPrimitives = EthNetworkPrimitives,
    PBundle: TransactionPolicies = NetworkPolicies<
        TransactionPropagationKind,
        StrictEthAnnouncementFilter,
    >,
> {
    /// Access to the transaction pool.
    pool: Pool,
    /// Network access.
    network: NetworkHandle<N>,
    /// Subscriptions to all network related events.
    ///
    /// From which we get all new incoming transaction related messages.
    network_events: EventStream<NetworkEvent<PeerRequest<N>>>,
    /// Transaction fetcher to handle inflight and missing transaction requests.
    transaction_fetcher: TransactionFetcher<N>,
    /// All currently pending transactions grouped by peers.
    ///
    /// This way we can track incoming transactions and prevent multiple pool imports for the same
    /// transaction
    transactions_by_peers: HashMap<TxHash, HashSet<PeerId>>,
    /// Transactions that are currently imported into the `Pool`.
    ///
    /// The import process includes:
    ///  - validation of the transactions, e.g. transaction is well formed: valid tx type, fees are
    ///    valid, or for 4844 transaction the blobs are valid. See also
    ///    [`EthTransactionValidator`](reth_transaction_pool::validate::EthTransactionValidator)
    /// - if the transaction is valid, it is added into the pool.
    ///
    /// Once the new transaction reaches the __pending__ state it will be emitted by the pool via
    /// [`TransactionPool::pending_transactions_listener`] and arrive at the `pending_transactions`
    /// receiver.
    pool_imports: FuturesUnordered<PoolImportFuture>,
    /// Stats on pending pool imports that help the node self-monitor.
    pending_pool_imports_info: PendingPoolImportsInfo,
    /// Bad imports.
    bad_imports: LruCache<TxHash>,
    /// All the connected peers.
    peers: HashMap<PeerId, PeerMetadata<N>>,
    /// Send half for the command channel.
    ///
    /// This is kept so that a new [`TransactionsHandle`] can be created at any time.
    command_tx: mpsc::UnboundedSender<TransactionsCommand<N>>,
    /// Incoming commands from [`TransactionsHandle`].
    ///
    /// This will only receive commands if a user manually sends a command to the manager through
    /// the [`TransactionsHandle`] to interact with this type directly.
    command_rx: UnboundedReceiverStream<TransactionsCommand<N>>,
    /// A stream that yields new __pending__ transactions.
    ///
    /// A transaction is considered __pending__ if it is executable on the current state of the
    /// chain. In other words, this only yields transactions that satisfy all consensus
    /// requirements, these include:
    ///   - no nonce gaps
    ///   - all dynamic fee requirements are (currently) met
    ///   - account has enough balance to cover the transaction's gas
    pending_transactions: ReceiverStream<TxHash>,
    /// Incoming events from the [`NetworkManager`](crate::NetworkManager).
    transaction_events: UnboundedMeteredReceiver<NetworkTransactionEvent<N>>,
    /// How the `TransactionsManager` is configured.
    config: TransactionsManagerConfig,
    /// Network Policies
    policies: PBundle,
    /// `TransactionsManager` metrics
    metrics: TransactionsManagerMetrics,
    /// `AnnouncedTxTypes` metrics
    announced_tx_types_metrics: AnnouncedTxTypesMetrics,
}

impl<Pool: TransactionPool, N: NetworkPrimitives>
    TransactionsManager<
        Pool,
        N,
        NetworkPolicies<TransactionPropagationKind, StrictEthAnnouncementFilter>,
    >
{
    /// Sets up a new instance.
    ///
    /// Note: This expects an existing [`NetworkManager`](crate::NetworkManager) instance.
    pub fn new(
        network: NetworkHandle<N>,
        pool: Pool,
        from_network: mpsc::UnboundedReceiver<NetworkTransactionEvent<N>>,
        transactions_manager_config: TransactionsManagerConfig,
    ) -> Self {
        Self::with_policy(
            network,
            pool,
            from_network,
            transactions_manager_config,
            NetworkPolicies::default(),
        )
    }
}

impl<Pool: TransactionPool, N: NetworkPrimitives, PBundle: TransactionPolicies>
    TransactionsManager<Pool, N, PBundle>
{
    /// Sets up a new instance with given the settings.
    ///
    /// Note: This expects an existing [`NetworkManager`](crate::NetworkManager) instance.
    pub fn with_policy(
        network: NetworkHandle<N>,
        pool: Pool,
        from_network: mpsc::UnboundedReceiver<NetworkTransactionEvent<N>>,
        transactions_manager_config: TransactionsManagerConfig,
        policies: PBundle,
    ) -> Self {
        let network_events = network.event_listener();

        let (command_tx, command_rx) = mpsc::unbounded_channel();

        let transaction_fetcher = TransactionFetcher::with_transaction_fetcher_config(
            &transactions_manager_config.transaction_fetcher_config,
        );

        // install a listener for new __pending__ transactions that are allowed to be propagated
        // over the network
        let pending = pool.pending_transactions_listener();
        let pending_pool_imports_info = PendingPoolImportsInfo::default();
        let metrics = TransactionsManagerMetrics::default();
        metrics
            .capacity_pending_pool_imports
            .increment(pending_pool_imports_info.max_pending_pool_imports as u64);

        Self {
            pool,
            network,
            network_events,
            transaction_fetcher,
            transactions_by_peers: Default::default(),
            pool_imports: Default::default(),
            pending_pool_imports_info: PendingPoolImportsInfo::new(
                DEFAULT_MAX_COUNT_PENDING_POOL_IMPORTS,
            ),
            bad_imports: LruCache::new(DEFAULT_MAX_COUNT_BAD_IMPORTS),
            peers: Default::default(),
            command_tx,
            command_rx: UnboundedReceiverStream::new(command_rx),
            pending_transactions: ReceiverStream::new(pending),
            transaction_events: UnboundedMeteredReceiver::new(
                from_network,
                NETWORK_POOL_TRANSACTIONS_SCOPE,
            ),
            config: transactions_manager_config,
            policies,
            metrics,
            announced_tx_types_metrics: AnnouncedTxTypesMetrics::default(),
        }
    }

    /// Returns a new handle that can send commands to this type.
    pub fn handle(&self) -> TransactionsHandle<N> {
        TransactionsHandle { manager_tx: self.command_tx.clone() }
    }

    /// Returns `true` if [`TransactionsManager`] has capacity to request pending hashes. Returns
    /// `false` if [`TransactionsManager`] is operating close to full capacity.
    fn has_capacity_for_fetching_pending_hashes(&self) -> bool {
        self.pending_pool_imports_info
            .has_capacity(self.pending_pool_imports_info.max_pending_pool_imports) &&
            self.transaction_fetcher.has_capacity_for_fetching_pending_hashes()
    }

    fn report_peer_bad_transactions(&self, peer_id: PeerId) {
        self.report_peer(peer_id, ReputationChangeKind::BadTransactions);
        self.metrics.reported_bad_transactions.increment(1);
    }

    fn report_peer(&self, peer_id: PeerId, kind: ReputationChangeKind) {
        trace!(target: "net::tx", ?peer_id, ?kind, "reporting reputation change");
        self.network.reputation_change(peer_id, kind);
    }

    fn report_already_seen(&self, peer_id: PeerId) {
        trace!(target: "net::tx", ?peer_id, "Penalizing peer for already seen transaction");
        self.network.reputation_change(peer_id, ReputationChangeKind::AlreadySeenTransaction);
    }

    /// Clear the transaction
    fn on_good_import(&mut self, hash: TxHash) {
        self.transactions_by_peers.remove(&hash);
    }

    /// Penalize the peers that intentionally sent the bad transaction, and cache it to avoid
    /// fetching or importing it again.
    ///
    /// Errors that count as bad transactions are:
    ///
    /// - intrinsic gas too low
    /// - exceeds gas limit
    /// - gas uint overflow
    /// - exceeds max init code size
    /// - oversized data
    /// - signer account has bytecode
    /// - chain id mismatch
    /// - old legacy chain id
    /// - tx type not supported
    ///
    /// (and additionally for blobs txns...)
    ///
    /// - no blobs
    /// - too many blobs
    /// - invalid kzg proof
    /// - kzg error
    /// - not blob transaction (tx type mismatch)
    /// - wrong versioned kzg commitment hash
    fn on_bad_import(&mut self, err: PoolError) {
        let peers = self.transactions_by_peers.remove(&err.hash);

        // if we're _currently_ syncing, we ignore a bad transaction
        if !err.is_bad_transaction() || self.network.is_syncing() {
            return
        }
        // otherwise we penalize the peer that sent the bad transaction, with the assumption that
        // the peer should have known that this transaction is bad (e.g. violating consensus rules)
        if let Some(peers) = peers {
            for peer_id in peers {
                self.report_peer_bad_transactions(peer_id);
            }
        }
        self.metrics.bad_imports.increment(1);
        self.bad_imports.insert(err.hash);
    }

    /// Runs an operation to fetch hashes that are cached in [`TransactionFetcher`].
    fn on_fetch_hashes_pending_fetch(&mut self) {
        // try drain transaction hashes pending fetch
        let info = &self.pending_pool_imports_info;
        let max_pending_pool_imports = info.max_pending_pool_imports;
        let has_capacity_wrt_pending_pool_imports =
            |divisor| info.has_capacity(max_pending_pool_imports / divisor);

        self.transaction_fetcher
            .on_fetch_pending_hashes(&self.peers, has_capacity_wrt_pending_pool_imports);
    }

    fn on_request_error(&self, peer_id: PeerId, req_err: RequestError) {
        let kind = match req_err {
            RequestError::UnsupportedCapability => ReputationChangeKind::BadProtocol,
            RequestError::Timeout => ReputationChangeKind::Timeout,
            RequestError::ChannelClosed | RequestError::ConnectionDropped => {
                // peer is already disconnected
                return
            }
            RequestError::BadResponse => return self.report_peer_bad_transactions(peer_id),
        };
        self.report_peer(peer_id, kind);
    }

    #[inline]
    fn update_poll_metrics(&self, start: Instant, poll_durations: TxManagerPollDurations) {
        let metrics = &self.metrics;

        let TxManagerPollDurations {
            acc_network_events,
            acc_pending_imports,
            acc_tx_events,
            acc_imported_txns,
            acc_fetch_events,
            acc_pending_fetch,
            acc_cmds,
        } = poll_durations;

        // update metrics for whole poll function
        metrics.duration_poll_tx_manager.set(start.elapsed().as_secs_f64());
        // update metrics for nested expressions
        metrics.acc_duration_poll_network_events.set(acc_network_events.as_secs_f64());
        metrics.acc_duration_poll_pending_pool_imports.set(acc_pending_imports.as_secs_f64());
        metrics.acc_duration_poll_transaction_events.set(acc_tx_events.as_secs_f64());
        metrics.acc_duration_poll_imported_transactions.set(acc_imported_txns.as_secs_f64());
        metrics.acc_duration_poll_fetch_events.set(acc_fetch_events.as_secs_f64());
        metrics.acc_duration_fetch_pending_hashes.set(acc_pending_fetch.as_secs_f64());
        metrics.acc_duration_poll_commands.set(acc_cmds.as_secs_f64());
    }
}

impl<Pool: TransactionPool, N: NetworkPrimitives, PBundle: TransactionPolicies>
    TransactionsManager<Pool, N, PBundle>
{
    /// Processes a batch import results.
    fn on_batch_import_result(&mut self, batch_results: Vec<PoolResult<AddedTransactionOutcome>>) {
        for res in batch_results {
            match res {
                Ok(AddedTransactionOutcome { hash, .. }) => {
                    self.on_good_import(hash);
                }
                Err(err) => {
                    self.on_bad_import(err);
                }
            }
        }
    }

    /// Request handler for an incoming `NewPooledTransactionHashes`
    fn on_new_pooled_transaction_hashes(
        &mut self,
        peer_id: PeerId,
        msg: NewPooledTransactionHashes,
    ) {
        // If the node is initially syncing, ignore transactions
        if self.network.is_initially_syncing() {
            return
        }
        if self.network.tx_gossip_disabled() {
            return
        }

        // get handle to peer's session, if the session is still active
        let Some(peer) = self.peers.get_mut(&peer_id) else {
            trace!(
                peer_id = format!("{peer_id:#}"),
                ?msg,
                "discarding announcement from inactive peer"
            );

            return
        };
        let client = peer.client_version.clone();

        // keep track of the transactions the peer knows
        let mut count_txns_already_seen_by_peer = 0;
        for tx in msg.iter_hashes().copied() {
            if !peer.seen_transactions.insert(tx) {
                count_txns_already_seen_by_peer += 1;
            }
        }
        if count_txns_already_seen_by_peer > 0 {
            // this may occur if transactions are sent or announced to a peer, at the same time as
            // the peer sends/announces those hashes to us. this is because, marking
            // txns as seen by a peer is done optimistically upon sending them to the
            // peer.
            self.metrics.messages_with_hashes_already_seen_by_peer.increment(1);
            self.metrics
                .occurrences_hash_already_seen_by_peer
                .increment(count_txns_already_seen_by_peer);

            trace!(target: "net::tx",
                %count_txns_already_seen_by_peer,
                peer_id=format!("{peer_id:#}"),
                ?client,
                "Peer sent hashes that have already been marked as seen by peer"
            );

            self.report_already_seen(peer_id);
        }

        // 1. filter out spam
        if msg.is_empty() {
            self.report_peer(peer_id, ReputationChangeKind::BadAnnouncement);
            return;
        }

        let original_len = msg.len();
        let mut partially_valid_msg = msg.dedup();

        if partially_valid_msg.len() != original_len {
            self.report_peer(peer_id, ReputationChangeKind::BadAnnouncement);
        }

        // 2. filter out transactions pending import to pool
        partially_valid_msg.retain_by_hash(|hash| !self.transactions_by_peers.contains_key(hash));

        // 3. filter out known hashes
        //
        // known txns have already been successfully fetched or received over gossip.
        //
        // most hashes will be filtered out here since this the mempool protocol is a gossip
        // protocol, healthy peers will send many of the same hashes.
        //
        let hashes_count_pre_pool_filter = partially_valid_msg.len();
        self.pool.retain_unknown(&mut partially_valid_msg);
        if hashes_count_pre_pool_filter > partially_valid_msg.len() {
            let already_known_hashes_count =
                hashes_count_pre_pool_filter - partially_valid_msg.len();
            self.metrics
                .occurrences_hashes_already_in_pool
                .increment(already_known_hashes_count as u64);
        }

        if partially_valid_msg.is_empty() {
            // nothing to request
            return
        }

        // 4. filter out invalid entries (spam)
        //
        // validates messages with respect to the given network, e.g. allowed tx types
        //
        let mut should_report_peer = false;
        let mut tx_types_counter = TxTypesCounter::default();

        let is_eth68_message = partially_valid_msg
            .msg_version()
            .expect("partially valid announcement should have a version")
            .is_eth68();

        partially_valid_msg.retain(|tx_hash, metadata_ref_mut| {
            let (ty_byte, size_val) = match *metadata_ref_mut {
                Some((ty, size)) => {
                    if !is_eth68_message {
                        should_report_peer = true;
                    }
                    (ty, size)
                }
                None => {
                    if is_eth68_message {
                        should_report_peer = true;
                        return false;
                    }
                    (0u8, 0)
                }
            };

            if is_eth68_message {
                if let Some((actual_ty_byte, _)) = *metadata_ref_mut {
                    if let Ok(parsed_tx_type) = TxType::try_from(actual_ty_byte) {
                        tx_types_counter.increase_by_tx_type(parsed_tx_type);
                    }
                }
            }

            let decision = self
                .policies
                .announcement_filter()
                .decide_on_announcement(ty_byte, tx_hash, size_val);

            match decision {
                AnnouncementAcceptance::Accept => true,
                AnnouncementAcceptance::Ignore => false,
                AnnouncementAcceptance::Reject { penalize_peer } => {
                    if penalize_peer {
                        should_report_peer = true;
                    }
                    false
                }
            }
        });

        if is_eth68_message {
            self.announced_tx_types_metrics.update_eth68_announcement_metrics(tx_types_counter);
        }

        if should_report_peer {
            self.report_peer(peer_id, ReputationChangeKind::BadAnnouncement);
        }

        let mut valid_announcement_data =
            ValidAnnouncementData::from_partially_valid_data(partially_valid_msg);

        if valid_announcement_data.is_empty() {
            // no valid announcement data
            return
        }

        // 5. filter out already seen unknown hashes
        //
        // seen hashes are already in the tx fetcher, pending fetch.
        //
        // for any seen hashes add the peer as fallback. unseen hashes are loaded into the tx
        // fetcher, hence they should be valid at this point.
        let bad_imports = &self.bad_imports;
        self.transaction_fetcher.filter_unseen_and_pending_hashes(
            &mut valid_announcement_data,
            |hash| bad_imports.contains(hash),
            &peer_id,
            &client,
        );

        if valid_announcement_data.is_empty() {
            // nothing to request
            return
        }

        trace!(target: "net::tx::propagation",
            peer_id=format!("{peer_id:#}"),
            hashes_len=valid_announcement_data.iter().count(),
            hashes=?valid_announcement_data.keys().collect::<Vec<_>>(),
            msg_version=%valid_announcement_data.msg_version(),
            client_version=%client,
            "received previously unseen and pending hashes in announcement from peer"
        );

        // only send request for hashes to idle peer, otherwise buffer hashes storing peer as
        // fallback
        if !self.transaction_fetcher.is_idle(&peer_id) {
            // load message version before announcement data is destructed in packing
            let msg_version = valid_announcement_data.msg_version();
            let (hashes, _version) = valid_announcement_data.into_request_hashes();

            trace!(target: "net::tx",
                peer_id=format!("{peer_id:#}"),
                hashes=?*hashes,
                %msg_version,
                %client,
                "buffering hashes announced by busy peer"
            );

            self.transaction_fetcher.buffer_hashes(hashes, Some(peer_id));

            return
        }

        let mut hashes_to_request =
            RequestTxHashes::with_capacity(valid_announcement_data.len() / 4);
        let surplus_hashes =
            self.transaction_fetcher.pack_request(&mut hashes_to_request, valid_announcement_data);

        if !surplus_hashes.is_empty() {
            trace!(target: "net::tx",
                peer_id=format!("{peer_id:#}"),
                surplus_hashes=?*surplus_hashes,
                %client,
                "some hashes in announcement from peer didn't fit in `GetPooledTransactions` request, buffering surplus hashes"
            );

            self.transaction_fetcher.buffer_hashes(surplus_hashes, Some(peer_id));
        }

        trace!(target: "net::tx",
            peer_id=format!("{peer_id:#}"),
            hashes=?*hashes_to_request,
            %client,
            "sending hashes in `GetPooledTransactions` request to peer's session"
        );

        // request the missing transactions
        //
        // get handle to peer's session again, at this point we know it exists
        let Some(peer) = self.peers.get_mut(&peer_id) else { return };
        if let Some(failed_to_request_hashes) =
            self.transaction_fetcher.request_transactions_from_peer(hashes_to_request, peer)
        {
            let conn_eth_version = peer.version;

            trace!(target: "net::tx",
                peer_id=format!("{peer_id:#}"),
                failed_to_request_hashes=?*failed_to_request_hashes,
                %conn_eth_version,
                %client,
                "sending `GetPooledTransactions` request to peer's session failed, buffering hashes"
            );
            self.transaction_fetcher.buffer_hashes(failed_to_request_hashes, Some(peer_id));
        }
    }
}

impl<Pool, N, PBundle> TransactionsManager<Pool, N, PBundle>
where
    Pool: TransactionPool + Unpin + 'static,

    N: NetworkPrimitives<
            BroadcastedTransaction: SignedTransaction,
            PooledTransaction: SignedTransaction,
        > + Unpin,

    PBundle: TransactionPolicies,
    Pool::Transaction:
        PoolTransaction<Consensus = N::BroadcastedTransaction, Pooled = N::PooledTransaction>,
{
    /// Invoked when transactions in the local mempool are considered __pending__.
    ///
    /// When a transaction in the local mempool is moved to the pending pool, we propagate them to
    /// connected peers over network using the `Transactions` and `NewPooledTransactionHashes`
    /// messages. The Transactions message relays complete transaction objects and is typically
    /// sent to a small, random fraction of connected peers.
    ///
    /// All other peers receive a notification of the transaction hash and can request the
    /// complete transaction object if it is unknown to them. The dissemination of complete
    /// transactions to a fraction of peers usually ensures that all nodes receive the transaction
    /// and won't need to request it.
    fn on_new_pending_transactions(&mut self, hashes: Vec<TxHash>) {
        // Nothing to propagate while initially syncing
        if self.network.is_initially_syncing() {
            return
        }
        if self.network.tx_gossip_disabled() {
            return
        }

        trace!(target: "net::tx", num_hashes=?hashes.len(), "Start propagating transactions");

        self.propagate_all(hashes);
    }

    /// Propagate the full transactions to a specific peer.
    ///
    /// Returns the propagated transactions.
    fn propagate_full_transactions_to_peer(
        &mut self,
        txs: Vec<TxHash>,
        peer_id: PeerId,
        propagation_mode: PropagationMode,
    ) -> Option<PropagatedTransactions> {
        trace!(target: "net::tx", ?peer_id, "Propagating transactions to peer");

        let peer = self.peers.get_mut(&peer_id)?;
        let mut propagated = PropagatedTransactions::default();

        // filter all transactions unknown to the peer
        let mut full_transactions = FullTransactionsBuilder::new(peer.version);

        let to_propagate = self.pool.get_all(txs).into_iter().map(PropagateTransaction::pool_tx);

        if propagation_mode.is_forced() {
            // skip cache check if forced
            full_transactions.extend(to_propagate);
        } else {
            // Iterate through the transactions to propagate and fill the hashes and full
            // transaction
            for tx in to_propagate {
                if !peer.seen_transactions.contains(tx.tx_hash()) {
                    // Only include if the peer hasn't seen the transaction
                    full_transactions.push(&tx);
                }
            }
        }

        if full_transactions.is_empty() {
            // nothing to propagate
            return None
        }

        let PropagateTransactions { pooled, full } = full_transactions.build();

        // send hashes if any
        if let Some(new_pooled_hashes) = pooled {
            for hash in new_pooled_hashes.iter_hashes().copied() {
                propagated.0.entry(hash).or_default().push(PropagateKind::Hash(peer_id));
                // mark transaction as seen by peer
                peer.seen_transactions.insert(hash);
            }

            // send hashes of transactions
            self.network.send_transactions_hashes(peer_id, new_pooled_hashes);
        }

        // send full transactions, if any
        if let Some(new_full_transactions) = full {
            for tx in &new_full_transactions {
                propagated.0.entry(*tx.tx_hash()).or_default().push(PropagateKind::Full(peer_id));
                // mark transaction as seen by peer
                peer.seen_transactions.insert(*tx.tx_hash());
            }

            // send full transactions
            self.network.send_transactions(peer_id, new_full_transactions);
        }

        // Update propagated transactions metrics
        self.metrics.propagated_transactions.increment(propagated.0.len() as u64);

        Some(propagated)
    }

    /// Propagate the transaction hashes to the given peer
    ///
    /// Note: This will only send the hashes for transactions that exist in the pool.
    fn propagate_hashes_to(
        &mut self,
        hashes: Vec<TxHash>,
        peer_id: PeerId,
        propagation_mode: PropagationMode,
    ) {
        trace!(target: "net::tx", "Start propagating transactions as hashes");

        // This fetches a transactions from the pool, including the blob transactions, which are
        // only ever sent as hashes.
        let propagated = {
            let Some(peer) = self.peers.get_mut(&peer_id) else {
                // no such peer
                return
            };

            let to_propagate = self
                .pool
                .get_all(hashes)
                .into_iter()
                .map(PropagateTransaction::pool_tx)
                .collect::<Vec<_>>();

            let mut propagated = PropagatedTransactions::default();

            // check if transaction is known to peer
            let mut hashes = PooledTransactionsHashesBuilder::new(peer.version);

            if propagation_mode.is_forced() {
                hashes.extend(to_propagate)
            } else {
                for tx in to_propagate {
                    if !peer.seen_transactions.contains(tx.tx_hash()) {
                        // Include if the peer hasn't seen it
                        hashes.push(&tx);
                    }
                }
            }

            let new_pooled_hashes = hashes.build();

            if new_pooled_hashes.is_empty() {
                // nothing to propagate
                return
            }

            for hash in new_pooled_hashes.iter_hashes().copied() {
                propagated.0.entry(hash).or_default().push(PropagateKind::Hash(peer_id));
            }

            trace!(target: "net::tx::propagation", ?peer_id, ?new_pooled_hashes, "Propagating transactions to peer");

            // send hashes of transactions
            self.network.send_transactions_hashes(peer_id, new_pooled_hashes);

            // Update propagated transactions metrics
            self.metrics.propagated_transactions.increment(propagated.0.len() as u64);

            propagated
        };

        // notify pool so events get fired
        self.pool.on_propagated(propagated);
    }

    /// Propagate the transactions to all connected peers either as full objects or hashes.
    ///
    /// The message for new pooled hashes depends on the negotiated version of the stream.
    /// See [`NewPooledTransactionHashes`]
    ///
    /// Note: EIP-4844 are disallowed from being broadcast in full and are only ever sent as hashes, see also <https://eips.ethereum.org/EIPS/eip-4844#networking>.
    fn propagate_transactions(
        &mut self,
        to_propagate: Vec<PropagateTransaction<N::BroadcastedTransaction>>,
        propagation_mode: PropagationMode,
    ) -> PropagatedTransactions {
        let mut propagated = PropagatedTransactions::default();
        if self.network.tx_gossip_disabled() {
            return propagated
        }

        // send full transactions to a set of the connected peers based on the configured mode
        let max_num_full = self.config.propagation_mode.full_peer_count(self.peers.len());

        // Note: Assuming ~random~ order due to random state of the peers map hasher
        for (peer_idx, (peer_id, peer)) in self.peers.iter_mut().enumerate() {
            if !self.policies.propagation_policy().can_propagate(peer) {
                // skip peers we should not propagate to
                continue
            }
            // determine whether to send full tx objects or hashes.
            let mut builder = if peer_idx > max_num_full {
                PropagateTransactionsBuilder::pooled(peer.version)
            } else {
                PropagateTransactionsBuilder::full(peer.version)
            };

            if propagation_mode.is_forced() {
                builder.extend(to_propagate.iter());
            } else {
                // Iterate through the transactions to propagate and fill the hashes and full
                // transaction lists, before deciding whether or not to send full transactions to
                // the peer.
                for tx in &to_propagate {
                    // Only proceed if the transaction is not in the peer's list of seen
                    // transactions
                    if !peer.seen_transactions.contains(tx.tx_hash()) {
                        builder.push(tx);
                    }
                }
            }

            if builder.is_empty() {
                trace!(target: "net::tx", ?peer_id, "Nothing to propagate to peer; has seen all transactions");
                continue
            }

            let PropagateTransactions { pooled, full } = builder.build();

            // send hashes if any
            if let Some(mut new_pooled_hashes) = pooled {
                // enforce tx soft limit per message for the (unlikely) event the number of
                // hashes exceeds it
                new_pooled_hashes
                    .truncate(SOFT_LIMIT_COUNT_HASHES_IN_NEW_POOLED_TRANSACTIONS_BROADCAST_MESSAGE);

                for hash in new_pooled_hashes.iter_hashes().copied() {
                    propagated.0.entry(hash).or_default().push(PropagateKind::Hash(*peer_id));
                    // mark transaction as seen by peer
                    peer.seen_transactions.insert(hash);
                }

                trace!(target: "net::tx", ?peer_id, num_txs=?new_pooled_hashes.len(), "Propagating tx hashes to peer");

                // send hashes of transactions
                self.network.send_transactions_hashes(*peer_id, new_pooled_hashes);
            }

            // send full transactions, if any
            if let Some(new_full_transactions) = full {
                for tx in &new_full_transactions {
                    propagated
                        .0
                        .entry(*tx.tx_hash())
                        .or_default()
                        .push(PropagateKind::Full(*peer_id));
                    // mark transaction as seen by peer
                    peer.seen_transactions.insert(*tx.tx_hash());
                }

                trace!(target: "net::tx", ?peer_id, num_txs=?new_full_transactions.len(), "Propagating full transactions to peer");

                // send full transactions
                self.network.send_transactions(*peer_id, new_full_transactions);
            }
        }

        // Update propagated transactions metrics
        self.metrics.propagated_transactions.increment(propagated.0.len() as u64);

        propagated
    }

    /// Propagates the given transactions to the peers
    ///
    /// This fetches all transaction from the pool, including the 4844 blob transactions but
    /// __without__ their sidecar, because 4844 transactions are only ever announced as hashes.
    fn propagate_all(&mut self, hashes: Vec<TxHash>) {
        if self.peers.is_empty() {
            // nothing to propagate
            return
        }
        let propagated = self.propagate_transactions(
            self.pool.get_all(hashes).into_iter().map(PropagateTransaction::pool_tx).collect(),
            PropagationMode::Basic,
        );

        // notify pool so events get fired
        self.pool.on_propagated(propagated);
    }

    /// Request handler for an incoming request for transactions
    fn on_get_pooled_transactions(
        &mut self,
        peer_id: PeerId,
        request: GetPooledTransactions,
        response: oneshot::Sender<RequestResult<PooledTransactions<N::PooledTransaction>>>,
    ) {
        if let Some(peer) = self.peers.get_mut(&peer_id) {
            if self.network.tx_gossip_disabled() {
                let _ = response.send(Ok(PooledTransactions::default()));
                return
            }
            let transactions = self.pool.get_pooled_transaction_elements(
                request.0,
                GetPooledTransactionLimit::ResponseSizeSoftLimit(
                    self.transaction_fetcher.info.soft_limit_byte_size_pooled_transactions_response,
                ),
            );
            trace!(target: "net::tx::propagation", sent_txs=?transactions.iter().map(|tx| tx.tx_hash()), "Sending requested transactions to peer");

            // we sent a response at which point we assume that the peer is aware of the
            // transactions
            peer.seen_transactions.extend(transactions.iter().map(|tx| *tx.tx_hash()));

            let resp = PooledTransactions(transactions);
            let _ = response.send(Ok(resp));
        }
    }

    /// Handles a command received from a detached [`TransactionsHandle`]
    fn on_command(&mut self, cmd: TransactionsCommand<N>) {
        match cmd {
            TransactionsCommand::PropagateHash(hash) => {
                self.on_new_pending_transactions(vec![hash])
            }
            TransactionsCommand::PropagateHashesTo(hashes, peer) => {
                self.propagate_hashes_to(hashes, peer, PropagationMode::Forced)
            }
            TransactionsCommand::GetActivePeers(tx) => {
                let peers = self.peers.keys().copied().collect::<HashSet<_>>();
                tx.send(peers).ok();
            }
            TransactionsCommand::PropagateTransactionsTo(txs, peer) => {
                if let Some(propagated) =
                    self.propagate_full_transactions_to_peer(txs, peer, PropagationMode::Forced)
                {
                    self.pool.on_propagated(propagated);
                }
            }
            TransactionsCommand::PropagateTransactions(txs) => self.propagate_all(txs),
            TransactionsCommand::BroadcastTransactions(txs) => {
                let propagated = self.propagate_transactions(txs, PropagationMode::Forced);
                self.pool.on_propagated(propagated);
            }
            TransactionsCommand::GetTransactionHashes { peers, tx } => {
                let mut res = HashMap::with_capacity(peers.len());
                for peer_id in peers {
                    let hashes = self
                        .peers
                        .get(&peer_id)
                        .map(|peer| peer.seen_transactions.iter().copied().collect::<HashSet<_>>())
                        .unwrap_or_default();
                    res.insert(peer_id, hashes);
                }
                tx.send(res).ok();
            }
            TransactionsCommand::GetPeerSender { peer_id, peer_request_sender } => {
                let sender = self.peers.get(&peer_id).map(|peer| peer.request_tx.clone());
                peer_request_sender.send(sender).ok();
            }
        }
    }

    /// Handles session establishment and peer transactions initialization.
    ///
    /// This is invoked when a new session is established.
    fn handle_peer_session(
        &mut self,
        info: SessionInfo,
        messages: PeerRequestSender<PeerRequest<N>>,
    ) {
        let SessionInfo { peer_id, client_version, version, .. } = info;

        // Insert a new peer into the peerset.
        let peer = PeerMetadata::<N>::new(
            messages,
            version,
            client_version,
            self.config.max_transactions_seen_by_peer_history,
            info.peer_kind,
        );
        let peer = match self.peers.entry(peer_id) {
            Entry::Occupied(mut entry) => {
                entry.insert(peer);
                entry.into_mut()
            }
            Entry::Vacant(entry) => entry.insert(peer),
        };

        self.policies.propagation_policy_mut().on_session_established(peer);

        // Send a `NewPooledTransactionHashes` to the peer with up to
        // `SOFT_LIMIT_COUNT_HASHES_IN_NEW_POOLED_TRANSACTIONS_BROADCAST_MESSAGE`
        // transactions in the pool.
        if self.network.is_initially_syncing() || self.network.tx_gossip_disabled() {
            trace!(target: "net::tx", ?peer_id, "Skipping transaction broadcast: node syncing or gossip disabled");
            return
        }

        // Get transactions to broadcast
        let pooled_txs = self.pool.pooled_transactions_max(
            SOFT_LIMIT_COUNT_HASHES_IN_NEW_POOLED_TRANSACTIONS_BROADCAST_MESSAGE,
        );
        if pooled_txs.is_empty() {
            trace!(target: "net::tx", ?peer_id, "No transactions in the pool to broadcast");
            return;
        }

        // Build and send transaction hashes message
        let mut msg_builder = PooledTransactionsHashesBuilder::new(version);
        for pooled_tx in pooled_txs {
            peer.seen_transactions.insert(*pooled_tx.hash());
            msg_builder.push_pooled(pooled_tx);
        }

        debug!(target: "net::tx", ?peer_id, tx_count = msg_builder.is_empty(), "Broadcasting transaction hashes");
        let msg = msg_builder.build();
        self.network.send_transactions_hashes(peer_id, msg);
    }

    /// Handles a received event related to common network events.
    fn on_network_event(&mut self, event_result: NetworkEvent<PeerRequest<N>>) {
        match event_result {
            NetworkEvent::Peer(PeerEvent::SessionClosed { peer_id, .. }) => {
                // remove the peer

                let peer = self.peers.remove(&peer_id);
                if let Some(mut peer) = peer {
                    self.policies.propagation_policy_mut().on_session_closed(&mut peer);
                }
                self.transaction_fetcher.remove_peer(&peer_id);
            }
            NetworkEvent::ActivePeerSession { info, messages } => {
                // process active peer session and broadcast available transaction from the pool
                self.handle_peer_session(info, messages);
            }
            NetworkEvent::Peer(PeerEvent::SessionEstablished(info)) => {
                let peer_id = info.peer_id;
                // get messages from existing peer
                let messages = match self.peers.get(&peer_id) {
                    Some(p) => p.request_tx.clone(),
                    None => {
                        debug!(target: "net::tx", ?peer_id, "No peer request sender found");
                        return;
                    }
                };
                self.handle_peer_session(info, messages);
            }
            _ => {}
        }
    }

    /// Handles dedicated transaction events related to the `eth` protocol.
    fn on_network_tx_event(&mut self, event: NetworkTransactionEvent<N>) {
        match event {
            NetworkTransactionEvent::IncomingTransactions { peer_id, msg } => {
                // ensure we didn't receive any blob transactions as these are disallowed to be
                // broadcasted in full

                let has_blob_txs = msg.has_eip4844();

                let non_blob_txs = msg
                    .0
                    .into_iter()
                    .map(N::PooledTransaction::try_from)
                    .filter_map(Result::ok)
                    .collect();

                self.import_transactions(peer_id, non_blob_txs, TransactionSource::Broadcast);

                if has_blob_txs {
                    debug!(target: "net::tx", ?peer_id, "received bad full blob transaction broadcast");
                    self.report_peer_bad_transactions(peer_id);
                }
            }
            NetworkTransactionEvent::IncomingPooledTransactionHashes { peer_id, msg } => {
                self.on_new_pooled_transaction_hashes(peer_id, msg)
            }
            NetworkTransactionEvent::GetPooledTransactions { peer_id, request, response } => {
                self.on_get_pooled_transactions(peer_id, request, response)
            }
            NetworkTransactionEvent::GetTransactionsHandle(response) => {
                let _ = response.send(Some(self.handle()));
            }
        }
    }

    /// Starts the import process for the given transactions.
    fn import_transactions(
        &mut self,
        peer_id: PeerId,
        transactions: PooledTransactions<N::PooledTransaction>,
        source: TransactionSource,
    ) {
        // If the node is pipeline syncing, ignore transactions
        if self.network.is_initially_syncing() {
            return
        }
        if self.network.tx_gossip_disabled() {
            return
        }

        let Some(peer) = self.peers.get_mut(&peer_id) else { return };
        let mut transactions = transactions.0;

        // mark the transactions as received
        self.transaction_fetcher
            .remove_hashes_from_transaction_fetcher(transactions.iter().map(|tx| *tx.tx_hash()));

        // track that the peer knows these transaction, but only if this is a new broadcast.
        // If we received the transactions as the response to our `GetPooledTransactions``
        // requests (based on received `NewPooledTransactionHashes`) then we already
        // recorded the hashes as seen by this peer in `Self::on_new_pooled_transaction_hashes`.
        let mut num_already_seen_by_peer = 0;
        for tx in &transactions {
            if source.is_broadcast() && !peer.seen_transactions.insert(*tx.tx_hash()) {
                num_already_seen_by_peer += 1;
            }
        }

        // 1. filter out txns already inserted into pool
        let txns_count_pre_pool_filter = transactions.len();
        self.pool.retain_unknown(&mut transactions);
        if txns_count_pre_pool_filter > transactions.len() {
            let already_known_txns_count = txns_count_pre_pool_filter - transactions.len();
            self.metrics
                .occurrences_transactions_already_in_pool
                .increment(already_known_txns_count as u64);
        }

        // tracks the quality of the given transactions
        let mut has_bad_transactions = false;

        // 2. filter out transactions that are invalid or already pending import pre-size to avoid
        //    reallocations
        let mut new_txs = Vec::with_capacity(transactions.len());
        for tx in transactions {
            // recover transaction
            let tx = match tx.try_into_recovered() {
                Ok(tx) => tx,
                Err(badtx) => {
                    trace!(target: "net::tx",
                        peer_id=format!("{peer_id:#}"),
                        hash=%badtx.tx_hash(),
                        client_version=%peer.client_version,
                        "failed ecrecovery for transaction"
                    );
                    has_bad_transactions = true;
                    continue
                }
            };

            match self.transactions_by_peers.entry(*tx.tx_hash()) {
                Entry::Occupied(mut entry) => {
                    // transaction was already inserted
                    entry.get_mut().insert(peer_id);
                }
                Entry::Vacant(entry) => {
                    if self.bad_imports.contains(tx.tx_hash()) {
                        trace!(target: "net::tx",
                            peer_id=format!("{peer_id:#}"),
                            hash=%tx.tx_hash(),
                            client_version=%peer.client_version,
                            "received a known bad transaction from peer"
                        );
                        has_bad_transactions = true;
                    } else {
                        // this is a new transaction that should be imported into the pool

                        let pool_transaction = Pool::Transaction::from_pooled(tx);
                        new_txs.push(pool_transaction);

                        entry.insert(HashSet::from([peer_id]));
                    }
                }
            }
        }
        new_txs.shrink_to_fit();

        // 3. import new transactions as a batch to minimize lock contention on the underlying
        // pool
        if !new_txs.is_empty() {
            let pool = self.pool.clone();
            // update metrics
            let metric_pending_pool_imports = self.metrics.pending_pool_imports.clone();
            metric_pending_pool_imports.increment(new_txs.len() as f64);

            // update self-monitoring info
            self.pending_pool_imports_info
                .pending_pool_imports
                .fetch_add(new_txs.len(), Ordering::Relaxed);
            let tx_manager_info_pending_pool_imports =
                self.pending_pool_imports_info.pending_pool_imports.clone();

            trace!(target: "net::tx::propagation", new_txs_len=?new_txs.len(), "Importing new transactions");
            let import = Box::pin(async move {
                let added = new_txs.len();
                let res = pool.add_external_transactions(new_txs).await;

                // update metrics
                metric_pending_pool_imports.decrement(added as f64);
                // update self-monitoring info
                tx_manager_info_pending_pool_imports.fetch_sub(added, Ordering::Relaxed);

                res
            });

            self.pool_imports.push(import);
        }

        if num_already_seen_by_peer > 0 {
            self.metrics.messages_with_transactions_already_seen_by_peer.increment(1);
            self.metrics
                .occurrences_of_transaction_already_seen_by_peer
                .increment(num_already_seen_by_peer);
            trace!(target: "net::tx", num_txs=%num_already_seen_by_peer, ?peer_id, client=?peer.client_version, "Peer sent already seen transactions");
        }

        if has_bad_transactions {
            // peer sent us invalid transactions
            self.report_peer_bad_transactions(peer_id)
        }

        if num_already_seen_by_peer > 0 {
            self.report_already_seen(peer_id);
        }
    }

    /// Processes a [`FetchEvent`].
    fn on_fetch_event(&mut self, fetch_event: FetchEvent<N::PooledTransaction>) {
        match fetch_event {
            FetchEvent::TransactionsFetched { peer_id, transactions, report_peer } => {
                self.import_transactions(peer_id, transactions, TransactionSource::Response);
                if report_peer {
                    self.report_peer(peer_id, ReputationChangeKind::BadTransactions);
                }
            }
            FetchEvent::FetchError { peer_id, error } => {
                trace!(target: "net::tx", ?peer_id, %error, "requesting transactions from peer failed");
                self.on_request_error(peer_id, error);
            }
            FetchEvent::EmptyResponse { peer_id } => {
                trace!(target: "net::tx", ?peer_id, "peer returned empty response");
            }
        }
    }
}

/// An endless future. Preemption ensure that future is non-blocking, nonetheless. See
/// [`crate::NetworkManager`] for more context on the design pattern.
///
/// This should be spawned or used as part of `tokio::select!`.
//
// spawned in `NodeConfig::start_network`(reth_node_core::NodeConfig) and
// `NetworkConfig::start_network`(reth_network::NetworkConfig)
impl<
        Pool: TransactionPool + Unpin + 'static,
        N: NetworkPrimitives<
                BroadcastedTransaction: SignedTransaction,
                PooledTransaction: SignedTransaction,
            > + Unpin,
        PBundle: TransactionPolicies + Unpin,
    > Future for TransactionsManager<Pool, N, PBundle>
where
    Pool::Transaction:
        PoolTransaction<Consensus = N::BroadcastedTransaction, Pooled = N::PooledTransaction>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let start = Instant::now();
        let mut poll_durations = TxManagerPollDurations::default();

        let this = self.get_mut();

        // All streams are polled until their corresponding budget is exhausted, then we manually
        // yield back control to tokio. See `NetworkManager` for more context on the design
        // pattern.

        // Advance network/peer related events (update peers map).
        let maybe_more_network_events = metered_poll_nested_stream_with_budget!(
            poll_durations.acc_network_events,
            "net::tx",
            "Network events stream",
            DEFAULT_BUDGET_TRY_DRAIN_STREAM,
            this.network_events.poll_next_unpin(cx),
            |event| this.on_network_event(event)
        );

        // Advances new __pending__ transactions, transactions that were successfully inserted into
        // pending set in pool (are valid), and propagates them (inform peers which
        // transactions we have seen).
        //
        // We try to drain this to batch the transactions in a single message.
        //
        // We don't expect this buffer to be large, since only pending transactions are
        // emitted here.
        let mut new_txs = Vec::new();
        let maybe_more_pending_txns = metered_poll_nested_stream_with_budget!(
            poll_durations.acc_imported_txns,
            "net::tx",
            "Pending transactions stream",
            DEFAULT_BUDGET_TRY_DRAIN_POOL_IMPORTS,
            this.pending_transactions.poll_next_unpin(cx),
            |hash| new_txs.push(hash)
        );
        if !new_txs.is_empty() {
            this.on_new_pending_transactions(new_txs);
        }

        // Advance inflight fetch requests (flush transaction fetcher and queue for
        // import to pool).
        //
        // The smallest decodable transaction is an empty legacy transaction, 10 bytes
        // (2 MiB / 10 bytes > 200k transactions).
        //
        // Since transactions aren't validated until they are inserted into the pool,
        // this can potentially queue >200k transactions for insertion to pool. More
        // if the message size is bigger than the soft limit on a `PooledTransactions`
        // response which is 2 MiB.
        let maybe_more_tx_fetch_events = metered_poll_nested_stream_with_budget!(
            poll_durations.acc_fetch_events,
            "net::tx",
            "Transaction fetch events stream",
            DEFAULT_BUDGET_TRY_DRAIN_STREAM,
            this.transaction_fetcher.poll_next_unpin(cx),
            |event| this.on_fetch_event(event),
        );

        // Advance incoming transaction events (stream new txns/announcements from
        // network manager and queue for import to pool/fetch txns).
        //
        // This will potentially remove hashes from hashes pending fetch, it the event
        // is an announcement (if same hashes are announced that didn't fit into a
        // previous request).
        //
        // The smallest decodable transaction is an empty legacy transaction, 10 bytes
        // (128 KiB / 10 bytes > 13k transactions).
        //
        // If this is an event with `Transactions` message, since transactions aren't
        // validated until they are inserted into the pool, this can potentially queue
        // >13k transactions for insertion to pool. More if the message size is bigger
        // than the soft limit on a `Transactions` broadcast message, which is 128 KiB.
        let maybe_more_tx_events = metered_poll_nested_stream_with_budget!(
            poll_durations.acc_tx_events,
            "net::tx",
            "Network transaction events stream",
            DEFAULT_BUDGET_TRY_DRAIN_NETWORK_TRANSACTION_EVENTS,
            this.transaction_events.poll_next_unpin(cx),
            |event| this.on_network_tx_event(event),
        );

        // Advance pool imports (flush txns to pool).
        //
        // Note, this is done in batches. A batch is filled from one `Transactions`
        // broadcast messages or one `PooledTransactions` response at a time. The
        // minimum batch size is 1 transaction (and might often be the case with blob
        // transactions).
        //
        // The smallest decodable transaction is an empty legacy transaction, 10 bytes
        // (2 MiB / 10 bytes > 200k transactions).
        //
        // Since transactions aren't validated until they are inserted into the pool,
        // this can potentially validate >200k transactions. More if the message size
        // is bigger than the soft limit on a `PooledTransactions` response which is
        // 2 MiB (`Transactions` broadcast messages is smaller, 128 KiB).
        let maybe_more_pool_imports = metered_poll_nested_stream_with_budget!(
            poll_durations.acc_pending_imports,
            "net::tx",
            "Batched pool imports stream",
            DEFAULT_BUDGET_TRY_DRAIN_PENDING_POOL_IMPORTS,
            this.pool_imports.poll_next_unpin(cx),
            |batch_results| this.on_batch_import_result(batch_results)
        );

        // Tries to drain hashes pending fetch cache if the tx manager currently has
        // capacity for this (fetch txns).
        //
        // Sends at most one request.
        duration_metered_exec!(
            {
                if this.has_capacity_for_fetching_pending_hashes() {
                    this.on_fetch_hashes_pending_fetch();
                }
            },
            poll_durations.acc_pending_fetch
        );

        // Advance commands (propagate/fetch/serve txns).
        let maybe_more_commands = metered_poll_nested_stream_with_budget!(
            poll_durations.acc_cmds,
            "net::tx",
            "Commands channel",
            DEFAULT_BUDGET_TRY_DRAIN_STREAM,
            this.command_rx.poll_next_unpin(cx),
            |cmd| this.on_command(cmd)
        );

        this.transaction_fetcher.update_metrics();

        // all channels are fully drained and import futures pending
        if maybe_more_network_events ||
            maybe_more_commands ||
            maybe_more_tx_events ||
            maybe_more_tx_fetch_events ||
            maybe_more_pool_imports ||
            maybe_more_pending_txns
        {
            // make sure we're woken up again
            cx.waker().wake_by_ref();
            return Poll::Pending
        }

        this.update_poll_metrics(start, poll_durations);

        Poll::Pending
    }
}

/// Represents the different modes of transaction propagation.
///
/// This enum is used to determine how transactions are propagated to peers in the network.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum PropagationMode {
    /// Default propagation mode.
    ///
    /// Transactions are only sent to peers that haven't seen them yet.
    Basic,
    /// Forced propagation mode.
    ///
    /// Transactions are sent to all peers regardless of whether they have been sent or received
    /// before.
    Forced,
}

impl PropagationMode {
    /// Returns `true` if the propagation kind is `Forced`.
    const fn is_forced(self) -> bool {
        matches!(self, Self::Forced)
    }
}

/// A transaction that's about to be propagated to multiple peers.
#[derive(Debug, Clone)]
struct PropagateTransaction<T = TransactionSigned> {
    size: usize,
    transaction: Arc<T>,
}

impl<T: SignedTransaction> PropagateTransaction<T> {
    /// Create a new instance from a transaction.
    pub fn new(transaction: T) -> Self {
        let size = transaction.length();
        Self { size, transaction: Arc::new(transaction) }
    }

    /// Create a new instance from a pooled transaction
    fn pool_tx<P>(tx: Arc<ValidPoolTransaction<P>>) -> Self
    where
        P: PoolTransaction<Consensus = T>,
    {
        let size = tx.encoded_length();
        let transaction = tx.transaction.clone_into_consensus();
        let transaction = Arc::new(transaction.into_inner());
        Self { size, transaction }
    }

    fn tx_hash(&self) -> &TxHash {
        self.transaction.tx_hash()
    }
}

/// Helper type to construct the appropriate message to send to the peer based on whether the peer
/// should receive them in full or as pooled
#[derive(Debug, Clone)]
enum PropagateTransactionsBuilder<T> {
    Pooled(PooledTransactionsHashesBuilder),
    Full(FullTransactionsBuilder<T>),
}

impl<T> PropagateTransactionsBuilder<T> {
    /// Create a builder for pooled transactions
    fn pooled(version: EthVersion) -> Self {
        Self::Pooled(PooledTransactionsHashesBuilder::new(version))
    }

    /// Create a builder that sends transactions in full and records transactions that don't fit.
    fn full(version: EthVersion) -> Self {
        Self::Full(FullTransactionsBuilder::new(version))
    }

    /// Returns true if no transactions are recorded.
    fn is_empty(&self) -> bool {
        match self {
            Self::Pooled(builder) => builder.is_empty(),
            Self::Full(builder) => builder.is_empty(),
        }
    }

    /// Consumes the type and returns the built messages that should be sent to the peer.
    fn build(self) -> PropagateTransactions<T> {
        match self {
            Self::Pooled(pooled) => {
                PropagateTransactions { pooled: Some(pooled.build()), full: None }
            }
            Self::Full(full) => full.build(),
        }
    }
}

impl<T: SignedTransaction> PropagateTransactionsBuilder<T> {
    /// Appends all transactions
    fn extend<'a>(&mut self, txs: impl IntoIterator<Item = &'a PropagateTransaction<T>>) {
        for tx in txs {
            self.push(tx);
        }
    }

    /// Appends a transaction to the list.
    fn push(&mut self, transaction: &PropagateTransaction<T>) {
        match self {
            Self::Pooled(builder) => builder.push(transaction),
            Self::Full(builder) => builder.push(transaction),
        }
    }
}

/// Represents how the transactions should be sent to a peer if any.
struct PropagateTransactions<T> {
    /// The pooled transaction hashes to send.
    pooled: Option<NewPooledTransactionHashes>,
    /// The transactions to send in full.
    full: Option<Vec<Arc<T>>>,
}

/// Helper type for constructing the full transaction message that enforces the
/// [`DEFAULT_SOFT_LIMIT_BYTE_SIZE_TRANSACTIONS_BROADCAST_MESSAGE`] for full transaction broadcast
/// and enforces other propagation rules for EIP-4844 and tracks those transactions that can't be
/// broadcasted in full.
#[derive(Debug, Clone)]
struct FullTransactionsBuilder<T> {
    /// The soft limit to enforce for a single broadcast message of full transactions.
    total_size: usize,
    /// All transactions to be broadcasted.
    transactions: Vec<Arc<T>>,
    /// Transactions that didn't fit into the broadcast message
    pooled: PooledTransactionsHashesBuilder,
}

impl<T> FullTransactionsBuilder<T> {
    /// Create a builder for the negotiated version of the peer's session
    fn new(version: EthVersion) -> Self {
        Self {
            total_size: 0,
            pooled: PooledTransactionsHashesBuilder::new(version),
            transactions: vec![],
        }
    }

    /// Returns whether or not any transactions are in the [`FullTransactionsBuilder`].
    fn is_empty(&self) -> bool {
        self.transactions.is_empty() && self.pooled.is_empty()
    }

    /// Returns the messages that should be propagated to the peer.
    fn build(self) -> PropagateTransactions<T> {
        let pooled = Some(self.pooled.build()).filter(|pooled| !pooled.is_empty());
        let full = Some(self.transactions).filter(|full| !full.is_empty());
        PropagateTransactions { pooled, full }
    }
}

impl<T: SignedTransaction> FullTransactionsBuilder<T> {
    /// Appends all transactions.
    fn extend(&mut self, txs: impl IntoIterator<Item = PropagateTransaction<T>>) {
        for tx in txs {
            self.push(&tx)
        }
    }

    /// Append a transaction to the list of full transaction if the total message bytes size doesn't
    /// exceed the soft maximum target byte size. The limit is soft, meaning if one single
    /// transaction goes over the limit, it will be broadcasted in its own [`Transactions`]
    /// message. The same pattern is followed in filling a [`GetPooledTransactions`] request in
    /// [`TransactionFetcher::fill_request_from_hashes_pending_fetch`].
    ///
    /// If the transaction is unsuitable for broadcast or would exceed the softlimit, it is appended
    /// to list of pooled transactions, (e.g. 4844 transactions).
    /// See also [`SignedTransaction::is_broadcastable_in_full`].
    fn push(&mut self, transaction: &PropagateTransaction<T>) {
        // Do not send full 4844 transaction hashes to peers.
        //
        //  Nodes MUST NOT automatically broadcast blob transactions to their peers.
        //  Instead, those transactions are only announced using
        //  `NewPooledTransactionHashes` messages, and can then be manually requested
        //  via `GetPooledTransactions`.
        //
        // From: <https://eips.ethereum.org/EIPS/eip-4844#networking>
        if !transaction.transaction.is_broadcastable_in_full() {
            self.pooled.push(transaction);
            return
        }

        let new_size = self.total_size + transaction.size;
        if new_size > DEFAULT_SOFT_LIMIT_BYTE_SIZE_TRANSACTIONS_BROADCAST_MESSAGE &&
            self.total_size > 0
        {
            // transaction does not fit into the message
            self.pooled.push(transaction);
            return
        }

        self.total_size = new_size;
        self.transactions.push(Arc::clone(&transaction.transaction));
    }
}

/// A helper type to create the pooled transactions message based on the negotiated version of the
/// session with the peer
#[derive(Debug, Clone)]
enum PooledTransactionsHashesBuilder {
    Eth66(NewPooledTransactionHashes66),
    Eth68(NewPooledTransactionHashes68),
}

// === impl PooledTransactionsHashesBuilder ===

impl PooledTransactionsHashesBuilder {
    /// Push a transaction from the pool to the list.
    fn push_pooled<T: PoolTransaction>(&mut self, pooled_tx: Arc<ValidPoolTransaction<T>>) {
        match self {
            Self::Eth66(msg) => msg.0.push(*pooled_tx.hash()),
            Self::Eth68(msg) => {
                msg.hashes.push(*pooled_tx.hash());
                msg.sizes.push(pooled_tx.encoded_length());
                msg.types.push(pooled_tx.transaction.ty());
            }
        }
    }

    /// Returns whether or not any transactions are in the [`PooledTransactionsHashesBuilder`].
    fn is_empty(&self) -> bool {
        match self {
            Self::Eth66(hashes) => hashes.is_empty(),
            Self::Eth68(hashes) => hashes.is_empty(),
        }
    }

    /// Appends all hashes
    fn extend<T: SignedTransaction>(
        &mut self,
        txs: impl IntoIterator<Item = PropagateTransaction<T>>,
    ) {
        for tx in txs {
            self.push(&tx);
        }
    }

    fn push<T: SignedTransaction>(&mut self, tx: &PropagateTransaction<T>) {
        match self {
            Self::Eth66(msg) => msg.0.push(*tx.tx_hash()),
            Self::Eth68(msg) => {
                msg.hashes.push(*tx.tx_hash());
                msg.sizes.push(tx.size);
                msg.types.push(tx.transaction.ty());
            }
        }
    }

    /// Create a builder for the negotiated version of the peer's session
    fn new(version: EthVersion) -> Self {
        match version {
            EthVersion::Eth66 | EthVersion::Eth67 => Self::Eth66(Default::default()),
            EthVersion::Eth68 | EthVersion::Eth69 => Self::Eth68(Default::default()),
        }
    }

    fn build(self) -> NewPooledTransactionHashes {
        match self {
            Self::Eth66(msg) => msg.into(),
            Self::Eth68(msg) => msg.into(),
        }
    }
}

/// How we received the transactions.
enum TransactionSource {
    /// Transactions were broadcast to us via [`Transactions`] message.
    Broadcast,
    /// Transactions were sent as the response of [`fetcher::GetPooledTxRequest`] issued by us.
    Response,
}

// === impl TransactionSource ===

impl TransactionSource {
    /// Whether the transaction were sent as broadcast.
    const fn is_broadcast(&self) -> bool {
        matches!(self, Self::Broadcast)
    }
}

/// Tracks a single peer in the context of [`TransactionsManager`].
#[derive(Debug)]
pub struct PeerMetadata<N: NetworkPrimitives = EthNetworkPrimitives> {
    /// Optimistically keeps track of transactions that we know the peer has seen. Optimistic, in
    /// the sense that transactions are preemptively marked as seen by peer when they are sent to
    /// the peer.
    seen_transactions: LruCache<TxHash>,
    /// A communication channel directly to the peer's session task.
    request_tx: PeerRequestSender<PeerRequest<N>>,
    /// negotiated version of the session.
    version: EthVersion,
    /// The peer's client version.
    client_version: Arc<str>,
    /// The kind of peer.
    peer_kind: PeerKind,
}

impl<N: NetworkPrimitives> PeerMetadata<N> {
    /// Returns a new instance of [`PeerMetadata`].
    pub fn new(
        request_tx: PeerRequestSender<PeerRequest<N>>,
        version: EthVersion,
        client_version: Arc<str>,
        max_transactions_seen_by_peer: u32,
        peer_kind: PeerKind,
    ) -> Self {
        Self {
            seen_transactions: LruCache::new(max_transactions_seen_by_peer),
            request_tx,
            version,
            client_version,
            peer_kind,
        }
    }

    /// Returns a reference to the peer's request sender channel.
    pub const fn request_tx(&self) -> &PeerRequestSender<PeerRequest<N>> {
        &self.request_tx
    }

    /// Return a
    pub const fn seen_transactions_mut(&mut self) -> &mut LruCache<TxHash> {
        &mut self.seen_transactions
    }

    /// Returns the negotiated `EthVersion` of the session.
    pub const fn version(&self) -> EthVersion {
        self.version
    }

    /// Returns a reference to the peer's client version string.
    pub fn client_version(&self) -> &str {
        &self.client_version
    }

    /// Returns the peer's kind.
    pub const fn peer_kind(&self) -> PeerKind {
        self.peer_kind
    }
}

/// Commands to send to the [`TransactionsManager`]
#[derive(Debug)]
enum TransactionsCommand<N: NetworkPrimitives = EthNetworkPrimitives> {
    /// Propagate a transaction hash to the network.
    PropagateHash(B256),
    /// Propagate transaction hashes to a specific peer.
    PropagateHashesTo(Vec<B256>, PeerId),
    /// Request the list of active peer IDs from the [`TransactionsManager`].
    GetActivePeers(oneshot::Sender<HashSet<PeerId>>),
    /// Propagate a collection of full transactions to a specific peer.
    PropagateTransactionsTo(Vec<TxHash>, PeerId),
    /// Propagate a collection of hashes to all peers.
    PropagateTransactions(Vec<TxHash>),
    /// Propagate a collection of broadcastable transactions in full to all peers.
    BroadcastTransactions(Vec<PropagateTransaction<N::BroadcastedTransaction>>),
    /// Request transaction hashes known by specific peers from the [`TransactionsManager`].
    GetTransactionHashes {
        peers: Vec<PeerId>,
        tx: oneshot::Sender<HashMap<PeerId, HashSet<TxHash>>>,
    },
    /// Requests a clone of the sender channel to the peer.
    GetPeerSender {
        peer_id: PeerId,
        peer_request_sender: oneshot::Sender<Option<PeerRequestSender<PeerRequest<N>>>>,
    },
}

/// All events related to transactions emitted by the network.
#[derive(Debug)]
pub enum NetworkTransactionEvent<N: NetworkPrimitives = EthNetworkPrimitives> {
    /// Represents the event of receiving a list of transactions from a peer.
    ///
    /// This indicates transactions that were broadcasted to us from the peer.
    IncomingTransactions {
        /// The ID of the peer from which the transactions were received.
        peer_id: PeerId,
        /// The received transactions.
        msg: Transactions<N::BroadcastedTransaction>,
    },
    /// Represents the event of receiving a list of transaction hashes from a peer.
    IncomingPooledTransactionHashes {
        /// The ID of the peer from which the transaction hashes were received.
        peer_id: PeerId,
        /// The received new pooled transaction hashes.
        msg: NewPooledTransactionHashes,
    },
    /// Represents the event of receiving a `GetPooledTransactions` request from a peer.
    GetPooledTransactions {
        /// The ID of the peer from which the request was received.
        peer_id: PeerId,
        /// The received `GetPooledTransactions` request.
        request: GetPooledTransactions,
        /// The sender for responding to the request with a result of `PooledTransactions`.
        response: oneshot::Sender<RequestResult<PooledTransactions<N::PooledTransaction>>>,
    },
    /// Represents the event of receiving a `GetTransactionsHandle` request.
    GetTransactionsHandle(oneshot::Sender<Option<TransactionsHandle<N>>>),
}

/// Tracks stats about the [`TransactionsManager`].
#[derive(Debug)]
pub struct PendingPoolImportsInfo {
    /// Number of transactions about to be inserted into the pool.
    pending_pool_imports: Arc<AtomicUsize>,
    /// Max number of transactions allowed to be imported concurrently.
    max_pending_pool_imports: usize,
}

impl PendingPoolImportsInfo {
    /// Returns a new [`PendingPoolImportsInfo`].
    pub fn new(max_pending_pool_imports: usize) -> Self {
        Self { pending_pool_imports: Arc::new(AtomicUsize::default()), max_pending_pool_imports }
    }

    /// Returns `true` if the number of pool imports is under a given tolerated max.
    pub fn has_capacity(&self, max_pending_pool_imports: usize) -> bool {
        self.pending_pool_imports.load(Ordering::Relaxed) < max_pending_pool_imports
    }
}

impl Default for PendingPoolImportsInfo {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_COUNT_PENDING_POOL_IMPORTS)
    }
}

#[derive(Debug, Default)]
struct TxManagerPollDurations {
    acc_network_events: Duration,
    acc_pending_imports: Duration,
    acc_tx_events: Duration,
    acc_imported_txns: Duration,
    acc_fetch_events: Duration,
    acc_pending_fetch: Duration,
    acc_cmds: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_utils::{
            transactions::{buffer_hash_to_tx_fetcher, new_mock_session, new_tx_manager},
            Testnet,
        },
        transactions::config::RelaxedEthAnnouncementFilter,
        NetworkConfigBuilder, NetworkManager,
    };
    use alloy_consensus::{TxEip1559, TxLegacy};
    use alloy_primitives::{hex, Signature, TxKind, U256};
    use alloy_rlp::Decodable;
    use futures::FutureExt;
    use reth_chainspec::MIN_TRANSACTION_GAS;
    use reth_ethereum_primitives::{PooledTransactionVariant, Transaction, TransactionSigned};
    use reth_network_api::{NetworkInfo, PeerKind};
    use reth_network_p2p::{
        error::{RequestError, RequestResult},
        sync::{NetworkSyncUpdater, SyncState},
    };
    use reth_storage_api::noop::NoopProvider;
    use reth_transaction_pool::test_utils::{
        testing_pool, MockTransaction, MockTransactionFactory, TestPool,
    };
    use secp256k1::SecretKey;
    use std::{
        future::poll_fn,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        str::FromStr,
    };
    use tracing::error;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_ignored_tx_broadcasts_while_initially_syncing() {
        reth_tracing::init_test_tracing();
        let net = Testnet::create(3).await;

        let mut handles = net.handles();
        let handle0 = handles.next().unwrap();
        let handle1 = handles.next().unwrap();

        drop(handles);
        let handle = net.spawn();

        let listener0 = handle0.event_listener();
        handle0.add_peer(*handle1.peer_id(), handle1.local_addr());
        let secret_key = SecretKey::new(&mut rand_08::thread_rng());

        let client = NoopProvider::default();
        let pool = testing_pool();
        let config = NetworkConfigBuilder::eth(secret_key)
            .disable_discovery()
            .listener_port(0)
            .build(client);
        let transactions_manager_config = config.transactions_manager_config.clone();
        let (network_handle, network, mut transactions, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone(), transactions_manager_config)
            .split_with_handle();

        tokio::task::spawn(network);

        // go to syncing (pipeline sync)
        network_handle.update_sync_state(SyncState::Syncing);
        assert!(NetworkInfo::is_syncing(&network_handle));
        assert!(NetworkInfo::is_initially_syncing(&network_handle));

        // wait for all initiator connections
        let mut established = listener0.take(2);
        while let Some(ev) = established.next().await {
            match ev {
                NetworkEvent::Peer(PeerEvent::SessionEstablished(info)) => {
                    // to insert a new peer in transactions peerset
                    transactions
                        .on_network_event(NetworkEvent::Peer(PeerEvent::SessionEstablished(info)))
                }
                NetworkEvent::Peer(PeerEvent::PeerAdded(_peer_id)) => {}
                ev => {
                    error!("unexpected event {ev:?}")
                }
            }
        }
        // random tx: <https://etherscan.io/getRawTx?tx=0x9448608d36e721ef403c53b00546068a6474d6cbab6816c3926de449898e7bce>
        let input = hex!(
            "02f871018302a90f808504890aef60826b6c94ddf4c5025d1a5742cf12f74eec246d4432c295e487e09c3bbcc12b2b80c080a0f21a4eacd0bf8fea9c5105c543be5a1d8c796516875710fafafdf16d16d8ee23a001280915021bb446d1973501a67f93d2b38894a514b976e7b46dc2fe54598d76"
        );
        let signed_tx = TransactionSigned::decode(&mut &input[..]).unwrap();
        transactions.on_network_tx_event(NetworkTransactionEvent::IncomingTransactions {
            peer_id: *handle1.peer_id(),
            msg: Transactions(vec![signed_tx.clone()]),
        });
        poll_fn(|cx| {
            let _ = transactions.poll_unpin(cx);
            Poll::Ready(())
        })
        .await;
        assert!(pool.is_empty());
        handle.terminate().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_tx_broadcasts_through_two_syncs() {
        reth_tracing::init_test_tracing();
        let net = Testnet::create(3).await;

        let mut handles = net.handles();
        let handle0 = handles.next().unwrap();
        let handle1 = handles.next().unwrap();

        drop(handles);
        let handle = net.spawn();

        let listener0 = handle0.event_listener();
        handle0.add_peer(*handle1.peer_id(), handle1.local_addr());
        let secret_key = SecretKey::new(&mut rand_08::thread_rng());

        let client = NoopProvider::default();
        let pool = testing_pool();
        let config = NetworkConfigBuilder::new(secret_key)
            .disable_discovery()
            .listener_port(0)
            .build(client);
        let transactions_manager_config = config.transactions_manager_config.clone();
        let (network_handle, network, mut transactions, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone(), transactions_manager_config)
            .split_with_handle();

        tokio::task::spawn(network);

        // go to syncing (pipeline sync) to idle and then to syncing (live)
        network_handle.update_sync_state(SyncState::Syncing);
        assert!(NetworkInfo::is_syncing(&network_handle));
        network_handle.update_sync_state(SyncState::Idle);
        assert!(!NetworkInfo::is_syncing(&network_handle));
        network_handle.update_sync_state(SyncState::Syncing);
        assert!(NetworkInfo::is_syncing(&network_handle));

        // wait for all initiator connections
        let mut established = listener0.take(2);
        while let Some(ev) = established.next().await {
            match ev {
                NetworkEvent::ActivePeerSession { .. } |
                NetworkEvent::Peer(PeerEvent::SessionEstablished(_)) => {
                    // to insert a new peer in transactions peerset
                    transactions.on_network_event(ev);
                }
                NetworkEvent::Peer(PeerEvent::PeerAdded(_peer_id)) => {}
                _ => {
                    error!("unexpected event {ev:?}")
                }
            }
        }
        // random tx: <https://etherscan.io/getRawTx?tx=0x9448608d36e721ef403c53b00546068a6474d6cbab6816c3926de449898e7bce>
        let input = hex!(
            "02f871018302a90f808504890aef60826b6c94ddf4c5025d1a5742cf12f74eec246d4432c295e487e09c3bbcc12b2b80c080a0f21a4eacd0bf8fea9c5105c543be5a1d8c796516875710fafafdf16d16d8ee23a001280915021bb446d1973501a67f93d2b38894a514b976e7b46dc2fe54598d76"
        );
        let signed_tx = TransactionSigned::decode(&mut &input[..]).unwrap();
        transactions.on_network_tx_event(NetworkTransactionEvent::IncomingTransactions {
            peer_id: *handle1.peer_id(),
            msg: Transactions(vec![signed_tx.clone()]),
        });
        poll_fn(|cx| {
            let _ = transactions.poll_unpin(cx);
            Poll::Ready(())
        })
        .await;
        assert!(!NetworkInfo::is_initially_syncing(&network_handle));
        assert!(NetworkInfo::is_syncing(&network_handle));
        assert!(!pool.is_empty());
        handle.terminate().await;
    }

    // Ensure that the transaction manager correctly handles the `IncomingPooledTransactionHashes`
    // event and is able to retrieve the corresponding transactions.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_handle_incoming_transactions_hashes() {
        reth_tracing::init_test_tracing();

        let secret_key = SecretKey::new(&mut rand_08::thread_rng());
        let client = NoopProvider::default();

        let config = NetworkConfigBuilder::new(secret_key)
            // let OS choose port
            .listener_port(0)
            .disable_discovery()
            .build(client);

        let pool = testing_pool();

        let transactions_manager_config = config.transactions_manager_config.clone();
        let (_network_handle, _network, mut tx_manager, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone(), transactions_manager_config)
            .split_with_handle();

        let peer_id_1 = PeerId::new([1; 64]);
        let eth_version = EthVersion::Eth66;

        let txs = vec![TransactionSigned::new_unhashed(
            Transaction::Legacy(TxLegacy {
                chain_id: Some(4),
                nonce: 15u64,
                gas_price: 2200000000,
                gas_limit: 34811,
                to: TxKind::Call(hex!("cf7f9e66af820a19257a2108375b180b0ec49167").into()),
                value: U256::from(1234u64),
                input: Default::default(),
            }),
            Signature::new(
                U256::from_str(
                    "0x35b7bfeb9ad9ece2cbafaaf8e202e706b4cfaeb233f46198f00b44d4a566a981",
                )
                .unwrap(),
                U256::from_str(
                    "0x612638fb29427ca33b9a3be2a0a561beecfe0269655be160d35e72d366a6a860",
                )
                .unwrap(),
                true,
            ),
        )];

        let txs_hashes: Vec<B256> = txs.iter().map(|tx| *tx.hash()).collect();

        let (peer_1, mut to_mock_session_rx) = new_mock_session(peer_id_1, eth_version);
        tx_manager.peers.insert(peer_id_1, peer_1);

        assert!(pool.is_empty());

        tx_manager.on_network_tx_event(NetworkTransactionEvent::IncomingPooledTransactionHashes {
            peer_id: peer_id_1,
            msg: NewPooledTransactionHashes::from(NewPooledTransactionHashes66::from(
                txs_hashes.clone(),
            )),
        });

        // mock session of peer_1 receives request
        let req = to_mock_session_rx
            .recv()
            .await
            .expect("peer_1 session should receive request with buffered hashes");
        let PeerRequest::GetPooledTransactions { request, response } = req else { unreachable!() };
        assert_eq!(request, GetPooledTransactions::from(txs_hashes.clone()));

        let message: Vec<PooledTransactionVariant> = txs
            .into_iter()
            .map(|tx| {
                PooledTransactionVariant::try_from(tx)
                    .expect("Failed to convert MockTransaction to PooledTransaction")
            })
            .collect();

        // return the transactions corresponding to the transaction hashes.
        response
            .send(Ok(PooledTransactions(message)))
            .expect("should send peer_1 response to tx manager");

        // adance the transaction manager future
        poll_fn(|cx| {
            let _ = tx_manager.poll_unpin(cx);
            Poll::Ready(())
        })
        .await;

        // ensure that the transactions corresponding to the transaction hashes have been
        // successfully retrieved and stored in the Pool.
        assert_eq!(pool.get_all(txs_hashes.clone()).len(), txs_hashes.len());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_handle_incoming_transactions() {
        reth_tracing::init_test_tracing();
        let net = Testnet::create(3).await;

        let mut handles = net.handles();
        let handle0 = handles.next().unwrap();
        let handle1 = handles.next().unwrap();

        drop(handles);
        let handle = net.spawn();

        let listener0 = handle0.event_listener();

        handle0.add_peer(*handle1.peer_id(), handle1.local_addr());
        let secret_key = SecretKey::new(&mut rand_08::thread_rng());

        let client = NoopProvider::default();
        let pool = testing_pool();
        let config = NetworkConfigBuilder::new(secret_key)
            .disable_discovery()
            .listener_port(0)
            .build(client);
        let transactions_manager_config = config.transactions_manager_config.clone();
        let (network_handle, network, mut transactions, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone(), transactions_manager_config)
            .split_with_handle();
        tokio::task::spawn(network);

        network_handle.update_sync_state(SyncState::Idle);

        assert!(!NetworkInfo::is_syncing(&network_handle));

        // wait for all initiator connections
        let mut established = listener0.take(2);
        while let Some(ev) = established.next().await {
            match ev {
                NetworkEvent::ActivePeerSession { .. } |
                NetworkEvent::Peer(PeerEvent::SessionEstablished(_)) => {
                    // to insert a new peer in transactions peerset
                    transactions.on_network_event(ev);
                }
                NetworkEvent::Peer(PeerEvent::PeerAdded(_peer_id)) => {}
                ev => {
                    error!("unexpected event {ev:?}")
                }
            }
        }
        // random tx: <https://etherscan.io/getRawTx?tx=0x9448608d36e721ef403c53b00546068a6474d6cbab6816c3926de449898e7bce>
        let input = hex!(
            "02f871018302a90f808504890aef60826b6c94ddf4c5025d1a5742cf12f74eec246d4432c295e487e09c3bbcc12b2b80c080a0f21a4eacd0bf8fea9c5105c543be5a1d8c796516875710fafafdf16d16d8ee23a001280915021bb446d1973501a67f93d2b38894a514b976e7b46dc2fe54598d76"
        );
        let signed_tx = TransactionSigned::decode(&mut &input[..]).unwrap();
        transactions.on_network_tx_event(NetworkTransactionEvent::IncomingTransactions {
            peer_id: *handle1.peer_id(),
            msg: Transactions(vec![signed_tx.clone()]),
        });
        assert!(transactions
            .transactions_by_peers
            .get(signed_tx.tx_hash())
            .unwrap()
            .contains(handle1.peer_id()));

        // advance the transaction manager future
        poll_fn(|cx| {
            let _ = transactions.poll_unpin(cx);
            Poll::Ready(())
        })
        .await;

        assert!(!pool.is_empty());
        assert!(pool.get(signed_tx.tx_hash()).is_some());
        handle.terminate().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_on_get_pooled_transactions_network() {
        reth_tracing::init_test_tracing();
        let net = Testnet::create(2).await;

        let mut handles = net.handles();
        let handle0 = handles.next().unwrap();
        let handle1 = handles.next().unwrap();

        drop(handles);
        let handle = net.spawn();

        let listener0 = handle0.event_listener();

        handle0.add_peer(*handle1.peer_id(), handle1.local_addr());
        let secret_key = SecretKey::new(&mut rand_08::thread_rng());

        let client = NoopProvider::default();
        let pool = testing_pool();
        let config = NetworkConfigBuilder::new(secret_key)
            .disable_discovery()
            .listener_port(0)
            .build(client);
        let transactions_manager_config = config.transactions_manager_config.clone();
        let (network_handle, network, mut transactions, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone(), transactions_manager_config)
            .split_with_handle();
        tokio::task::spawn(network);

        network_handle.update_sync_state(SyncState::Idle);

        assert!(!NetworkInfo::is_syncing(&network_handle));

        // wait for all initiator connections
        let mut established = listener0.take(2);
        while let Some(ev) = established.next().await {
            match ev {
                NetworkEvent::ActivePeerSession { .. } |
                NetworkEvent::Peer(PeerEvent::SessionEstablished(_)) => {
                    transactions.on_network_event(ev);
                }
                NetworkEvent::Peer(PeerEvent::PeerAdded(_peer_id)) => {}
                ev => {
                    error!("unexpected event {ev:?}")
                }
            }
        }
        handle.terminate().await;

        let tx = MockTransaction::eip1559();
        let _ = transactions
            .pool
            .add_transaction(reth_transaction_pool::TransactionOrigin::External, tx.clone())
            .await;

        let request = GetPooledTransactions(vec![*tx.get_hash()]);

        let (send, receive) =
            oneshot::channel::<RequestResult<PooledTransactions<PooledTransactionVariant>>>();

        transactions.on_network_tx_event(NetworkTransactionEvent::GetPooledTransactions {
            peer_id: *handle1.peer_id(),
            request,
            response: send,
        });

        match receive.await.unwrap() {
            Ok(PooledTransactions(transactions)) => {
                assert_eq!(transactions.len(), 1);
            }
            Err(e) => {
                panic!("error: {e:?}");
            }
        }
    }

    // Ensure that when the remote peer only returns part of the requested transactions, the
    // replied transactions are removed from the `tx_fetcher`, while the unresponsive ones are
    // re-buffered.
    #[tokio::test]
    async fn test_partially_tx_response() {
        reth_tracing::init_test_tracing();

        let mut tx_manager = new_tx_manager().await.0;
        let tx_fetcher = &mut tx_manager.transaction_fetcher;

        let peer_id_1 = PeerId::new([1; 64]);
        let eth_version = EthVersion::Eth66;

        let txs = vec![
            TransactionSigned::new_unhashed(
                Transaction::Legacy(TxLegacy {
                    chain_id: Some(4),
                    nonce: 15u64,
                    gas_price: 2200000000,
                    gas_limit: 34811,
                    to: TxKind::Call(hex!("cf7f9e66af820a19257a2108375b180b0ec49167").into()),
                    value: U256::from(1234u64),
                    input: Default::default(),
                }),
                Signature::new(
                    U256::from_str(
                        "0x35b7bfeb9ad9ece2cbafaaf8e202e706b4cfaeb233f46198f00b44d4a566a981",
                    )
                    .unwrap(),
                    U256::from_str(
                        "0x612638fb29427ca33b9a3be2a0a561beecfe0269655be160d35e72d366a6a860",
                    )
                    .unwrap(),
                    true,
                ),
            ),
            TransactionSigned::new_unhashed(
                Transaction::Eip1559(TxEip1559 {
                    chain_id: 4,
                    nonce: 26u64,
                    max_priority_fee_per_gas: 1500000000,
                    max_fee_per_gas: 1500000013,
                    gas_limit: MIN_TRANSACTION_GAS,
                    to: TxKind::Call(hex!("61815774383099e24810ab832a5b2a5425c154d5").into()),
                    value: U256::from(3000000000000000000u64),
                    input: Default::default(),
                    access_list: Default::default(),
                }),
                Signature::new(
                    U256::from_str(
                        "0x59e6b67f48fb32e7e570dfb11e042b5ad2e55e3ce3ce9cd989c7e06e07feeafd",
                    )
                    .unwrap(),
                    U256::from_str(
                        "0x016b83f4f980694ed2eee4d10667242b1f40dc406901b34125b008d334d47469",
                    )
                    .unwrap(),
                    true,
                ),
            ),
        ];

        let txs_hashes: Vec<B256> = txs.iter().map(|tx| *tx.hash()).collect();

        let (mut peer_1, mut to_mock_session_rx) = new_mock_session(peer_id_1, eth_version);
        // mark hashes as seen by peer so it can fish them out from the cache for hashes pending
        // fetch
        peer_1.seen_transactions.insert(txs_hashes[0]);
        peer_1.seen_transactions.insert(txs_hashes[1]);
        tx_manager.peers.insert(peer_id_1, peer_1);

        buffer_hash_to_tx_fetcher(tx_fetcher, txs_hashes[0], peer_id_1, 0, None);
        buffer_hash_to_tx_fetcher(tx_fetcher, txs_hashes[1], peer_id_1, 0, None);

        // peer_1 is idle
        assert!(tx_fetcher.is_idle(&peer_id_1));
        assert_eq!(tx_fetcher.active_peers.len(), 0);

        // sends requests for buffered hashes to peer_1
        tx_fetcher.on_fetch_pending_hashes(&tx_manager.peers, |_| true);

        assert_eq!(tx_fetcher.num_pending_hashes(), 0);
        // as long as request is in flight peer_1 is not idle
        assert!(!tx_fetcher.is_idle(&peer_id_1));
        assert_eq!(tx_fetcher.active_peers.len(), 1);

        // mock session of peer_1 receives request
        let req = to_mock_session_rx
            .recv()
            .await
            .expect("peer_1 session should receive request with buffered hashes");
        let PeerRequest::GetPooledTransactions { response, .. } = req else { unreachable!() };

        let message: Vec<PooledTransactionVariant> = txs
            .into_iter()
            .take(1)
            .map(|tx| {
                PooledTransactionVariant::try_from(tx)
                    .expect("Failed to convert MockTransaction to PooledTransaction")
            })
            .collect();
        // response partial request
        response
            .send(Ok(PooledTransactions(message)))
            .expect("should send peer_1 response to tx manager");
        let Some(FetchEvent::TransactionsFetched { peer_id, .. }) = tx_fetcher.next().await else {
            unreachable!()
        };

        // request has resolved, peer_1 is idle again
        assert!(tx_fetcher.is_idle(&peer_id));
        assert_eq!(tx_fetcher.active_peers.len(), 0);
        // failing peer_1's request buffers requested hashes for retry.
        assert_eq!(tx_fetcher.num_pending_hashes(), 1);
    }

    #[tokio::test]
    async fn test_max_retries_tx_request() {
        reth_tracing::init_test_tracing();

        let mut tx_manager = new_tx_manager().await.0;
        let tx_fetcher = &mut tx_manager.transaction_fetcher;

        let peer_id_1 = PeerId::new([1; 64]);
        let peer_id_2 = PeerId::new([2; 64]);
        let eth_version = EthVersion::Eth66;
        let seen_hashes = [B256::from_slice(&[1; 32]), B256::from_slice(&[2; 32])];

        let (mut peer_1, mut to_mock_session_rx) = new_mock_session(peer_id_1, eth_version);
        // mark hashes as seen by peer so it can fish them out from the cache for hashes pending
        // fetch
        peer_1.seen_transactions.insert(seen_hashes[0]);
        peer_1.seen_transactions.insert(seen_hashes[1]);
        tx_manager.peers.insert(peer_id_1, peer_1);

        // hashes are seen and currently not inflight, with one fallback peer, and are buffered
        // for first retry in reverse order to make index 0 lru
        let retries = 1;
        buffer_hash_to_tx_fetcher(tx_fetcher, seen_hashes[1], peer_id_1, retries, None);
        buffer_hash_to_tx_fetcher(tx_fetcher, seen_hashes[0], peer_id_1, retries, None);

        // peer_1 is idle
        assert!(tx_fetcher.is_idle(&peer_id_1));
        assert_eq!(tx_fetcher.active_peers.len(), 0);

        // sends request for buffered hashes to peer_1
        tx_fetcher.on_fetch_pending_hashes(&tx_manager.peers, |_| true);

        let tx_fetcher = &mut tx_manager.transaction_fetcher;

        assert_eq!(tx_fetcher.num_pending_hashes(), 0);
        // as long as request is in inflight peer_1 is not idle
        assert!(!tx_fetcher.is_idle(&peer_id_1));
        assert_eq!(tx_fetcher.active_peers.len(), 1);

        // mock session of peer_1 receives request
        let req = to_mock_session_rx
            .recv()
            .await
            .expect("peer_1 session should receive request with buffered hashes");
        let PeerRequest::GetPooledTransactions { request, response } = req else { unreachable!() };
        let GetPooledTransactions(hashes) = request;

        let hashes = hashes.into_iter().collect::<HashSet<_>>();

        assert_eq!(hashes, seen_hashes.into_iter().collect::<HashSet<_>>());

        // fail request to peer_1
        response
            .send(Err(RequestError::BadResponse))
            .expect("should send peer_1 response to tx manager");
        let Some(FetchEvent::FetchError { peer_id, .. }) = tx_fetcher.next().await else {
            unreachable!()
        };

        // request has resolved, peer_1 is idle again
        assert!(tx_fetcher.is_idle(&peer_id));
        assert_eq!(tx_fetcher.active_peers.len(), 0);
        // failing peer_1's request buffers requested hashes for retry
        assert_eq!(tx_fetcher.num_pending_hashes(), 2);

        let (peer_2, mut to_mock_session_rx) = new_mock_session(peer_id_2, eth_version);
        tx_manager.peers.insert(peer_id_2, peer_2);

        // peer_2 announces same hashes as peer_1
        let msg =
            NewPooledTransactionHashes::Eth66(NewPooledTransactionHashes66(seen_hashes.to_vec()));
        tx_manager.on_new_pooled_transaction_hashes(peer_id_2, msg);

        let tx_fetcher = &mut tx_manager.transaction_fetcher;

        // peer_2 should be in active_peers.
        assert_eq!(tx_fetcher.active_peers.len(), 1);

        // since hashes are already seen, no changes to length of unknown hashes
        assert_eq!(tx_fetcher.num_all_hashes(), 2);
        // but hashes are taken out of buffer and packed into request to peer_2
        assert_eq!(tx_fetcher.num_pending_hashes(), 0);

        // mock session of peer_2 receives request
        let req = to_mock_session_rx
            .recv()
            .await
            .expect("peer_2 session should receive request with buffered hashes");
        let PeerRequest::GetPooledTransactions { response, .. } = req else { unreachable!() };

        // report failed request to tx manager
        response
            .send(Err(RequestError::BadResponse))
            .expect("should send peer_2 response to tx manager");
        let Some(FetchEvent::FetchError { .. }) = tx_fetcher.next().await else { unreachable!() };

        // `MAX_REQUEST_RETRIES_PER_TX_HASH`, 2, for hashes reached so this time won't be buffered
        // for retry
        assert_eq!(tx_fetcher.num_pending_hashes(), 0);
        assert_eq!(tx_fetcher.active_peers.len(), 0);
    }

    #[test]
    fn test_transaction_builder_empty() {
        let mut builder =
            PropagateTransactionsBuilder::<TransactionSigned>::pooled(EthVersion::Eth68);
        assert!(builder.is_empty());

        let mut factory = MockTransactionFactory::default();
        let tx = PropagateTransaction::pool_tx(Arc::new(factory.create_eip1559()));
        builder.push(&tx);
        assert!(!builder.is_empty());

        let txs = builder.build();
        assert!(txs.full.is_none());
        let txs = txs.pooled.unwrap();
        assert_eq!(txs.len(), 1);
    }

    #[test]
    fn test_transaction_builder_large() {
        let mut builder =
            PropagateTransactionsBuilder::<TransactionSigned>::full(EthVersion::Eth68);
        assert!(builder.is_empty());

        let mut factory = MockTransactionFactory::default();
        let mut tx = factory.create_eip1559();
        // create a transaction that still fits
        tx.transaction.set_size(DEFAULT_SOFT_LIMIT_BYTE_SIZE_TRANSACTIONS_BROADCAST_MESSAGE + 1);
        let tx = Arc::new(tx);
        let tx = PropagateTransaction::pool_tx(tx);
        builder.push(&tx);
        assert!(!builder.is_empty());

        let txs = builder.clone().build();
        assert!(txs.pooled.is_none());
        let txs = txs.full.unwrap();
        assert_eq!(txs.len(), 1);

        builder.push(&tx);

        let txs = builder.clone().build();
        let pooled = txs.pooled.unwrap();
        assert_eq!(pooled.len(), 1);
        let txs = txs.full.unwrap();
        assert_eq!(txs.len(), 1);
    }

    #[test]
    fn test_transaction_builder_eip4844() {
        let mut builder =
            PropagateTransactionsBuilder::<TransactionSigned>::full(EthVersion::Eth68);
        assert!(builder.is_empty());

        let mut factory = MockTransactionFactory::default();
        let tx = PropagateTransaction::pool_tx(Arc::new(factory.create_eip4844()));
        builder.push(&tx);
        assert!(!builder.is_empty());

        let txs = builder.clone().build();
        assert!(txs.full.is_none());
        let txs = txs.pooled.unwrap();
        assert_eq!(txs.len(), 1);

        let tx = PropagateTransaction::pool_tx(Arc::new(factory.create_eip1559()));
        builder.push(&tx);

        let txs = builder.clone().build();
        let pooled = txs.pooled.unwrap();
        assert_eq!(pooled.len(), 1);
        let txs = txs.full.unwrap();
        assert_eq!(txs.len(), 1);
    }

    #[tokio::test]
    async fn test_propagate_full() {
        reth_tracing::init_test_tracing();

        let (mut tx_manager, network) = new_tx_manager().await;
        let peer_id = PeerId::random();

        // ensure not syncing
        network.handle().update_sync_state(SyncState::Idle);

        // mock a peer
        let (tx, _rx) = mpsc::channel::<PeerRequest>(1);

        let session_info = SessionInfo {
            peer_id,
            remote_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            client_version: Arc::from(""),
            capabilities: Arc::new(vec![].into()),
            status: Arc::new(Default::default()),
            version: EthVersion::Eth68,
            peer_kind: PeerKind::Basic,
        };
        let messages: PeerRequestSender<PeerRequest> = PeerRequestSender::new(peer_id, tx);
        tx_manager
            .on_network_event(NetworkEvent::ActivePeerSession { info: session_info, messages });
        let mut propagate = vec![];
        let mut factory = MockTransactionFactory::default();
        let eip1559_tx = Arc::new(factory.create_eip1559());
        propagate.push(PropagateTransaction::pool_tx(eip1559_tx.clone()));
        let eip4844_tx = Arc::new(factory.create_eip4844());
        propagate.push(PropagateTransaction::pool_tx(eip4844_tx.clone()));

        let propagated =
            tx_manager.propagate_transactions(propagate.clone(), PropagationMode::Basic);
        assert_eq!(propagated.0.len(), 2);
        let prop_txs = propagated.0.get(eip1559_tx.transaction.hash()).unwrap();
        assert_eq!(prop_txs.len(), 1);
        assert!(prop_txs[0].is_full());

        let prop_txs = propagated.0.get(eip4844_tx.transaction.hash()).unwrap();
        assert_eq!(prop_txs.len(), 1);
        assert!(prop_txs[0].is_hash());

        let peer = tx_manager.peers.get(&peer_id).unwrap();
        assert!(peer.seen_transactions.contains(eip1559_tx.transaction.hash()));
        assert!(peer.seen_transactions.contains(eip1559_tx.transaction.hash()));
        peer.seen_transactions.contains(eip4844_tx.transaction.hash());

        // propagate again
        let propagated = tx_manager.propagate_transactions(propagate, PropagationMode::Basic);
        assert!(propagated.0.is_empty());
    }

    #[tokio::test]
    async fn test_relaxed_filter_ignores_unknown_tx_types() {
        reth_tracing::init_test_tracing();

        let transactions_manager_config = TransactionsManagerConfig::default();

        let propagation_policy = TransactionPropagationKind::default();
        let announcement_policy = RelaxedEthAnnouncementFilter::default();

        let policy_bundle = NetworkPolicies::new(propagation_policy, announcement_policy);

        let pool = testing_pool();
        let secret_key = SecretKey::new(&mut rand_08::thread_rng());
        let client = NoopProvider::default();

        let network_config = NetworkConfigBuilder::new(secret_key)
            .listener_port(0)
            .disable_discovery()
            .build(client.clone());

        let mut network_manager = NetworkManager::new(network_config).await.unwrap();
        let (to_tx_manager_tx, from_network_rx) =
            mpsc::unbounded_channel::<NetworkTransactionEvent<EthNetworkPrimitives>>();
        network_manager.set_transactions(to_tx_manager_tx);
        let network_handle = network_manager.handle().clone();
        let network_service_handle = tokio::spawn(network_manager);

        let mut tx_manager = TransactionsManager::<
            TestPool,
            EthNetworkPrimitives,
            NetworkPolicies<TransactionPropagationKind, RelaxedEthAnnouncementFilter>,
        >::with_policy(
            network_handle.clone(),
            pool.clone(),
            from_network_rx,
            transactions_manager_config,
            policy_bundle,
        );

        let peer_id = PeerId::random();
        let eth_version = EthVersion::Eth68;
        let (mock_peer_metadata, mut mock_session_rx) = new_mock_session(peer_id, eth_version);
        tx_manager.peers.insert(peer_id, mock_peer_metadata);

        let mut tx_factory = MockTransactionFactory::default();

        let valid_known_tx = tx_factory.create_eip1559();
        let known_tx_signed: Arc<ValidPoolTransaction<MockTransaction>> = Arc::new(valid_known_tx);

        let known_tx_hash = *known_tx_signed.hash();
        let known_tx_type_byte = known_tx_signed.transaction.tx_type();
        let known_tx_size = known_tx_signed.encoded_length();

        let unknown_tx_hash = B256::random();
        let unknown_tx_type_byte = 0xff_u8;
        let unknown_tx_size = 150;

        let announcement_msg = NewPooledTransactionHashes::Eth68(NewPooledTransactionHashes68 {
            types: vec![known_tx_type_byte, unknown_tx_type_byte],
            sizes: vec![known_tx_size, unknown_tx_size],
            hashes: vec![known_tx_hash, unknown_tx_hash],
        });

        tx_manager.on_new_pooled_transaction_hashes(peer_id, announcement_msg);

        poll_fn(|cx| {
            let _ = tx_manager.poll_unpin(cx);
            Poll::Ready(())
        })
        .await;

        let mut requested_hashes_in_getpooled = HashSet::new();
        let mut unexpected_request_received = false;

        match tokio::time::timeout(std::time::Duration::from_millis(200), mock_session_rx.recv())
            .await
        {
            Ok(Some(PeerRequest::GetPooledTransactions { request, response: tx_response_ch })) => {
                let GetPooledTransactions(hashes) = request;
                for hash in hashes {
                    requested_hashes_in_getpooled.insert(hash);
                }
                let _ = tx_response_ch.send(Ok(PooledTransactions(vec![])));
            }
            Ok(Some(other_request)) => {
                tracing::error!(?other_request, "Received unexpected PeerRequest type");
                unexpected_request_received = true;
            }
            Ok(None) => tracing::info!("Mock session channel closed or no request received."),
            Err(_timeout_err) => {
                tracing::info!("Timeout: No GetPooledTransactions request received.")
            }
        }

        assert!(
            requested_hashes_in_getpooled.contains(&known_tx_hash),
            "Should have requested the known EIP-1559 transaction. Requested: {requested_hashes_in_getpooled:?}"
        );
        assert!(
            !requested_hashes_in_getpooled.contains(&unknown_tx_hash),
            "Should NOT have requested the unknown transaction type. Requested: {requested_hashes_in_getpooled:?}"
        );
        assert!(
            !unexpected_request_received,
            "An unexpected P2P request was received by the mock peer."
        );

        network_service_handle.abort();
    }
}
