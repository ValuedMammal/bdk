use anyhow::Context;
use bdk_bitcoind_rpc::compact_filter;
use bdk_chain::keychain::KeychainTxOutIndex;
use bdk_chain::local_chain;
use bdk_chain::local_chain::LocalChain;
use bdk_chain::miniscript::Descriptor;
use bdk_chain::ConfirmationTimeHeightAnchor;
use bdk_chain::IndexedTxGraph;
use bdk_testenv::anyhow::Result;
use bdk_testenv::bitcoind::BitcoinD;
use bdk_testenv::bitcoind::Conf;
use bitcoin::constants::genesis_block;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::{Address, Amount, BlockHash, Network, Txid};
use bitcoincore_rpc;
use bitcoincore_rpc::Auth;
use bitcoincore_rpc::RpcApi;

#[allow(unused)]
struct TestEnv {
    bitcoind: BitcoinD,
    client: bitcoincore_rpc::Client,
}

impl TestEnv {
    fn new() -> Result<Self> {
        let client = new_client()?;
        let mut conf = Conf::default();
        conf.args.push("-blockfilterindex=1");
        conf.args.push("-peerblockfilters=1");
        let exe = std::env::var("TEST_BITCOIND")?;
        let bitcoind = BitcoinD::with_conf(exe, &conf)?;

        Ok(Self { bitcoind, client })
    }

    fn client(&self) -> &bitcoincore_rpc::Client {
        &self.client
    }

    fn mine_blocks(&self, blocks: u64, addr: &Address) -> Result<Vec<BlockHash>> {
        Ok(self.client.generate_to_address(blocks, addr)?)
    }

    fn send_tx(&self, address: &Address, amount: Amount) -> Result<Txid> {
        Ok(self
            .client
            .send_to_address(address, amount, None, None, None, None, Some(1), None)?)
    }

    fn block_until_height(&self, height: u64) -> Result<()> {
        let mut delay = std::time::Duration::from_millis(64);
        while self.client.get_block_count()? < height {
            std::thread::sleep(delay);
            delay *= 2;
        }
        Ok(())
    }
}

fn new_client() -> Result<bitcoincore_rpc::Client> {
    let path = std::env::var("RPC_COOKIE").context("must set RPC_COOKIE env var")?;
    Ok(bitcoincore_rpc::Client::new(
        "127.0.0.1:18443",
        Auth::CookieFile(path.into()),
    )?)
}

/// Test keychain kind
#[allow(unused)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Keychain {
    External,
    Internal,
}

const DESC: &str = "tr([7d94197e/86'/1'/0']tpubDCyQVJj8KzjiQsFjmb3KwECVXPvMwvAxxZGCP9XmWSopmjW3bCV3wD7TgxrUhiGSueDS1MU5X1Vb1YjYcp8jitXc5fXfdC1z68hDDEyKRNr/0/*)";

/// Given a new LocalChain, and a remote tip height of 101,
/// when we send a tx from the mining node to the receiver
/// expect:
///     local chain is synced to the new tip
///     new anchors inserted into graph
///     tx graph shows a confirmed balance equal to the send amt
///     keychain indices updated
#[test]
#[ignore]
fn test_sync() -> Result<()> {
    let env = TestEnv::new()?;
    let core = env.client();

    // Setup receiving chain and graph structures.
    let secp = Secp256k1::new();
    let (descriptor, _) = Descriptor::parse_descriptor(&secp, DESC)?;

    let (mut chain, _) =
        LocalChain::from_genesis_hash(genesis_block(Network::Regtest).block_hash());
    let last_cp = chain.tip();
    assert_eq!(last_cp.height(), 0);

    let keychain = Keychain::External;
    let mut graph =
        IndexedTxGraph::<ConfirmationTimeHeightAnchor, KeychainTxOutIndex<Keychain>>::new({
            let mut index = KeychainTxOutIndex::default();
            index.add_keychain(keychain.clone(), descriptor.clone());
            index
        });
    let spk = graph.index.spk_at_index(keychain.clone(), 0).unwrap();
    let recv_addr = bitcoin::Address::from_script(spk, Network::Regtest).unwrap();

    // Mine blocks, and send tx to receiver.
    let _ = core.create_wallet("test", None, None, None, None)?;
    let miner = core.get_new_address(None, None).unwrap().assume_checked();
    let _ = env.mine_blocks(101, &miner)?;
    env.block_until_height(101)?;

    let send_amt = Amount::from_btc(0.21)?;
    let sent_txid = env.send_tx(&recv_addr, send_amt)?;
    let _ = env.mine_blocks(1, &miner)?;
    env.block_until_height(102)?;

    let remote_tip_height = core.get_block_count()? as u32;
    assert_eq!(remote_tip_height, 102);

    // Sync
    let mut req = compact_filter::Request::<Keychain>::new(last_cp);
    let target_index = 9;
    req.add_descriptor(keychain.clone(), descriptor, target_index);
    let mut client = req.build_client(core);
    let compact_filter::Update {
        tip,
        indexed_tx_graph,
    } = client.sync()?;

    // Apply updates
    let _ = chain.apply_update(local_chain::Update {
        tip,
        introduce_older_blocks: true,
    })?;
    let _ = graph.apply_changeset(indexed_tx_graph.initial_changeset());

    // chain updated
    assert!(chain.iter_checkpoints().collect::<Vec<_>>().len() > 1);
    let cp = chain.tip();
    assert_eq!(cp.height(), remote_tip_height);

    // graph populated with tx data
    let indexed_outpoints = graph.index.outpoints().clone();
    assert!(!indexed_outpoints.is_empty());
    let op = indexed_outpoints.iter().next().unwrap().1;
    assert_eq!(op.txid, sent_txid);
    let balance = graph
        .graph()
        .balance(&chain, cp.block_id(), indexed_outpoints, |_, _| true);
    assert_eq!(balance.confirmed, send_amt.to_sat());
    let (anchor, _) = graph.graph().all_anchors().iter().next().unwrap();
    assert_eq!(anchor.confirmation_height, remote_tip_height);

    // keychains updated
    assert_eq!(
        graph.index.last_revealed_index(&keychain),
        Some(target_index)
    );

    Ok(())
}
