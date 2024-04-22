//! # Compact block filters
//!
//! This module provides a method of chain syncing via [`BIP157`](https://github.com/bitcoin/bips/blob/master/bip-0157.mediawiki)
//! compact block filters.
//!
//! # Example (TODO)

use core::cmp;
use core::fmt;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use bdk_chain::bitcoin::{
    bip158::{self, BlockFilter},
    Block, BlockHash, ScriptBuf, Transaction,
};
use bdk_chain::keychain::KeychainTxOutIndex;
use bdk_chain::local_chain::CheckPoint;
use bdk_chain::miniscript::Descriptor;
use bdk_chain::miniscript::DescriptorPublicKey;
use bdk_chain::{BlockId, ConfirmationTimeHeightAnchor, IndexedTxGraph};
use bitcoincore_rpc;

/// Block height
type Height = u32;

/// Unix time
type UnixTime = u64;

/// A keychain descriptor to be derived up to a target index.
#[derive(Debug, Clone)]
struct Watch<K> {
    keychain: K,
    descriptor: Descriptor<DescriptorPublicKey>,
    target_index: u32,
}

/// A sync request.
#[derive(Debug)]
pub struct Request<K> {
    watching: Vec<Watch<K>>,
    cp: CheckPoint,
    include_mempool: bool,
}

impl<K: fmt::Debug + Clone + Ord> Request<K> {
    /// Create a new [`Request`] from the last local checkpoint.
    pub fn new(last_cp: CheckPoint) -> Self {
        Self {
            watching: vec![],
            cp: last_cp,
            include_mempool: false,
        }
    }

    /// Adds a descriptor to watch up to the specified `target_index`.
    pub fn add_descriptor(
        &mut self,
        keychain: K,
        descriptor: Descriptor<DescriptorPublicKey>,
        target_index: u32,
    ) -> &mut Self {
        self.watching.push(Watch {
            keychain,
            descriptor,
            target_index,
        });
        self
    }

    /// Whether to include relevant unconfirmed transactions in this request.
    pub fn include_mempool(&mut self) -> &mut Self {
        self.include_mempool = true;
        self
    }

    /// Finish building the request and return a new [`Client`] with the given RPC `client`.
    pub fn build_client<C: bitcoincore_rpc::RpcApi>(self, client: &C) -> Client<C, K> {
        // index and reveal the requested SPKs
        let mut indexer = KeychainTxOutIndex::default();
        for watch in self.watching {
            let Watch {
                keychain,
                descriptor,
                target_index,
            } = watch;
            indexer.add_keychain(keychain.clone(), descriptor);
            let _ = indexer.reveal_to_target(&keychain, target_index);
        }

        Client {
            client,
            indexed_graph: IndexedTxGraph::new(indexer),
            request: Request {
                watching: vec![],
                cp: self.cp,
                include_mempool: self.include_mempool,
            },
        }
    }
}

/// Type for executing a compact filters [`sync`](Client::sync).
#[derive(Debug)]
pub struct Client<'a, C, K> {
    client: &'a C,
    indexed_graph: IndexedTxGraph<ConfirmationTimeHeightAnchor, KeychainTxOutIndex<K>>,
    request: Request<K>,
}

impl<'a, C, K> Client<'a, C, K>
where
    C: bitcoincore_rpc::RpcApi,
    K: fmt::Debug + Clone + Ord,
{
    /// Sync to the new remote tip and return a new [`Update`].
    pub fn sync(&mut self) -> Result<Update<K>, Error> {
        // Create `Emitter` from the local tip `BlockId` and SPK inventory.
        let spks = self
            .indexed_graph
            .index
            .revealed_spks()
            .map(|(_k, _i, spk)| spk.to_owned());

        let mut emitter = Emitter::new(self.client, self.request.cp.block_id(), spks);

        // Get new remote tip and consume events
        let mut blocks = vec![];
        if emitter.get_tip()?.is_some() {
            while let Some(event) = emitter.next_block()? {
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
                    _ => continue,
                }
            }
        }

        // Query mempool for unconfirmed txs
        if self.request.include_mempool {
            let unconfirmed = self.mempool()?;
            let _ = self.indexed_graph.batch_insert_relevant_unconfirmed(
                unconfirmed.iter().map(|(tx, time)| (tx, *time)),
            );
        }

        // Construct update
        let tip = if !blocks.is_empty() {
            chain_update_tip(self.request.cp.clone(), blocks)
        } else {
            self.request.cp.clone()
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
struct Emitter<'a, C> {
    // RPC client
    client: &'a C,
    // local tip block id
    base: BlockId,
    // raw SPK inventory
    spks: Vec<ScriptBuf>,
    // block map (height, hash)
    blocks: BTreeMap<Height, BlockHash>,
    // holds the next block filter
    next_filter: Option<(BlockId, BlockFilter)>,
    // best height counter
    height: Height,
    // stop height, used to tell when to stop
    stop: Height,
}

impl<'a, C> Emitter<'a, C>
where
    C: bitcoincore_rpc::RpcApi,
{
    /// Construct a new [`Emitter`].
    fn new(client: &'a C, base: BlockId, spks: impl IntoIterator<Item = ScriptBuf>) -> Self {
        Self {
            client,
            base,
            spks: spks.into_iter().collect(),
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

        // Force the starting height to be the minimum of (local_height + 1) and (tip_height - 9).
        let min_update_height = tip_height.saturating_sub(9);
        self.height = cmp::min(min_update_height, local_height + 1);
        self.stop = tip_height;

        // Get the first filter
        self.next_filter = self.next_filter_increment()?;

        Ok(Some(BlockId {
            height: tip_height,
            hash: tip_hash,
        }))
    }

    /// Emits the next [`Event`]. Returns `Ok(None)` when all requested filters
    /// have been processed.
    fn next_block(&mut self) -> Result<Option<Event>, Error> {
        let (id, filter) = match self.next_filter.clone() {
            Some(f) => f,
            None => return Ok(None),
        };

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
            Some(Event::Block(event))

        // Always emit block ids for the most recent fetched blocks
        } else if self.blocks.contains_key(&height) {
            let event = EventInner { id, block: None };
            Some(Event::Id(event))
        } else {
            Some(Event::NoMatch)
        };

        // Fetch and cache the next filter
        self.next_filter = self.next_filter_increment()?;

        Ok(event)
    }
}

impl<'a, C, K> Client<'a, C, K>
where
    C: bitcoincore_rpc::RpcApi,
{
    /// Emit mempool transactions, alongside their first-seen unix timestamps.
    ///
    /// This will internally call the corresponding method on [`Emitter`](crate::Emitter).
    /// See [`super::Emitter::mempool`].
    fn mempool(&mut self) -> Result<Vec<(Transaction, UnixTime)>, Error> {
        // since this is a dummy Emitter, we can use a start height of 0.
        let mut emitter = super::Emitter::new(self.client, self.request.cp.clone(), 0);
        emitter.mempool().map_err(Error::Rpc)
    }
}

/// Craft the new update tip
fn chain_update_tip(cp: CheckPoint, block_ids: impl IntoIterator<Item = BlockId>) -> CheckPoint {
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
    pub indexed_tx_graph: IndexedTxGraph<ConfirmationTimeHeightAnchor, KeychainTxOutIndex<K>>,
}

/// Event inner type.
#[derive(Debug, Clone)]
struct EventInner {
    /// Block id
    id: BlockId,
    /// Block
    block: Option<Block>,
}

/// Type of event emitted by [`Emitter`].
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
