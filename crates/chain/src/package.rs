//! Package

use alloc::sync::Arc;
use alloc::vec::Vec;

use bitcoin::{Transaction, Txid};

use crate::collections::{BTreeMap, BTreeSet};
use crate::{Anchor, ChainPosition, TxGraph};

/// Structure that accepts transactions and puts them in topological order.
///
/// # Example
///
/// Create a [`Pool`] from two unrelated transactions `tx_1` and `tx_2`.
///
/// ```rust,no_run
/// # use bdk_core::BlockId;
/// # use bdk_chain::TxGraph;
/// # use bdk_chain::package::Pool;
/// # use bdk_chain::example_utils::*;
/// # let tx1 = tx_from_hex(RAW_TX_1);
/// # let tx2 = tx_from_hex(RAW_TX_2);
/// let mut graph = TxGraph::<BlockId>::new(vec![tx1, tx2]);
/// let mut pool = Pool::from_tx_graph(&graph);
/// let packages = pool.select_packages();
/// assert_eq!(packages.len(), 2);
/// ```
#[derive(Debug)]
pub struct Pool<A> {
    /// lookup a package tx by txid
    pool: BTreeMap<Txid, PackageTx<A>>,
    /// tx graph
    graph: TxGraph<A>,
}

impl<A: Anchor> Pool<A> {
    /// New from tx graph
    pub fn from_tx_graph(graph: &TxGraph<A>) -> Self {
        let txs = graph.full_txs().map(|tx| tx.tx);
        Self::new(txs)
    }

    /// New from txs
    pub fn new(txs: impl IntoIterator<Item = Arc<Transaction>>) -> Self {
        Self::from_chain_position_txs(txs.into_iter().map(|tx| (None, tx)))
    }

    /// New from txs with optional chain position
    pub fn from_chain_position_txs(
        txs: impl IntoIterator<Item = (Option<ChainPosition<A>>, Arc<Transaction>)>,
    ) -> Self {
        let pool: BTreeMap<Txid, PackageTx<A>> = txs
            .into_iter()
            .map(|(pos, tx)| {
                let txid = tx.compute_txid();
                let tx = PackageTx::new(txid, tx, pos);
                (txid, tx)
            })
            .collect();
        let mut graph = TxGraph::<A>::default();
        for tx in pool.values() {
            let _ = graph.insert_tx(tx.transaction());
        }
        let mut pool = Self { pool, graph };
        pool.set_links();
        pool
    }

    /// Get package tx by txid
    fn get(&self, txid: &Txid) -> Option<PackageTx<A>> {
        self.pool.get(txid).cloned()
    }

    /// Get package tx by txid, `unwrap`ed. Panics if no value exists in the pool
    /// under the given key
    fn get_unwrap(&self, txid: &Txid) -> PackageTx<A> {
        self.get(txid).unwrap()
    }

    /// Set links
    ///
    /// This method populates the ancestor and descendant lists for all txs
    /// in the pool.
    fn set_links(&mut self) {
        for (txid, tx) in self.pool.iter_mut() {
            tx.ancestors = self
                .graph
                .walk_ancestors(tx.transaction(), |_, tx| Some(tx.compute_txid()))
                .collect();
            tx.descendants = self
                .graph
                .walk_descendants(*txid, |_, txid| Some(txid))
                .collect();
        }
    }

    /// Select packages
    ///
    /// Internally this works by locating the terminal children in the pool, i.e. the txs
    /// with no direct descendants, and for each one creates a [`Package`] containing the
    /// child and all of its ancestors. Packages are created in order of the [`ChainPosition`]
    /// of the terminal child. If an ancestor is common to more than one package, it shall only
    /// appear in the first package to include it. Transactions in a package are sorted
    /// topologically such that ancestors appear before children.
    pub fn select_packages(&mut self) -> Vec<Package<A>> {
        // find txs with no direct descendants
        let mut children: Vec<&PackageTx<A>> = self
            .pool
            .values()
            .filter(|tx| tx.descendants.is_empty())
            .collect();

        // sort children by chain position
        children.sort_by_key(|tx| &tx.chain_position);
        let children: Vec<Txid> = children.into_iter().map(|tx| tx.txid).collect();

        children
            .into_iter()
            .map(|txid| {
                // create this package
                let tx = self.get_unwrap(&txid);
                let mut package = Package::new(tx.clone());
                for txid in &tx.ancestors {
                    let tx = self.get_unwrap(txid);
                    package.txs.push(tx);
                }
                // update descendants
                for tx in &package.txs {
                    self.update_descendants(tx);
                }
                package.sort_topologically();
                package
            })
            .collect()
    }

    /// Update descendants
    ///
    /// This method removes the given ancestor from the ancestor list of all its descendants.
    fn update_descendants(&mut self, ancestor: &PackageTx<A>) {
        for txid in &ancestor.descendants {
            self.pool.entry(*txid).and_modify(|tx| {
                tx.ancestors.remove(&ancestor.txid);
            });
        }
    }
}

/// A set of transactions linked by parent-child relationships
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package<A> {
    /// txs
    txs: Vec<PackageTx<A>>,
}

impl<A> Package<A> {
    /// New
    fn new(tx: PackageTx<A>) -> Self {
        Self { txs: vec![tx] }
    }

    /// Sort package txs topologically. We use the length of the ancestor set
    /// of each tx as the sort key.
    fn sort_topologically(&mut self) {
        self.txs.sort_by_key(|tx| tx.ancestors.len());
    }

    /// List the transactions in this [`Package`].
    pub fn transactions(&self) -> impl Iterator<Item = Arc<Transaction>> + '_ {
        self.txs.iter().map(|tx| tx.transaction())
    }

    /// List the transactions by [`Txid`] in this [`Package`].
    pub fn txids(&self) -> impl Iterator<Item = Txid> + '_ {
        self.txs.iter().map(|tx| tx.txid)
    }
}

/// Package tx
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageTx<A> {
    /// chain position
    chain_position: Option<ChainPosition<A>>,
    /// txid
    txid: Txid,
    /// tx
    tx: Arc<Transaction>,
    /// ancestors
    ancestors: BTreeSet<Txid>,
    /// descendants
    descendants: BTreeSet<Txid>,
}

impl<A> PackageTx<A> {
    /// New
    pub(crate) fn new(txid: Txid, tx: Arc<Transaction>, pos: Option<ChainPosition<A>>) -> Self {
        Self {
            chain_position: pos,
            txid,
            tx,
            ancestors: BTreeSet::new(),
            descendants: BTreeSet::new(),
        }
    }

    /// Get transaction
    fn transaction(&self) -> Arc<Transaction> {
        self.tx.clone()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use bdk_core::BlockId;
    use bitcoin::OutPoint;
    use bitcoin::TxIn;
    use bitcoin::TxOut;
    use rand::seq::SliceRandom;

    fn new_tx(lt: u32) -> Transaction {
        Transaction {
            version: bitcoin::transaction::Version(1),
            lock_time: bitcoin::absolute::LockTime::from_consensus(lt),
            input: vec![],
            output: vec![],
        }
    }

    #[test]
    fn topo_sorted() {
        /* Consider a cluster of related txs
        we have:
            B spends A
            D spends A and C
            E and F spend D
                A   C
               / \ /
              B   D
                 / \
                E   F

        we expect three packages:
            (A, B),
            (C, D, E),
            (F),
        */
        let tx_a = Transaction {
            input: vec![TxIn::default()],
            output: vec![TxOut::NULL, TxOut::NULL],
            ..new_tx(0)
        };
        let txid_a = tx_a.compute_txid();
        let tx_b = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint::new(txid_a, 0),
                ..Default::default()
            }],
            output: vec![TxOut::NULL],
            ..new_tx(1)
        };
        let tx_c = Transaction {
            input: vec![TxIn::default()],
            output: vec![TxOut::NULL],
            ..new_tx(2)
        };
        let txid_c = tx_c.compute_txid();
        let tx_d = Transaction {
            input: vec![
                TxIn {
                    previous_output: OutPoint::new(txid_a, 0),
                    ..Default::default()
                },
                TxIn {
                    previous_output: OutPoint::new(txid_c, 0),
                    ..Default::default()
                },
            ],
            output: vec![TxOut::NULL, TxOut::NULL],
            ..new_tx(3)
        };
        let txid_d = tx_d.compute_txid();
        let tx_e = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint::new(txid_d, 0),
                ..Default::default()
            }],
            output: vec![TxOut::NULL],
            ..new_tx(4)
        };
        let tx_f = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint::new(txid_d, 1),
                ..Default::default()
            }],
            output: vec![TxOut::NULL],
            ..new_tx(5)
        };
        let txs = vec![
            // these should be in expected order
            Arc::new(tx_a),
            Arc::new(tx_b),
            Arc::new(tx_c),
            Arc::new(tx_d),
            Arc::new(tx_e),
            Arc::new(tx_f),
        ];
        let txids: Vec<Txid> = txs.iter().map(|tx| tx.compute_txid()).collect();

        // include some chain positions. A comes before B, B before C, etc.
        let mut txs: Vec<(Option<ChainPosition<BlockId>>, Arc<Transaction>)> = (0..txs.len())
            .into_iter()
            .map(|i| Some(ChainPosition::Unconfirmed(i as u64)))
            .zip(txs)
            .collect();

        // randomize pool inputs
        txs.shuffle(&mut rand::thread_rng());

        let mut pool = Pool::from_chain_position_txs(txs);
        let packages = pool.select_packages();
        let sorted_txids: Vec<_> = packages
            .into_iter()
            .flat_map(|p| p.txs.into_iter().map(|tx| tx.txid))
            .collect();
        assert_eq!(
            sorted_txids, txids,
            "pool should sort packages topologically"
        );
    }
}
