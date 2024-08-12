//! # Compact block filters
//!
//! This module provides a method of chain syncing via [`BIP157`](https://github.com/bitcoin/bips/blob/master/bip-0157.mediawiki)
//! compact block filters.

use core::cmp;
use core::fmt;
use core::ops;
use core::ops::Bound;
use core::ops::RangeBounds;
use std::collections::BTreeMap;

use bdk_chain::bitcoin::{
    bip158::{self, BlockFilter},
    Block, BlockHash, ScriptBuf, Transaction,
};
use bdk_chain::indexer::keychain_txout::KeychainTxOutIndex;
use bdk_chain::local_chain::CheckPoint;
use bdk_chain::miniscript::Descriptor;
use bdk_chain::miniscript::DescriptorPublicKey;
use bdk_chain::SpkIterator;
use bdk_chain::{BlockId, ConfirmationBlockTime, IndexedTxGraph};
use bitcoincore_rpc;
use bitcoincore_rpc::RpcApi;

/// Block height
type Height = u32;

/// Unix time
type UnixTime = u64;

/// A keychain descriptor derived over an index range.
#[derive(Debug, Clone)]
struct Watch<K> {
    keychain: K,
    descriptor: Descriptor<DescriptorPublicKey>,
    range: (u32, u32),
}

/// A sync request.
pub struct Request<K> {
    cp: CheckPoint,
    watching: Vec<Watch<K>>,
    include_mempool: bool,
    #[allow(clippy::type_complexity)]
    inspect: Box<dyn Fn(&K, u32, &ScriptBuf)>,
}

impl<K: fmt::Debug + Clone + Ord> Request<K> {
    /// Create a new [`Request`] from the last local checkpoint.
    pub fn new(last_cp: CheckPoint) -> Self {
        Self {
            cp: last_cp,
            watching: vec![],
            include_mempool: false,
            inspect: Box::new(|_, _, _| {}),
        }
    }

    /// Add a descriptor to watch within an index `range`.
    pub fn add_descriptor(
        &mut self,
        keychain: K,
        descriptor: Descriptor<DescriptorPublicKey>,
        range: impl RangeBounds<u32>,
    ) -> &mut Self {
        let start = match range.start_bound() {
            Bound::Included(i) => *i,
            Bound::Excluded(i) => *i + 1,
            Bound::Unbounded => u32::MIN,
        };
        let end = match range.end_bound() {
            Bound::Included(i) => *i,
            Bound::Excluded(i) => *i - 1,
            Bound::Unbounded => u32::MAX,
        };
        self.watching.push(Watch {
            keychain,
            descriptor,
            range: (start, end),
        });
        self
    }

    /// Whether to include relevant unconfirmed transactions in this request.
    pub fn include_mempool(&mut self) -> &mut Self {
        self.include_mempool = true;
        self
    }

    /// Adds a callback to execute for each inspected spk.
    pub fn inspect_spks<F>(&mut self, f: F)
    where
        F: Fn(&K, u32, &ScriptBuf) + 'static,
    {
        self.inspect = Box::new(f);
    }

    /// Inspect spk.
    fn inspect_spk(&self, keychain: &K, index: u32, spk: &ScriptBuf) {
        self.inspect.as_ref()(keychain, index, spk);
    }

    /// Finish building the request and return a new [`Client`] with the given RPC `client`.
    pub fn build_client<C: RpcApi>(self, client: &C) -> Client<C, K> {
        // index and reveal the requested SPKs
        let mut indexer = KeychainTxOutIndex::default();
        let mut spks = vec![];
        let watching = self.watching.clone();
        for watch in watching {
            let Watch {
                keychain,
                descriptor,
                range,
            } = watch;
            let (start, end) = range;
            indexer
                .insert_descriptor(keychain.clone(), descriptor.clone())
                .unwrap();
            let spk_iter = SpkIterator::new_with_range(descriptor, start..=end);
            for (spk_index, spk) in spk_iter {
                self.inspect_spk(&keychain, spk_index, &spk);
                spks.push(spk);
            }
        }

        Client {
            client,
            cp: self.cp,
            spks,
            include_mempool: self.include_mempool,
            indexed_graph: IndexedTxGraph::new(indexer),
            blocks: BTreeMap::new(),
            next_filter: None,
            height: 0,
            stop: 0,
        }
    }
}

/// Client
#[derive(Debug)]
pub struct Client<'a, C, K> {
    // RPC client
    client: &'a C,

    /* Params */
    // local tip
    cp: CheckPoint,
    // SPK inventory
    spks: Vec<ScriptBuf>,
    // whether to query mempool txs
    include_mempool: bool,

    /* State */
    // indexed tx graph
    indexed_graph: IndexedTxGraph<ConfirmationBlockTime, KeychainTxOutIndex<K>>,
    // block map (height, hash)
    blocks: BTreeMap<Height, BlockHash>,
    // holds the next block filter
    next_filter: Option<(BlockId, BlockFilter)>,
    // best height counter
    height: Height,
    // stop height, used to tell when to stop
    stop: Height,
}

impl<'a, C: RpcApi, K> Client<'a, C, K> {
    /// Get [`BlockFilter`] by hash.
    fn get_filter(&self, hash: &BlockHash) -> Result<BlockFilter, Error> {
        let filter_bytes = self.client.get_block_filter(hash)?.filter;
        Ok(BlockFilter::new(&filter_bytes))
    }

    /// Get the next filter and increment the current best height.
    ///
    /// Returns `Ok(None)` when the stop height is exceeded.
    fn next_filter_increment(&mut self) -> Result<Option<(BlockId, BlockFilter)>, Error> {
        if self.height > self.stop {
            return Ok(None);
        }
        let height = self.height;
        let hash = match self.blocks.get(&height) {
            Some(h) => *h,
            None => self.client.get_block_hash(height as u64)?,
        };
        let filter = self.get_filter(&hash)?;
        self.height += 1;
        Ok(Some((BlockId { height, hash }, filter)))
    }

    /// Find the remote tip and determine the starting height to scan from.
    ///
    /// Returns the [`BlockId`] of the remote tip if it differs from that of
    /// the last local checkpoint, or else `None`.
    fn get_tip(&mut self) -> Result<Option<BlockId>, Error> {
        // To ensure consistency, cache the ten most recent block ids.
        self.blocks = {
            let mut map = BTreeMap::new();
            let tip_hash = self.client.get_best_block_hash()?;
            let mut header_info = self.client.get_block_header_info(&tip_hash)?;
            let tip_height = header_info.height as u32;
            map.insert(tip_height, tip_hash);
            for _ in 0..9 {
                if header_info.previous_block_hash.is_none() {
                    break;
                }
                let hash = header_info
                    .previous_block_hash
                    .expect("prev hash wasn't None");
                header_info = self.client.get_block_header_info(&hash)?;
                let height = header_info.height as u32;
                map.insert(height, hash);
            }
            map
        };
        let tip_height = self
            .blocks
            .keys()
            .copied()
            .last()
            .expect("blocks not empty");
        let tip_hash = self.blocks[&tip_height];
        let local_height = self.cp.height();
        let local_hash = self.cp.hash();
        if local_height == tip_height && local_hash == tip_hash {
            // nothing to do
            return Ok(None);
        }
        // Force the starting height to be the minimum of (tip_height - 9) and (local_height + 1).
        let min_update_height = self.blocks.keys().next().expect("must have fetched blocks");
        self.height = cmp::min(*min_update_height, local_height + 1);
        self.stop = tip_height;

        // Get the first filter
        self.next_filter = self.next_filter_increment()?;

        Ok(Some(BlockId {
            height: tip_height,
            hash: tip_hash,
        }))
    }
}

impl<'a, C, K> Client<'a, C, K>
where
    C: RpcApi,
    K: fmt::Debug + Clone + Ord,
{
    /// Sync to the new remote tip and return a new [`Update`].
    pub fn sync(mut self) -> Result<Option<Update<K>>, Error> {
        if self.get_tip()?.is_none() {
            return Ok(None);
        }

        let mut it = FilterIter::new(&mut self);
        while let Some(res) = it.next() {
            let event = res?;
            match event {
                // Index tx data
                Event::Block(inner) => {
                    let EventInner { height, block } = inner;
                    let _ = it.indexed_graph.apply_block_relevant(&block, height);
                }
                // No match
                Event::NoMatch => continue,
            }
        }

        // Query mempool for unconfirmed txs
        if self.include_mempool {
            let unconfirmed = mempool(self.client, self.cp.clone())?;
            let _ = self.indexed_graph.batch_insert_relevant_unconfirmed(
                unconfirmed.iter().map(|(tx, time)| (tx, *time)),
            );
        }

        // Construct update
        let tip = self.chain_update()?;
        let indexed_tx_graph = self.indexed_graph;

        Ok(Some(Update {
            tip,
            indexed_tx_graph,
        }))
    }
}

/// Type that produces [`Event`]s by matching a set of SPKs against a [`bip158::BlockFilter`].
#[derive(Debug)]
struct FilterIter<'b, 'a, C, K> {
    inner: &'b mut Client<'a, C, K>,
}

impl<'b, 'a, C, K> FilterIter<'b, 'a, C, K> {
    fn new(inner: &'b mut Client<'a, C, K>) -> Self {
        Self { inner }
    }
}

impl<'b, 'a, C: RpcApi, K> Iterator for FilterIter<'b, 'a, C, K> {
    type Item = Result<Event, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let (id, filter) = self.inner.next_filter.clone()?;

        (|| -> Result<_, Error> {
            // If the next filter matches any of our watched SPKs, get the block
            // and return it, inserting any new block ids into self.
            let height = id.height;
            let hash = id.hash;
            let event = if filter
                .match_any(&hash, self.spks.iter().map(|script| script.as_bytes()))
                .map_err(Error::Bip158)?
            {
                let block = self.client.get_block(&hash)?;
                self.blocks.insert(height, hash);
                let inner = EventInner { height, block };
                Event::Block(inner)
            } else {
                Event::NoMatch
            };

            // Fetch and cache the next filter
            self.next_filter = self.inner.next_filter_increment()?;

            Ok(Some(event))
        })()
        .transpose()
    }
}

impl<'b, 'a, C, K> ops::Deref for FilterIter<'b, 'a, C, K> {
    type Target = Client<'a, C, K>;

    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

impl<'b, 'a, C, K> ops::DerefMut for FilterIter<'b, 'a, C, K> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner
    }
}

/// Emit mempool transactions, alongside their first-seen unix timestamps.
///
/// This will internally call the corresponding method on [`Emitter`](crate::Emitter).
/// See [`super::Emitter::mempool`].
fn mempool<C: RpcApi>(client: &C, cp: CheckPoint) -> Result<Vec<(Transaction, UnixTime)>, Error> {
    // since this is a dummy Emitter, we can use a start height of 0.
    let mut emitter = super::Emitter::new(client, cp, 0);
    emitter.mempool().map_err(Error::Rpc)
}

impl<'a, C: RpcApi, K> Client<'a, C, K> {
    /// Fetches block either from `self` or from the RPC client, ensuring
    /// the block then exists in `self` (used to form a chain update).
    fn fetch_block(&mut self, height: Height) -> Result<BlockId, Error> {
        if height == 0 {
            return Ok(self.cp.get(height).expect("must have genesis").block_id());
        }
        let hash = match self.blocks.get(&height) {
            Some(hash) => *hash,
            None => {
                let hash = self.client.get_block_hash(height as _)?;
                self.blocks.insert(height, hash);
                hash
            }
        };
        Ok(BlockId { height, hash })
    }

    /// Craft the new update tip.
    fn chain_update(&mut self) -> Result<CheckPoint, Error> {
        let mut base = Option::<BlockId>::None;

        // find PoA
        for local_cp in self.cp.iter() {
            let height = local_cp.height();
            let remote_hash = self.fetch_block(height)?.hash;
            if local_cp.hash() == remote_hash {
                base = Some(local_cp.block_id());
                break;
            }
        }

        let mut cp = CheckPoint::new(base.expect("must find PoA"));
        for block in self.blocks.iter().map(BlockId::from) {
            cp = cp.insert(block);
        }

        Ok(cp)
    }
}

/// An update returned from a compact filters [`sync`](Client::sync).
#[derive(Debug)]
pub struct Update<K> {
    /// Chain tip
    pub tip: CheckPoint,
    /// Indexed tx-graph
    pub indexed_tx_graph: IndexedTxGraph<ConfirmationBlockTime, KeychainTxOutIndex<K>>,
}

/// Event inner type.
#[derive(Debug, Clone)]
struct EventInner {
    /// Height
    height: Height,
    /// Block
    block: Block,
}

/// Type of event emitted by [`FilterIter`].
#[derive(Debug, Clone)]
enum Event {
    /// Block
    Block(EventInner),
    /// No match
    NoMatch,
}

/// Errors that may occur during a compact filters sync.
#[derive(Debug)]
pub enum Error {
    /// [`bip158::Error`]
    Bip158(bip158::Error),
    /// [`bitcoincore_rpc::Error`]
    Rpc(bitcoincore_rpc::Error),
}

impl From<bitcoincore_rpc::Error> for Error {
    fn from(e: bitcoincore_rpc::Error) -> Self {
        Self::Rpc(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bip158(e) => e.fmt(f),
            Self::Rpc(e) => e.fmt(f),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}
