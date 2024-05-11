#![doc = include_str!("../README.md")]

//! # Example (TODO)

#![warn(missing_docs)]

use core::cmp;
use core::fmt;
use core::ops::RangeBounds;
use std::collections::BTreeMap;
use std::str::FromStr;

use bdk_chain::bitcoin::{BlockHash, ScriptBuf, Transaction, Txid};
use bdk_chain::keychain::KeychainTxOutIndex;
use bdk_chain::local_chain;
use bdk_chain::local_chain::CheckPoint;
use bdk_chain::miniscript::Descriptor;
use bdk_chain::miniscript::DescriptorPublicKey;
use bdk_chain::BlockId;
use bdk_chain::ConfirmationTimeHeightAnchor;
use bdk_chain::IndexedTxGraph;
use nakamoto::client::chan;
use nakamoto::client::handle::Handle;
use nakamoto::client::Event;
use nakamoto::common::bitcoin::Script;
use nakamoto::common::bitcoin_hashes::hex::FromHex;
use nakamoto::common::network::Services;

pub use nakamoto::client;
pub use nakamoto::net::poll as net_poll;

/// Block height.
type Height = u32;

/// Type for watching a ranged descriptor.
#[allow(unused)]
#[derive(Debug, Clone)]
struct Watch<K> {
    /// Keychain
    keychain: K,
    /// Descriptor
    descriptor: Descriptor<DescriptorPublicKey>,
    /// Range
    range: (u32, u32),
}

/// Request.
pub struct Request<K> {
    cp: CheckPoint,
    watching: Vec<Watch<K>>,
}

impl<K> Request<K> {
    /// New.
    pub fn new(last_cp: CheckPoint) -> Self {
        Self {
            cp: last_cp,
            watching: vec![],
        }
    }
}

impl<K> Request<K>
where
    K: fmt::Debug + Clone + Ord,
{
    /// Add descriptor.
    pub fn add_descriptor(
        &mut self,
        keychain: &K,
        descriptor: Descriptor<DescriptorPublicKey>,
        range: impl RangeBounds<u32>,
    ) {
        let start = match range.start_bound() {
            std::ops::Bound::Included(i) => *i,
            std::ops::Bound::Excluded(i) => *i + 1,
            std::ops::Bound::Unbounded => u32::MIN,
        };
        let end = match range.end_bound() {
            std::ops::Bound::Included(i) => *i,
            std::ops::Bound::Excluded(i) => *i - 1,
            std::ops::Bound::Unbounded => u32::MAX,
        };
        self.watching.push(Watch {
            keychain: keychain.clone(),
            descriptor,
            range: (start, end),
        });
    }

    /// Into client handle.
    pub fn into_client_handle<H: Handle>(self, handle: H) -> ClientHandle<H, K> {
        // derive spks
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
            indexer.add_keychain(keychain.clone(), descriptor);
            let (spk_iter, _) = indexer.reveal_to_target(&keychain, end);
            for (spk_index, spk) in spk_iter {
                if spk_index >= start {
                    spks.push(spk);
                }
            }
        }

        let mut handle = ClientHandle::new(handle, self.cp);
        handle.spks = Box::new(spks.into_iter());
        handle.graph = IndexedTxGraph::new(indexer);
        handle
    }
}

/// Type for managing block filters sync.
pub struct ClientHandle<H, K> {
    // client handle
    handle: H,
    // script pubkeys to watch
    spks: Box<dyn Iterator<Item = ScriptBuf>>,
    // block map
    blocks: BTreeMap<Height, BlockHash>,
    // indexed graph
    graph: IndexedTxGraph<ConfirmationTimeHeightAnchor, KeychainTxOutIndex<K>>,

    /* Sync params */
    cp: CheckPoint,
    stop: Height,

    /* Counters */
    height: Height,
    filters_matched: usize,
    blocks_matched: usize,
}

impl<H: Handle, K> ClientHandle<H, K> {
    /// Construct a new [`ClientHandle`].
    pub fn new(handle: H, cp: CheckPoint) -> Self {
        Self {
            handle,
            spks: Box::new(core::iter::empty()),
            blocks: BTreeMap::new(),
            cp,
            height: 0,
            stop: 0,
            filters_matched: 0,
            blocks_matched: 0,
            graph: IndexedTxGraph::default(),
        }
    }

    /// Construct a new [`ClientHandle`], and block until `count` peer connections are made.
    ///
    // Currently this is only used in example cli to submit a tx.
    pub fn new_wait_for_peers(handle: H, cp: CheckPoint, count: usize) -> Result<Self, Error> {
        let mut handle = Self::new(handle, cp);
        handle.wait_for_peers(count)?;
        Ok(handle)
    }

    /// Get a reference to the client handle.
    pub fn handle(&self) -> &H {
        &self.handle
    }

    /// Adds a closure to be called for each inspected spk.
    pub fn inspect_spks<F>(mut self, f: F) -> Self
    where
        F: Fn(&ScriptBuf) + 'static,
    {
        self.spks = Box::new(self.spks.inspect(move |spk| f(spk)));
        self
    }

    /// Waits for `count` peer connections and returns the best height of connected peers.
    fn wait_for_peers(&mut self, count: usize) -> Result<Height, Error> {
        let peers = self
            .handle
            .wait_for_peers(count, Services::default())
            .map_err(Error::Handle)?;

        let height = peers
            .iter()
            .map(|(_, h, _)| *h)
            .max()
            .expect("must have peers");
        let height = height as u32;
        self.stop = height;

        Ok(height)
    }
}

impl<H, K> ClientHandle<H, K>
where
    H: Handle,
    K: fmt::Debug + Clone + Ord,
{
    /// Get the new remote tip.
    ///
    /// The client will attempt to find peers and catch up to the best peer height.
    pub fn get_tip(&mut self) -> Result<(Height, BlockHash), Error> {
        self.blocks.clear();

        // wait for client to reach peer_height before getting the tip
        let peer_height = self.wait_for_peers(1)?;
        if self.cp.height() < peer_height {
            let _ = self
                .handle
                .wait_for_height(peer_height as u64)
                .map_err(Error::Handle)?;
        }

        let (height, mut header, _work) = self.handle.get_tip().map_err(Error::Handle)?;
        let tip_height = height as u32;
        let tip_hash = nakamoto_str_hash(&header.block_hash());
        self.blocks.insert(tip_height, tip_hash);

        // fetch recent blocks from the client's header store.
        for _ in 0..9 {
            let hash = header.prev_blockhash;
            match self.handle.get_block(&hash).map_err(Error::Handle)? {
                Some(height_header) => {
                    let (height, prev_header) = height_header;
                    self.blocks.insert(height as u32, nakamoto_str_hash(&hash));
                    header = prev_header;
                }
                None => break,
            }
        }

        Ok((tip_height, tip_hash))
    }

    /// Rescan the chain over a given `range` of heights.
    pub fn rescan(&mut self, range: impl RangeBounds<u32>) -> Result<Update<K>, Error> {
        use std::ops::Bound;
        // make sure we have a new remote tip
        if self.blocks.is_empty() {
            let _ = self.get_tip()?;
        }

        // map heights to u64
        let start = match range.start_bound() {
            Bound::Included(n) => *n as u64,
            Bound::Excluded(n) => *n as u64 + 1,
            Bound::Unbounded => u64::MIN,
        };
        let end = match range.end_bound() {
            Bound::Included(n) => *n as u64,
            Bound::Excluded(n) => *n as u64 - 1,
            Bound::Unbounded => u64::MAX,
        };

        // initiate rescan
        let watch = self
            .spks
            .as_mut()
            .map(|s| Script::from_hex(&s.to_hex_string()).expect("parse Script"));
        let _ = self
            .handle
            .rescan(start..=end, watch)
            .map_err(Error::Handle);

        // process events
        let _ = self.handle_events();

        // create update
        self.as_update()
    }

    /// Conditions that signal a sync is complete.
    fn synced(&self) -> bool {
        let chain_synced = self.height >= self.stop;
        chain_synced && (self.filters_matched == 0 || self.blocks_matched == self.filters_matched)
    }

    /// Handle events.
    ///
    /// On each block event, apply Block to the graph if it matches our watch list,
    /// inserting (height, hash) checkpoints into the blocks map along the way. We also
    /// need to increment the last height seen to know when to stop.
    fn handle_events(&mut self) -> Result<(), Error> {
        let recv = self.handle.events();

        loop {
            if self.synced() {
                return Ok(());
            }

            chan::select! {
                recv(recv) -> event => {
                    match event.map_err(Error::Receiver)? {
                        // Filter processed - count it if matched
                        Event::FilterProcessed { height, matched, .. } => {
                            if matched {
                                self.filters_matched += 1;
                            }
                            let height: u32 = height.try_into().expect("valid height");
                            self.height = cmp::max(self.height, height);
                        }

                        // Block matched - count and apply the block
                        Event::BlockMatched { height, block } => {
                            self.blocks_matched += 1;
                            let height = height.try_into().expect("valid height");
                            let _ = self.graph.apply_block_relevant(&nakamoto_serde_block(&block), height);

                            let hash = nakamoto_str_hash(&block.block_hash());
                            self.blocks.insert(height, hash);
                            self.height = cmp::max(self.height, height);
                        }

                        // Block connected - insert block id
                        Event::BlockConnected { height, .. } => {
                            // do we need this?
                            let height: u32 = height.try_into().expect("valid height");
                            self.height = cmp::max(self.height, height);
                        }

                        // Scanned up to this height
                        Event::FilterRescanStopped { height } => {
                            let height = height.try_into().expect("valid height");
                            self.height = cmp::max(self.height, height);
                        }

                        // Ignore everything else
                        _ => {},
                    }
                }
            }
        }
    }

    /// As [`Update`].
    fn as_update(&mut self) -> Result<Update<K>, Error> {
        // build chain update
        let min_update_height = self.blocks.keys().next().expect("blocks must not be empty");
        let base = if self.cp.height() >= *min_update_height {
            let mut cp = self.cp.clone();
            for local_cp in self.cp.iter() {
                if local_cp.height() < *min_update_height {
                    cp = local_cp;
                    break;
                }
            }
            cp.block_id()
        } else {
            self.cp.block_id()
        };

        let mut blocks = vec![base];
        blocks.extend(
            self.blocks()
                .map(|(&height, &hash)| BlockId { height, hash }),
        );
        let tip = CheckPoint::from_block_ids(blocks).expect("blocks are well ordered");
        let chain_update = local_chain::Update {
            tip,
            introduce_older_blocks: true,
        };

        let indexed_graph = core::mem::take(&mut self.graph);

        Ok(Update {
            chain_update,
            indexed_graph,
        })
    }
}

impl<H: Handle, K> ClientHandle<H, K> {
    /// Get this [`ClientHandle`]'s map of cached blocks as an iterator of (height, hash) tuples.
    pub fn blocks(&self) -> impl Iterator<Item = (&Height, &BlockHash)> {
        self.blocks.iter()
    }

    /// Submit a [`Transaction`].
    pub fn submit_transaction(&self, tx: &Transaction) -> Result<Txid, Error> {
        let _peer_addr = self
            .handle
            .submit_transaction(bdk_serde_tx(tx))
            .map_err(Error::Handle)?;
        Ok(tx.txid())
    }

    /// Shutdown the node process.
    ///
    // note: it would be nice if we made this part of a Drop impl, but it appears to require
    // that Handle impl Default
    pub fn shutdown(self) {
        self.handle.shutdown().expect("handle shutdown");
    }
}

/// Update.
#[derive(Debug)]
pub struct Update<K> {
    /// Local chain update
    pub chain_update: local_chain::Update,
    /// Indexed tx-graph
    pub indexed_graph: IndexedTxGraph<ConfirmationTimeHeightAnchor, KeychainTxOutIndex<K>>,
}

/// Crate error
#[derive(Debug)]
pub enum Error {
    /// Nakamoto client
    Client(client::Error),
    /// Handle
    Handle(client::handle::Error),
    /// Crossbeam channel receiver
    Receiver(chan::RecvError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(e) => e.fmt(f),
            Self::Handle(e) => e.fmt(f),
            Self::Receiver(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

/* FIXME: make bdk and nakamoto depend on the same version of rust-bitcoin, and these won't be necessary */
/// Maps a nakamoto `&BlockHash` to a bdk `Blockhash`.
fn nakamoto_str_hash(hash: &nakamoto::common::bitcoin::BlockHash) -> bdk_chain::bitcoin::BlockHash {
    bdk_chain::bitcoin::BlockHash::from_str(&hash.to_string()).expect("parse BlockHash")
}

/// Maps a bdk `&Transaction` to a nakamoto `Transaction`.
fn bdk_serde_tx(tx: &Transaction) -> nakamoto::common::bitcoin::Transaction {
    use nakamoto::common::bitcoin::consensus::deserialize;
    let data = bdk_chain::bitcoin::consensus::serialize(tx);
    deserialize(&data).expect("deserialize Transaction")
}

/// Maps a nakamoto `&Block` to a bdk `Block`.
fn nakamoto_serde_block(block: &nakamoto::common::bitcoin::Block) -> bdk_chain::bitcoin::Block {
    use bdk_chain::bitcoin::consensus::deserialize;
    let data = nakamoto::common::bitcoin::consensus::serialize(block);
    deserialize(&data).expect("deserialize Block")
}
