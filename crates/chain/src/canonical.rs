//! Canonical view of transactions and unspent outputs.
//!
//! This module provides [`CanonicalView`], a utility for obtaining a canonical (ordered and
//! conflict-resolved) view of transactions from a [`TxGraph`].
//!
//! ## Example
//!
//! ```
//! # use bdk_chain::{TxGraph, CanonicalParams, CanonicalTask, local_chain::LocalChain};
//! # use bdk_core::BlockId;
//! # use bitcoin::hashes::Hash;
//! # let tx_graph = TxGraph::<BlockId>::default();
//! # let chain = LocalChain::from_blocks([(0, bitcoin::BlockHash::all_zeros())].into_iter().collect()).unwrap();
//! let chain_tip = chain.tip().block_id();
//! let params = CanonicalParams::default();
//! let task = CanonicalTask::new(&tx_graph, chain_tip, params);
//! let view = chain.canonicalize(task);
//!
//! // Iterate over canonical transactions
//! for tx in view.txs() {
//!     println!("Transaction {}: {:?}", tx.txid, tx.pos);
//! }
//! ```

use crate::collections::{HashMap, HashSet, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::{fmt, ops::RangeBounds};

use bdk_core::BlockId;
use bitcoin::{
    constants::COINBASE_MATURITY, Amount, OutPoint, ScriptBuf, Transaction, TxOut, Txid,
};

use crate::tx_graph::TxNode;
use crate::{spk_txout::SpkTxOutIndex, Anchor, Balance, ChainPosition};

/// A single canonical transaction with its position.
///
/// This struct represents a transaction that has been determined to be canonical (not
/// conflicted). It includes the transaction itself along with its position information.
/// The position type `P` is generic — it can be [`ChainPosition`] for resolved views,
/// or [`CanonicalReason`](crate::canonical_task::CanonicalReason) for unresolved canonicalization
/// results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalTx<P> {
    /// The position of this transaction.
    ///
    /// When `P` is [`ChainPosition`], this indicates whether the transaction is confirmed
    /// (and at what height) or unconfirmed (most likely pending in the mempool).
    pub pos: P,
    /// The transaction ID (hash) of this transaction.
    pub txid: Txid,
    /// The full transaction.
    pub tx: Arc<Transaction>,
}

impl<P: Ord> Ord for CanonicalTx<P> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.pos
            .cmp(&other.pos)
            // Txid tiebreaker for same position
            .then_with(|| self.txid.cmp(&other.txid))
    }
}

impl<P: Ord> PartialOrd for CanonicalTx<P> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A canonical transaction output with position and spend information.
///
/// The position type `P` is generic — it can be [`ChainPosition`] for resolved views,
/// or [`CanonicalReason`](crate::canonical_task::CanonicalReason) for unresolved canonicalization
/// results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalTxOut<P> {
    /// The position of the transaction in `outpoint` in the overall chain.
    pub pos: P,
    /// The location of the `TxOut`.
    pub outpoint: OutPoint,
    /// The `TxOut`.
    pub txout: TxOut,
    /// The txid and position of the transaction (if any) that has spent this output.
    pub spent_by: Option<(P, Txid)>,
    /// Whether this output is on a coinbase transaction.
    pub is_on_coinbase: bool,
}

impl<P: Ord> Ord for CanonicalTxOut<P> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.pos
            .cmp(&other.pos)
            // Tie-break with `outpoint` and `spent_by`.
            .then_with(|| self.outpoint.cmp(&other.outpoint))
            .then_with(|| self.spent_by.cmp(&other.spent_by))
    }
}

impl<P: Ord> PartialOrd for CanonicalTxOut<P> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<A: Anchor> CanonicalTxOut<ChainPosition<A>> {
    /// Whether the `txout` is considered mature.
    ///
    /// Depending on the implementation of [`confirmation_height_upper_bound`] in [`Anchor`], this
    /// method may return false-negatives. In other words, interpreted confirmation count may be
    /// less than the actual value.
    ///
    /// [`confirmation_height_upper_bound`]: Anchor::confirmation_height_upper_bound
    pub fn is_mature(&self, tip: u32) -> bool {
        if self.is_on_coinbase {
            let conf_height = match self.pos.confirmation_height_upper_bound() {
                Some(height) => height,
                None => {
                    debug_assert!(false, "coinbase tx can never be unconfirmed");
                    return false;
                }
            };
            let age = tip.saturating_sub(conf_height);
            if age + 1 < COINBASE_MATURITY {
                return false;
            }
        }

        true
    }

    /// Whether the utxo is/was/will be spendable with chain `tip`.
    ///
    /// This method does not take into account the lock time.
    ///
    /// Depending on the implementation of [`confirmation_height_upper_bound`] in [`Anchor`], this
    /// method may return false-negatives. In other words, interpreted confirmation count may be
    /// less than the actual value.
    ///
    /// [`confirmation_height_upper_bound`]: Anchor::confirmation_height_upper_bound
    pub fn is_confirmed_and_spendable(&self, tip: u32) -> bool {
        if !self.is_mature(tip) {
            return false;
        }

        let conf_height = match self.pos.confirmation_height_upper_bound() {
            Some(height) => height,
            None => return false,
        };
        if conf_height > tip {
            return false;
        }

        // if the spending tx is confirmed within tip height, the txout is no longer spendable
        if let Some(spend_height) = self
            .spent_by
            .as_ref()
            .and_then(|(pos, _)| pos.confirmation_height_upper_bound())
        {
            if spend_height <= tip {
                return false;
            }
        }

        true
    }
}

/// Canonical set of transactions from a [`TxGraph`](crate::TxGraph).
///
/// `Canonical` provides an ordered, conflict-resolved set of transactions. It determines
/// which transactions are canonical (non-conflicted) based on the current chain state and
/// provides methods to query transaction data, unspent outputs, and balances.
///
/// The position type `P` is generic:
/// - [`ChainPosition<A>`] for resolved views (aka [`CanonicalView`])
/// - [`CanonicalReason<A>`](crate::canonical_task::CanonicalReason) for unresolved results (aka
///   [`CanonicalTxs`])
///
/// The view maintains:
/// - A mapping of outpoints to the transactions that spend them
/// - The chain tip used for canonicalization
#[derive(Debug)]
pub struct Canonical<A, T, P> {
    /// Ordered list of transaction IDs.
    pub(crate) order: Vec<Txid>,
    /// Map of transaction IDs to their transaction data and canonical reason.
    pub(crate) txs: HashMap<Txid, (T, P)>,
    /// Map of outpoints to the transaction ID that spends them.
    pub(crate) spends: HashMap<OutPoint, Txid>,
    /// The chain tip at the time this view was created.
    pub(crate) tip: BlockId,
    /// Marker for the anchor type
    marker: core::marker::PhantomData<A>,
}

use crate::canonical_task::CanonicalReason;

/// Type alias for canonical transactions with unresolved
/// [`CanonicalReason`](crate::canonical_task::CanonicalReason)s.
pub type CanonicalTxs<'g, A> = Canonical<A, TxNode<'g, Arc<Transaction>, A>, CanonicalReason<A>>;

/// Type alias for canonical transactions with resolved [`ChainPosition`]s.
pub type CanonicalView<A> = Canonical<A, Arc<Transaction>, ChainPosition<A>>;

impl<A, T, P> Canonical<A, T, P>
where
    T: Clone + Into<Arc<Transaction>>,
    P: Clone,
{
    /// Creates a [`Canonical`] from its constituent parts.
    ///
    /// This internal constructor is used by [`CanonicalTask`](crate::CanonicalTask) to build the
    /// canonical set after completing the canonicalization process. It takes the processed
    /// transaction data including the canonical ordering, transaction map with positions, and
    /// spend information.
    pub(crate) fn new(
        tip: BlockId,
        order: Vec<Txid>,
        txs: HashMap<Txid, (T, P)>,
        spends: HashMap<OutPoint, Txid>,
    ) -> Self {
        Self {
            tip,
            order,
            txs,
            spends,
            marker: core::marker::PhantomData,
        }
    }

    /// Get the chain tip used to construct this canonical set.
    pub fn tip(&self) -> BlockId {
        self.tip
    }

    /// Get a single canonical transaction by its transaction ID.
    ///
    /// Returns `Some(CanonicalTx)` if the transaction exists in the canonical set,
    /// or `None` if the transaction doesn't exist or was excluded due to conflicts.
    pub fn tx(&self, txid: Txid) -> Option<CanonicalTx<P>> {
        self.txs.get(&txid).map(|(t, pos)| CanonicalTx {
            pos: pos.clone(),
            txid,
            tx: t.clone().into(),
        })
    }

    /// Get a single canonical transaction output.
    ///
    /// Returns detailed information about a transaction output, including whether it has been
    /// spent and by which transaction.
    ///
    /// Returns `None` if:
    /// - The transaction doesn't exist in the canonical set
    /// - The output index is out of bounds
    /// - The transaction was excluded due to conflicts
    pub fn txout(&self, op: OutPoint) -> Option<CanonicalTxOut<P>> {
        let (tx, pos) = self.txs.get(&op.txid)?;
        let vout: usize = op.vout.try_into().ok()?;
        let tx: Arc<Transaction> = tx.clone().into();
        let txout = tx.output.get(vout)?;
        let spent_by = self.spends.get(&op).map(|spent_by_txid| {
            let (_, spent_by_pos) = &self.txs[spent_by_txid];
            (spent_by_pos.clone(), *spent_by_txid)
        });
        Some(CanonicalTxOut {
            pos: pos.clone(),
            outpoint: op,
            txout: txout.clone(),
            spent_by,
            is_on_coinbase: tx.is_coinbase(),
        })
    }

    /// Get an iterator over all canonical transactions in order.
    ///
    /// Transactions are returned in canonical order, with confirmed transactions ordered by
    /// block height and position, followed by unconfirmed transactions.
    ///
    /// # Example
    ///
    /// ```
    /// # use bdk_chain::{TxGraph, CanonicalTask, local_chain::LocalChain};
    /// # use bdk_core::BlockId;
    /// # use bitcoin::hashes::Hash;
    /// # let tx_graph = TxGraph::<BlockId>::default();
    /// # let chain = LocalChain::from_blocks([(0, bitcoin::BlockHash::all_zeros())].into_iter().collect()).unwrap();
    /// # let chain_tip = chain.tip().block_id();
    /// # let task = CanonicalTask::new(&tx_graph, chain_tip, Default::default());
    /// # let view = chain.canonicalize(task);
    /// // Iterate over all canonical transactions
    /// for tx in view.txs() {
    ///     println!("TX {}: {:?}", tx.txid, tx.pos);
    /// }
    ///
    /// // Get the total number of canonical transactions
    /// println!("Total canonical transactions: {}", view.txs().len());
    /// ```
    pub fn txs(&self) -> impl ExactSizeIterator<Item = CanonicalTx<P>> + DoubleEndedIterator + '_ {
        self.order
            .iter()
            .map(|&txid| self.tx(txid).expect("order tx must exist"))
    }

    /// Get a filtered list of outputs from the given outpoints.
    ///
    /// This method takes an iterator of `(identifier, outpoint)` pairs and returns an iterator
    /// of `(identifier, canonical_txout)` pairs for outpoints that exist in the canonical set.
    /// Non-existent outpoints are silently filtered out.
    ///
    /// The identifier type `O` is useful for tracking which outpoints correspond to which addresses
    /// or keys.
    ///
    /// # Example
    ///
    /// ```
    /// # use bdk_chain::{TxGraph, CanonicalTask, local_chain::LocalChain, keychain_txout::KeychainTxOutIndex};
    /// # use bdk_core::BlockId;
    /// # use bitcoin::hashes::Hash;
    /// # let tx_graph = TxGraph::<BlockId>::default();
    /// # let chain = LocalChain::from_blocks([(0, bitcoin::BlockHash::all_zeros())].into_iter().collect()).unwrap();
    /// # let chain_tip = chain.tip().block_id();
    /// # let task = CanonicalTask::new(&tx_graph, chain_tip, Default::default());
    /// # let view = chain.canonicalize(task);
    /// # let indexer = KeychainTxOutIndex::<&str>::default();
    /// // Get all outputs from an indexer
    /// for (keychain, txout) in view.filter_outpoints(indexer.outpoints().clone()) {
    ///     println!("{}: {} sats", keychain.0, txout.txout.value);
    /// }
    /// ```
    pub fn filter_outpoints<'v, O: Clone + 'v>(
        &'v self,
        outpoints: impl IntoIterator<Item = (O, OutPoint)> + 'v,
    ) -> impl Iterator<Item = (O, CanonicalTxOut<P>)> + 'v {
        outpoints
            .into_iter()
            .filter_map(|(op_i, op)| Some((op_i, self.txout(op)?)))
    }

    /// Get a filtered list of unspent outputs (UTXOs) from the given outpoints.
    ///
    /// Similar to [`filter_outpoints`](Self::filter_outpoints), but only returns outputs that
    /// have not been spent. This is useful for finding available UTXOs for spending.
    ///
    /// # Example
    ///
    /// ```
    /// # use bdk_chain::{TxGraph, CanonicalTask, local_chain::LocalChain, keychain_txout::KeychainTxOutIndex};
    /// # use bdk_core::BlockId;
    /// # use bitcoin::hashes::Hash;
    /// # let tx_graph = TxGraph::<BlockId>::default();
    /// # let chain = LocalChain::from_blocks([(0, bitcoin::BlockHash::all_zeros())].into_iter().collect()).unwrap();
    /// # let chain_tip = chain.tip().block_id();
    /// # let task = CanonicalTask::new(&tx_graph, chain_tip, Default::default());
    /// # let view = chain.canonicalize(task);
    /// # let indexer = KeychainTxOutIndex::<&str>::default();
    /// // Get unspent outputs (UTXOs) from an indexer
    /// for (keychain, utxo) in view.filter_unspent_outpoints(indexer.outpoints().clone()) {
    ///     println!("{} UTXO: {} sats", keychain.0, utxo.txout.value);
    /// }
    /// ```
    pub fn filter_unspent_outpoints<'v, O: Clone + 'v>(
        &'v self,
        outpoints: impl IntoIterator<Item = (O, OutPoint)> + 'v,
    ) -> impl Iterator<Item = (O, CanonicalTxOut<P>)> + 'v {
        self.filter_outpoints(outpoints)
            .filter(|(_, txo)| txo.spent_by.is_none())
    }

    /// List transaction IDs that are expected to exist for the given script pubkeys.
    ///
    /// This method is primarily used for synchronization with external sources, helping to
    /// identify which transactions are expected to exist for a set of script pubkeys. It's
    /// commonly used with
    /// [`SyncRequestBuilder::expected_spk_txids`](bdk_core::spk_client::SyncRequestBuilder::expected_spk_txids)
    /// to inform sync operations about known transactions.
    pub fn list_expected_spk_txids<'v, I>(
        &'v self,
        indexer: &'v impl AsRef<SpkTxOutIndex<I>>,
        spk_index_range: impl RangeBounds<I> + 'v,
    ) -> impl Iterator<Item = (ScriptBuf, Txid)> + 'v
    where
        I: fmt::Debug + Clone + Ord + 'v,
    {
        let indexer = indexer.as_ref();
        self.txs().flat_map(move |c_tx| -> Vec<_> {
            let range = &spk_index_range;
            let relevant_spks = indexer.relevant_spks_of_tx(&c_tx.tx);
            relevant_spks
                .into_iter()
                .filter(|(i, _)| range.contains(i))
                .map(|(_, spk)| (spk, c_tx.txid))
                .collect()
        })
    }
}

impl<A: Anchor> CanonicalView<A> {
    /// Calculate the total balance of the given outpoints.
    ///
    /// This method computes a detailed balance breakdown for a set of outpoints, categorizing
    /// outputs as confirmed, pending (trusted/untrusted), or immature based on their chain
    /// position and the provided trust predicate.
    ///
    /// # Arguments
    ///
    /// * `outpoints` - Iterator of `(identifier, outpoint)` pairs to calculate balance for
    /// * `trust_predicate` - Function that returns `true` for trusted scripts. Trusted outputs
    ///   count toward `trusted_pending` balance, while untrusted ones count toward
    ///   `untrusted_pending`
    /// * `min_confirmations` - Minimum confirmations required for an output to be considered
    ///   confirmed. Outputs with fewer confirmations are treated as pending.
    ///
    /// # Minimum Confirmations
    ///
    /// The `min_confirmations` parameter controls when outputs are considered confirmed. A
    /// `min_confirmations` value of `0` is equivalent to `1` (require at least 1 confirmation).
    ///
    /// Outputs with fewer than `min_confirmations` are categorized as pending (trusted or
    /// untrusted based on the trust predicate).
    ///
    /// # Example
    ///
    /// ```
    /// # use bdk_chain::{CanonicalParams, TxGraph, local_chain::LocalChain, keychain_txout::KeychainTxOutIndex};
    /// # use bdk_core::BlockId;
    /// # use bitcoin::hashes::Hash;
    /// # let tx_graph = TxGraph::<BlockId>::default();
    /// # let chain = LocalChain::from_blocks([(0, bitcoin::BlockHash::all_zeros())].into_iter().collect()).unwrap();
    /// # let chain_tip = chain.tip().block_id();
    /// # let view = chain.canonical_view(&tx_graph, chain_tip, CanonicalParams::default());
    /// # let indexer = KeychainTxOutIndex::<&str>::default();
    /// // Calculate balance with 6 confirmations, trusting all outputs
    /// let balance = view.balance(
    ///     indexer.outpoints().into_iter().map(|(k, op)| (k.clone(), *op)),
    ///     |_keychain, _script| true,  // Trust all outputs
    ///     6,  // Require 6 confirmations
    /// );
    /// ```
    pub fn balance<'v, O: Clone + 'v>(
        &'v self,
        outpoints: impl IntoIterator<Item = (O, OutPoint)> + 'v,
        mut trust_predicate: impl FnMut(&O, &CanonicalTxOut<ChainPosition<A>>) -> bool,
        min_confirmations: u32,
    ) -> Balance {
        let mut immature = Amount::ZERO;
        let mut trusted_pending = Amount::ZERO;
        let mut untrusted_pending = Amount::ZERO;
        let mut confirmed = Amount::ZERO;

        for (spk_i, txout) in self.filter_unspent_outpoints(outpoints) {
            match &txout.pos {
                ChainPosition::Confirmed { anchor, .. } => {
                    let confirmation_height = anchor.confirmation_height_upper_bound();
                    let confirmations = self
                        .tip
                        .height
                        .saturating_sub(confirmation_height)
                        .saturating_add(1);
                    let min_confirmations = min_confirmations.max(1); // 0 and 1 behave identically

                    if confirmations < min_confirmations {
                        // Not enough confirmations, treat as trusted/untrusted pending
                        if trust_predicate(&spk_i, &txout) {
                            trusted_pending += txout.txout.value;
                        } else {
                            untrusted_pending += txout.txout.value;
                        }
                    } else if txout.is_confirmed_and_spendable(self.tip.height) {
                        confirmed += txout.txout.value;
                    } else if !txout.is_mature(self.tip.height) {
                        immature += txout.txout.value;
                    }
                }
                ChainPosition::Unconfirmed { .. } => {
                    if trust_predicate(&spk_i, &txout) {
                        trusted_pending += txout.txout.value;
                    } else {
                        untrusted_pending += txout.txout.value;
                    }
                }
            }
        }

        Balance {
            immature,
            trusted_pending,
            untrusted_pending,
            confirmed,
        }
    }
}

impl<'g, A: Anchor> CanonicalTxs<'g, A> {
    /// Finds the nearest descendant of `start_txid` that carries a direct anchor.
    ///
    /// To correctly resolve the `ChainPosition` we perform a breadth-first search over the
    /// `spends`, which only contains canonical spends, so the traversal stays within the
    /// canonical set and terminates.
    ///
    /// Returns `Some((anchor, desc_txid))` for the nearest anchored descendant, or `None`.
    fn find_anchored_descendant(&self, start_txid: Txid) -> Option<(A, Txid)> {
        let mut queue = VecDeque::<Txid>::new();
        let mut visited = HashSet::<Txid>::new();

        // Seed queue with direct canonical descendants of start_txid.
        if let Some((tx_node, _)) = self.txs.get(&start_txid) {
            for vout in 0..tx_node.tx.output.len() as u32 {
                let op = OutPoint::new(start_txid, vout);
                if let Some(&spending_txid) = self.spends.get(&op) {
                    if visited.insert(spending_txid) {
                        queue.push_back(spending_txid);
                    }
                }
            }
        }

        while let Some(desc_txid) = queue.pop_front() {
            if let Some((tx_node, reason)) = self.txs.get(&desc_txid) {
                match reason {
                    CanonicalReason::Anchor { anchor, .. } => {
                        return Some((anchor.clone(), desc_txid));
                    }
                    CanonicalReason::Assumed {
                        anchor: Some(anchor),
                        ..
                    } => {
                        return Some((anchor.clone(), desc_txid));
                    }
                    _ => {
                        // No direct anchor here, continue to the next level.
                        for vout in 0..tx_node.tx.output.len() as u32 {
                            let op = OutPoint::new(desc_txid, vout);
                            if let Some(&spending_txid) = self.spends.get(&op) {
                                if visited.insert(spending_txid) {
                                    queue.push_back(spending_txid);
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }

    /// Transform this [`Canonical`] into a view mapping every canonical reason to a specific chain
    /// position.
    pub fn view(self) -> CanonicalView<A> {
        use crate::ChainPosition;
        use crate::ObservedIn;
        let mut order = Vec::new();
        let mut txs = HashMap::new();

        for txid in &self.order {
            if let Some((tx_node, reason)) = self.txs.get(txid).cloned() {
                order.push(*txid);

                // Determine chain position from canonical reason
                let chain_position = match reason {
                    CanonicalReason::Assumed { anchor, descendant } => {
                        match anchor {
                            Some(anchor) => ChainPosition::Confirmed {
                                anchor,
                                transitively: None,
                            },
                            None => match descendant {
                                Some(_) => match self.find_anchored_descendant(*txid) {
                                    Some((anchor, anchored_desc)) => ChainPosition::Confirmed {
                                        anchor,
                                        transitively: Some(anchored_desc),
                                    },
                                    None => ChainPosition::Unconfirmed {
                                        first_seen: tx_node.first_seen,
                                        last_seen: tx_node.last_seen,
                                    },
                                },
                                // Directly assumed canonical, no anchor found.
                                None => ChainPosition::Unconfirmed {
                                    first_seen: tx_node.first_seen,
                                    last_seen: tx_node.last_seen,
                                },
                            },
                        }
                    }
                    CanonicalReason::Anchor { anchor, descendant } => ChainPosition::Confirmed {
                        anchor: anchor.clone(),
                        transitively: descendant,
                    },
                    CanonicalReason::ObservedIn { observed_in, .. } => match observed_in {
                        ObservedIn::Mempool(last_seen) => ChainPosition::Unconfirmed {
                            first_seen: tx_node.first_seen,
                            last_seen: Some(last_seen),
                        },
                        ObservedIn::Block(_) => ChainPosition::Unconfirmed {
                            first_seen: tx_node.first_seen,
                            last_seen: None,
                        },
                    },
                };

                txs.insert(*txid, (tx_node.tx, chain_position));
            }
        }

        CanonicalView::new(self.tip, order, txs, self.spends.clone())
    }
}
