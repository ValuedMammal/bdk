//! # Compact block filters
//!
//! This module provides a method of chain syncing via [`BIP157`](https://github.com/bitcoin/bips/blob/master/bip-0157.mediawiki)
//! compact block filters.
//!
//! # Example (TODO)

use core::cmp;
use core::fmt;
use core::ops::Bound;
use core::ops::RangeBounds;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

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
    pub fn build_client<C: bitcoincore_rpc::RpcApi>(self, client: &C) -> Client<C, K> {
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
            indexed_graph: IndexedTxGraph::new(indexer),
            param: Params {
                cp: self.cp,
                spks,
                include_mempool: self.include_mempool,
            },
        }
    }
}

/// Sync params.
#[derive(Debug)]
struct Params {
    cp: CheckPoint,
    spks: Vec<ScriptBuf>,
    include_mempool: bool,
}

/// Type for executing a compact filters [`sync`](Client::sync).
pub struct Client<'a, C, K> {
    client: &'a C,
    indexed_graph: IndexedTxGraph<ConfirmationBlockTime, KeychainTxOutIndex<K>>,
    param: Params,
}

impl<'a, C, K> Client<'a, C, K>
where
    C: bitcoincore_rpc::RpcApi,
    K: fmt::Debug + Clone + Ord,
{
    /// Sync to the new remote tip and return a new [`Update`].
    pub fn sync(&mut self) -> Result<Update<K>, Error> {
        let Params {
            cp,
            spks,
            include_mempool,
        } = &self.param;

        let mut filter_iter = FilterIter::new(self.client, cp.block_id(), spks);

        // Get new remote tip and consume events
        let mut blocks = vec![];
        if filter_iter.get_tip()?.is_some() {
            for res in filter_iter {
                let event = res?;
                match event {
                    // Index tx data, and collect block id
                    Event::Block(inner) => {
                        let EventInner { id, block } = inner;
                        let _ = self
                            .indexed_graph
                            .apply_block_relevant(&block.expect("must have Block"), id.height);
                        blocks.push(id);
                    }

                    // Collect block ids
                    Event::Id(inner) => blocks.push(inner.id),

                    // No match
                    Event::NoMatch => continue,
                }
            }
        }

        // Query mempool for unconfirmed txs
        if *include_mempool {
            let unconfirmed = mempool(self.client, cp.clone())?;
            let _ = self.indexed_graph.batch_insert_relevant_unconfirmed(
                unconfirmed.iter().map(|(tx, time)| (tx, *time)),
            );
        }

        // Construct update
        let tip = if blocks.is_empty() {
            cp.clone()
        } else {
            chain_update_tip(cp, blocks)
        };
        let indexed_tx_graph = core::mem::take(&mut self.indexed_graph);

        Ok(Update {
            tip,
            indexed_tx_graph,
        })
    }
}

/// Type that emits [`Event`]s by matching a set of SPKs against a [`bip158::BlockFilter`].
#[derive(Debug)]
struct FilterIter<'a, C> {
    // RPC client
    client: &'a C,
    // local tip block id
    base: BlockId,
    // raw SPK inventory
    spks: &'a [ScriptBuf],
    // block map (height, hash)
    blocks: BTreeMap<Height, BlockHash>,
    // holds the next block filter
    next_filter: Option<(BlockId, BlockFilter)>,
    // best height counter
    height: Height,
    // stop height, used to tell when to stop
    stop: Height,
}

impl<'a, C> FilterIter<'a, C>
where
    C: bitcoincore_rpc::RpcApi,
{
    /// Construct a new [`FilterIter`].
    fn new(client: &'a C, base: BlockId, spks: &'a [ScriptBuf]) -> Self {
        Self {
            client,
            base,
            spks,
            blocks: BTreeMap::new(),
            next_filter: None,
            height: 0,
            stop: 0,
        }
    }

    /// Get a [`BlockFilter`] by hash.
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
        let local_height = self.base.height;
        let local_hash = self.base.hash;
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

impl<'a, C: bitcoincore_rpc::RpcApi> Iterator for FilterIter<'a, C> {
    type Item = Result<Event, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let (id, filter) = self.next_filter.clone()?;

        (|| -> Result<Option<Event>, Error> {
            // If the next filter matches any of our watched SPKs,
            // get the block and emit the event.
            let height = id.height;
            let hash = id.hash;
            let event = if filter
                .match_any(&hash, self.spks.iter().map(|script| script.as_bytes()))
                .map_err(Error::Bip158)?
            {
                let block = self.client.get_block(&hash)?;
                let event = EventInner {
                    id,
                    block: Some(block),
                };
                Event::Block(event)

            // Always emit block ids for the most recent fetched blocks
            } else if self.blocks.contains_key(&height) {
                let event = EventInner { id, block: None };
                Event::Id(event)
            } else {
                Event::NoMatch
            };

            // Fetch and cache the next filter
            self.next_filter = self.next_filter_increment()?;

            Ok(Some(event))
        })()
        .transpose()
    }
}

/// Emit mempool transactions, alongside their first-seen unix timestamps.
///
/// This will internally call the corresponding method on [`Emitter`](crate::Emitter).
/// See [`super::Emitter::mempool`].
fn mempool<C: bitcoincore_rpc::RpcApi>(
    client: &C,
    cp: CheckPoint,
) -> Result<Vec<(Transaction, UnixTime)>, Error> {
    // since this is a dummy Emitter, we can use a start height of 0.
    let mut emitter = super::Emitter::new(client, cp, 0);
    emitter.mempool().map_err(Error::Rpc)
}

/// Craft the new update tip
fn chain_update_tip(cp: &CheckPoint, block_ids: impl IntoIterator<Item = BlockId>) -> CheckPoint {
    let block_ids: BTreeSet<BlockId> = block_ids.into_iter().collect();
    let min_update_height = block_ids
        .iter()
        .next()
        .expect("blocks must not be empty")
        .height;
    let local_height = cp.height();

    let base = if local_height >= min_update_height {
        // find next lowest base to build on
        cp.iter()
            .find(|cp| cp.height() < min_update_height)
            .expect("fallback to genesis")
            .block_id()
    } else {
        cp.block_id()
    };

    CheckPoint::new(base)
        .extend(block_ids)
        .expect("blocks are well ordered")
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
    /// Block id
    id: BlockId,
    /// Block
    block: Option<Block>,
}

/// Type of event emitted by [`FilterIter`].
#[derive(Debug, Clone)]
enum Event {
    /// Block
    Block(EventInner),
    /// Block Id
    Id(EventInner),
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
