use bdk_bitcoind_rpc::compact_filter;
use bdk_chain::bitcoin::{constants::genesis_block, secp256k1::Secp256k1, Network};
use bdk_chain::indexer::keychain_txout::KeychainTxOutIndex;
use bdk_chain::local_chain::LocalChain;
use bdk_chain::miniscript::Descriptor;
use bdk_chain::{BlockId, ConfirmationBlockTime, IndexedTxGraph};
use bdk_testenv::anyhow;

// This example shows how BDK chain and tx-graph structures are updated using compact filters syncing.
// assumes a local Signet node, and "RPC_COOKIE" set in environment.

// Usage: `cargo run -p bdk_bitcoind_rpc --example compact_filter`

const EXTERNAL: &str = "tr([83737d5e/86h/1h/0h]tpubDDR5GgtoxS8fJyjjvdahN4VzV5DV6jtbcyvVXhEKq2XtpxjxBXmxH3r8QrNbQqHg4bJM1EGkxi7Pjfkgnui9jQWqS7kxHvX6rhUeriLDKxz/0/*)";
const INTERNAL: &str = "tr([83737d5e/86h/1h/0h]tpubDDR5GgtoxS8fJyjjvdahN4VzV5DV6jtbcyvVXhEKq2XtpxjxBXmxH3r8QrNbQqHg4bJM1EGkxi7Pjfkgnui9jQWqS7kxHvX6rhUeriLDKxz/1/*)";
const SPK_COUNT: u32 = 20;
const NETWORK: Network = Network::Signet;

fn main() -> anyhow::Result<()> {
    // Setup receiving chain and graph structures.
    let secp = Secp256k1::new();
    let (descriptor, _) = Descriptor::parse_descriptor(&secp, EXTERNAL)?;
    let (change_descriptor, _) = Descriptor::parse_descriptor(&secp, INTERNAL)?;
    let (mut chain, _) = LocalChain::from_genesis_hash(genesis_block(NETWORK).block_hash());
    let mut graph = IndexedTxGraph::<ConfirmationBlockTime, KeychainTxOutIndex<usize>>::new({
        let mut index = KeychainTxOutIndex::default();
        index.insert_descriptor(0, descriptor.clone())?;
        index.insert_descriptor(1, change_descriptor.clone())?;
        index
    });

    // Assume a wallet birthday height
    let _ = chain
        .insert_block(BlockId {
            height: 205_000,
            hash: "0000002bd0f82f8c0c0f1e19128f84c938763641dba85c44bdb6aed1678d16cb"
                .parse()
                .unwrap(),
        })
        .unwrap();

    // Configure RPC client
    let rpc_client = bitcoincore_rpc::Client::new(
        "127.0.0.1:38332",
        bitcoincore_rpc::Auth::CookieFile(std::env::var("RPC_COOKIE")?.into()),
    )?;

    // Build request. note the type parameter `()` matches the
    // keychain kind of the receiver.
    let mut request = compact_filter::Request::<usize>::new(chain.tip());
    for (k, desc) in graph.index.keychains() {
        request.add_descriptor(k, desc.clone(), 0..SPK_COUNT);
    }

    let client = request.build_client(&rpc_client);

    // Sync
    if let Some(compact_filter::Update {
        tip,
        indexed_tx_graph,
    }) = client.sync()?
    {
        // Apply updates
        let _ = chain.apply_update(tip)?;
        graph.apply_changeset(indexed_tx_graph.initial_changeset());
    }

    let cp = chain.tip();
    let index = &graph.index;
    let outpoints = index.outpoints().clone();
    let unspent: Vec<_> = graph
        .graph()
        .filter_chain_unspents(&chain, cp.block_id(), outpoints.clone())
        .map(|(_, txo)| txo)
        .collect();
    for utxo in unspent {
        let spk = utxo.txout.script_pubkey;
        println!("Funded: {}", bitcoin::Address::from_script(&spk, NETWORK)?);
    }
    println!("Local tip: {} : {}", cp.height(), cp.hash());
    println!("Last revealed indices: {:?}", index.last_revealed_indices());
    println!(
        "Balance: {:#?}",
        graph
            .graph()
            .balance(&chain, cp.block_id(), outpoints, |_, _| true),
    );

    Ok(())
}
