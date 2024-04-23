use bdk_bitcoind_rpc::compact_filter;
use bdk_chain::bitcoin::{constants::genesis_block, secp256k1::Secp256k1, Network};
use bdk_chain::keychain::KeychainTxOutIndex;
use bdk_chain::local_chain;
use bdk_chain::local_chain::LocalChain;
use bdk_chain::miniscript::Descriptor;
use bdk_chain::{BlockId, ConfirmationTimeHeightAnchor, IndexedTxGraph};

// This example shows how BDK chain and tx-graph structures are updated using compact filters syncing.
// assumes a local Signet node, and "RPC_COOKIE" set in environment.

// Run: `$ cargo run -p bdk_bitcoind_rpc --example compact_filter`

const DESC: &str = "tr([7d94197e/86'/1'/0']tpubDCyQVJj8KzjiQsFjmb3KwECVXPvMwvAxxZGCP9XmWSopmjW3bCV3wD7TgxrUhiGSueDS1MU5X1Vb1YjYcp8jitXc5fXfdC1z68hDDEyKRNr/0/*)";

fn main() -> anyhow::Result<()> {
    // Setup receiving chain and graph structures.
    let secp = Secp256k1::new();
    let (descriptor, _) = Descriptor::parse_descriptor(&secp, DESC)?;
    let (mut chain, _) = LocalChain::from_genesis_hash(genesis_block(Network::Signet).block_hash());
    let mut graph = IndexedTxGraph::<ConfirmationTimeHeightAnchor, KeychainTxOutIndex<()>>::new({
        let mut index = KeychainTxOutIndex::default();
        index.add_keychain((), descriptor.clone());
        index
    });

    // Assume a wallet birthday height
    let _ = chain
        .insert_block(BlockId {
            height: 165_000,
            hash: "0000001643565a1eaed9e38e5f8ab998ee20cc8f8bb76e7c3ad2250f3c9c3fa0"
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
    let mut request = compact_filter::Request::<()>::new(chain.tip());
    request.add_descriptor((), descriptor, 0..10);

    let mut client = request.build_client(&rpc_client);

    // Sync
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

    println!(
        "Local tip: {} : {}",
        chain.tip().height(),
        chain.tip().hash()
    );
    println!(
        "Last revealed index: {:?}",
        graph.index.last_revealed_index(&())
    );
    println!(
        "Balance: {:#?}",
        graph.graph().balance(
            &chain,
            chain.tip().block_id(),
            graph.index.outpoints().clone(),
            |_, _| true
        ),
    );

    Ok(())
}
