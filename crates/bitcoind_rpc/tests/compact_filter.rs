use bdk_bitcoind_rpc::compact_filter;
use bdk_chain::keychain::KeychainTxOutIndex;
use bdk_chain::local_chain;
use bdk_chain::local_chain::LocalChain;
use bdk_chain::miniscript::Descriptor;
use bdk_chain::ConfirmationTimeHeightAnchor;
use bdk_chain::IndexedTxGraph;
use bdk_testenv::anyhow::Result;
use bdk_testenv::bitcoind;
use bdk_testenv::bitcoind::BitcoinD;
use bdk_testenv::bitcoind::Conf;
use bitcoin::constants::genesis_block;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::{Address, Amount, BlockHash, Network, Txid};
use bitcoincore_rpc;
use bitcoincore_rpc::RpcApi;

#[allow(unused)]
struct TestEnv {
    bitcoind: BitcoinD,
}

impl TestEnv {
    fn new() -> Result<Self> {
        let mut conf = Conf::default();
        conf.args.push("-blockfilterindex=1");
        conf.args.push("-peerblockfilters=1");
        let exe = std::env::var("TEST_BITCOIND").unwrap_or(bitcoind::downloaded_exe_path()?);
        let bitcoind = BitcoinD::with_conf(exe, &conf)?;

        Ok(Self { bitcoind })
    }

    fn client(&self) -> &impl bitcoincore_rpc::RpcApi {
        &self.bitcoind.client
    }

    fn mine_blocks(&self, blocks: u64, addr: &Address) -> Result<Vec<BlockHash>> {
        Ok(self.client().generate_to_address(blocks, addr)?)
    }

    fn send_tx(&self, address: &Address, amount: Amount) -> Result<Txid> {
        Ok(self
            .client()
            .send_to_address(address, amount, None, None, None, None, Some(1), None)?)
    }

    fn block_until_height(&self, height: u64) -> Result<()> {
        let mut delay = std::time::Duration::from_millis(64);
        while self.client().get_block_count()? < height {
            std::thread::sleep(delay);
            delay *= 2;
        }
        Ok(())
    }
}

/// Test keychain kind
#[allow(unused)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Keychain {
    External,
    Internal,
}

const DESC: &str = "tr([7d94197e/86'/1'/0']tpubDCyQVJj8KzjiQsFjmb3KwECVXPvMwvAxxZGCP9XmWSopmjW3bCV3wD7TgxrUhiGSueDS1MU5X1Vb1YjYcp8jitXc5fXfdC1z68hDDEyKRNr/0/*)";

/// Given initial params:
///     - keychain, descriptor, and target_index
///     - remote tip height of 101,
///
/// When we send a tx from the mining node to the receiver,
/// expect chain changeset:
///     - contains the new blocks
///
/// expect indexed_tx_graph changeset:
///     - includes sent tx
///     - new confirmation height anchor
///     - last_revealed keychain index
///
/// finally, expect computed balance equals send amt
#[test]
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
    req.add_descriptor(keychain.clone(), descriptor, 0..=target_index);
    let mut client = req.build_client(core);
    let compact_filter::Update {
        tip,
        indexed_tx_graph,
    } = client.sync()?;

    // Apply updates
    let chain_changeset = chain.apply_update(local_chain::Update {
        tip,
        introduce_older_blocks: true,
    })?;

    // chain updated
    assert_eq!(chain_changeset.len(), 10);
    let cp = chain.tip();
    assert_eq!(cp.height(), remote_tip_height);

    // new tx added
    let graph_init_changeset = indexed_tx_graph.initial_changeset();
    let tx = graph_init_changeset.graph.txs.iter().next().unwrap();
    let txout = tx
        .output
        .iter()
        .find(|txo| txo.script_pubkey == spk.to_owned())
        .unwrap();
    assert_eq!(txout.value, send_amt);

    // new anchor added
    let (anchor, txid) = graph_init_changeset.graph.anchors.iter().next().unwrap();
    assert_eq!(anchor.confirmation_height, 102);
    assert_eq!(txid, &sent_txid);

    // last revealed of keychain equal to `target_index`
    let (k, last_revealed) = graph_init_changeset.indexer.0.iter().next().unwrap();
    assert_eq!(k, &keychain);
    assert_eq!(last_revealed, &target_index);
    let _ = graph.apply_changeset(graph_init_changeset);

    // balance updated
    let balance = graph.graph().balance(
        &chain,
        cp.block_id(),
        graph.index.outpoints().clone(),
        |_, _| true,
    );
    assert_eq!(balance.confirmed, send_amt.to_sat());

    Ok(())
}
