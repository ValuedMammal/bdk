use std::net::TcpStream;
use std::sync::Mutex;
use std::thread;

use bdk_chain::bitcoin;
use bdk_chain::bitcoin::constants::genesis_block;
use bdk_chain::{
    indexed_tx_graph, keychain,
    local_chain::{self, LocalChain},
    ConfirmationTimeHeightAnchor, IndexedTxGraph,
};
use bdk_nakamoto::client;
use bdk_nakamoto::client::Client;
use bdk_nakamoto::client::Config;
use bdk_nakamoto::client::Handle;
use bdk_nakamoto::net_poll::Reactor;
use bdk_nakamoto::net_poll::Waker;
use bdk_nakamoto::ClientHandle;
use example_cli::{
    anyhow::{self, Result},
    clap::{self, Args, Subcommand},
    Keychain,
};

const DB_MAGIC: &[u8] = b"bdk_nakamoto_example";
const DB_PATH: &str = ".bdk_nakamoto_example.db";

/// Number of SPKs to sync if none have been revealed.
const _DEFAULT_LOOKAHEAD: u32 = 25;

type ChangeSet = (
    local_chain::ChangeSet,
    indexed_tx_graph::ChangeSet<ConfirmationTimeHeightAnchor, keychain::ChangeSet<Keychain>>,
);

#[derive(Debug, Clone, Subcommand)]
enum CbfCommands {
    /// Get tip, useful for testing
    Tip,
    /// Scan a range of heights
    Scan {
        /// Args
        #[clap(flatten)]
        args: CbfArgs,
    },
}

#[derive(Args, Debug, Clone)]
struct CbfArgs {
    /// Start height
    #[clap(long, short = 'f')]
    start: Option<u32>,
    /// Stop height
    #[clap(long, short = 't')]
    stop: Option<u32>,
}

fn into_client_network(network: bitcoin::Network) -> client::Network {
    match network {
        bitcoin::Network::Bitcoin => client::Network::Mainnet,
        bitcoin::Network::Testnet => client::Network::Testnet,
        bitcoin::Network::Regtest => client::Network::Regtest,
        bitcoin::Network::Signet => client::Network::Signet,
        _ => panic!("nonexhaustive enum"),
    }
}

// New nakamoto Client.
fn client() -> Client<Reactor<TcpStream>> {
    Client::<Reactor<TcpStream>>::new().unwrap()
}

fn main() -> Result<()> {
    pretty_env_logger::init();

    let example_cli::Init {
        args,
        keymap,
        index,
        db,
        init_changeset,
    } = example_cli::init::<CbfCommands, CbfArgs, ChangeSet>(DB_MAGIC, DB_PATH)?;

    let (init_chain_changeset, init_indexed_tx_graph_changeset) = init_changeset;
    let graph = Mutex::new({
        let mut graph = IndexedTxGraph::new(index);
        graph.apply_changeset(init_indexed_tx_graph_changeset);
        graph
    });
    let (chain, cp) = {
        let genesis_hash = genesis_block(args.network).block_hash();
        let (mut chain, _) = LocalChain::from_genesis_hash(genesis_hash);
        chain.apply_changeset(&init_chain_changeset)?;
        let cp = chain.tip();
        (Mutex::new(chain), cp)
    };

    let cmd = match &args.command {
        example_cli::Commands::ChainSpecific(cmd) => cmd,
        general_cmd => {
            return example_cli::handle_commands(
                &graph,
                &db,
                &chain,
                &keymap,
                args.network,
                |_, tx| {
                    let client = client();
                    let handle = client.handle();
                    thread::spawn(move || {
                        client
                            .run(Config::new(into_client_network(args.network)))
                            .expect("run client");
                    });
                    let client_handle =
                        ClientHandle::<Handle<Waker>, Keychain>::new_wait_for_peers(handle, cp, 1)
                            .expect("new client handle");
                    client_handle
                        .submit_transaction(tx)
                        .map(|_| ())
                        .map_err(anyhow::Error::from)
                },
                general_cmd.clone(),
            );
        }
    };

    // Setup `ClientHandle`
    let (mut handle, client) = {
        let graph = graph.lock().unwrap();
        let chain = chain.lock().unwrap();
        log::info!("Last local height: {}", chain.tip().height());

        let request = bdk_nakamoto::Request::new(chain.tip(), &graph.index);
        let client = client();
        let handle = client.handle();
        let handle: ClientHandle<_, _> = request.into_client_handle(handle);
        (handle, client)
    };

    // Run the client on another thread, so as not to block main.
    log::info!("Starting client");
    let network: client::Network = into_client_network(args.network);
    thread::spawn(move || client.run(Config::new(network)).unwrap());

    // Allow client to catch up with network
    let (tip_height, tip_hash) = handle.get_tip()?;
    log::info!("Connecting to peers at height {tip_height} : {tip_hash}");

    // Inspect spks
    let mut handle = handle.inspect_spks(|idx, spk| {
        let addr = bitcoin::Address::from_script(spk, bitcoin::Network::Signet).unwrap();
        log::info!("Watching Address at index {idx}: {addr}");
    });

    match cmd {
        // sync the client only
        CbfCommands::Tip => {}
        // scan a given range
        CbfCommands::Scan { args } => {
            let start = args.start.unwrap_or(cp.height());
            let end = args.stop.unwrap_or(tip_height);
            if start != end {
                log::info!("Scanning filters!");

                let bdk_nakamoto::Update {
                    chain_update,
                    indexed_graph,
                } = handle.rescan(start..=end)?;

                {
                    let mut graph = graph.lock().unwrap();
                    let mut chain = chain.lock().unwrap();
                    let mut db = db.lock().unwrap();

                    let chain_cs = chain.apply_update(chain_update)?;
                    let graph_cs = indexed_graph.initial_changeset();
                    graph.apply_changeset(graph_cs.clone());
                    db.stage((chain_cs, graph_cs));
                    db.commit()?;
                }
            }
        }
    }

    let cp = chain.lock().unwrap().tip();
    log::info!("Synced to height {}", cp.height());
    log::info!("Shutting down");
    handle.shutdown();

    Ok(())
}
