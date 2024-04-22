use std::path::PathBuf;
use std::sync::Mutex;
use std::time;

use bdk_bitcoind_rpc::{
    bitcoincore_rpc::{Auth, Client, RpcApi},
    compact_filter::{Request, Update},
};
use bdk_chain::{
    bitcoin::{constants::genesis_block, hashes::Hash, BlockHash},
    BlockId,
};
use bdk_chain::{
    indexed_tx_graph, keychain,
    local_chain::{self, LocalChain},
    ConfirmationTimeHeightAnchor, IndexedTxGraph,
};
use example_cli::{
    anyhow,
    clap::{self, Args, Subcommand},
    Keychain,
};

const DB_MAGIC: &[u8] = b"bdk_example_cbf";
const DB_PATH: &str = ".bdk_example_cbf.db";

/// Default lookahead
const DEFAULT_LOOKAHEAD: u32 = 10;

type ChangeSet = (
    local_chain::ChangeSet,
    indexed_tx_graph::ChangeSet<ConfirmationTimeHeightAnchor, keychain::ChangeSet<Keychain>>,
);

#[derive(Args, Debug, Clone)]
struct RpcArgs {
    /// RPC URL
    #[clap(env = "RPC_URL", long, default_value = "127.0.0.1:38332")]
    url: String,
    /// RPC auth cookie file
    #[clap(env = "RPC_COOKIE", long)]
    rpc_cookie: Option<PathBuf>,
    /// RPC auth username
    #[clap(env = "RPC_USER", long)]
    rpc_user: Option<String>,
    /// RPC auth password
    #[clap(env = "RPC_PASS", long)]
    rpc_password: Option<String>,

    /// Assume a wallet birthday (this will insert a fake checkpoint at the given height).
    #[clap(long, short = 'f')]
    start: Option<u32>,
    /// Minimum number of SPKs to derive for all keychains.
    #[clap(long, short = 'd')]
    lookahead: Option<u32>,
}

impl RpcArgs {
    fn new_client(&self) -> anyhow::Result<Client> {
        Ok(Client::new(
            &self.url,
            match (&self.rpc_cookie, &self.rpc_user, &self.rpc_password) {
                (None, None, None) => Auth::None,
                (Some(path), _, _) => Auth::CookieFile(path.clone()),
                (_, Some(user), Some(pass)) => Auth::UserPass(user.clone(), pass.clone()),
                (_, Some(_), None) => panic!("rpc auth: missing rpc_pass"),
                (_, None, Some(_)) => panic!("rpc auth: missing rpc_user"),
            },
        )?)
    }
}

#[derive(Subcommand, Debug, Clone)]
enum RpcCommands {
    /// Sync from latest checkpoint
    Sync {
        #[clap(flatten)]
        rpc_args: RpcArgs,
    },
}

fn main() -> anyhow::Result<()> {
    let example_cli::Init {
        args,
        keymap,
        index,
        db,
        init_changeset,
    } = example_cli::init::<RpcCommands, RpcArgs, ChangeSet>(DB_MAGIC, DB_PATH)?;

    let (init_chain_changeset, init_graph_changeset) = init_changeset;
    let graph = Mutex::new({
        let mut graph = IndexedTxGraph::new(index);
        graph.apply_changeset(init_graph_changeset);
        graph
    });
    let chain = Mutex::new({
        let genesis_hash = genesis_block(args.network).block_hash();
        let (mut chain, _) = LocalChain::from_genesis_hash(genesis_hash);
        let _ = chain.apply_changeset(&init_chain_changeset);
        chain
    });

    let rpc_cmd = match args.command {
        example_cli::Commands::ChainSpecific(rpc_cmd) => rpc_cmd,
        general_cmd => {
            return example_cli::handle_commands(
                &graph,
                &db,
                &chain,
                &keymap,
                args.network,
                |rpc_args, tx| {
                    let client = rpc_args.new_client()?;
                    client.send_raw_transaction(tx)?;
                    Ok(())
                },
                general_cmd,
            );
        }
    };

    let start = time::Instant::now();

    match rpc_cmd {
        // Sync from the last local checkpoint
        RpcCommands::Sync { rpc_args } => {
            let rpc_client = rpc_args.new_client()?;

            let mut client = {
                let mut chain = chain.lock().unwrap();
                let graph = graph.lock().unwrap();

                // Assume we don't need data below a given height
                // note, currently this fails with `AlterCheckPointError` if a block
                // already exists locally at this height
                if let Some(height) = rpc_args.start {
                    let _ = chain.insert_block(BlockId {
                        height,
                        hash: BlockHash::all_zeros(),
                    })?;
                }

                // Build request
                let mut request = Request::<Keychain>::new(chain.tip());
                for (keychain, descriptor) in graph.index.keychains().clone() {
                    let target_index = match graph.index.last_revealed_index(&keychain) {
                        Some(i) => i,
                        None => rpc_args.lookahead.unwrap_or(DEFAULT_LOOKAHEAD),
                    };
                    request.add_descriptor(keychain, descriptor, target_index);
                    request.include_mempool();
                }
                request.build_client(&rpc_client)
            };

            // Sync
            let Update {
                tip,
                indexed_tx_graph,
            } = client.sync()?;

            {
                // Apply updates
                let mut chain = chain.lock().unwrap();
                let mut graph = graph.lock().unwrap();

                let indexed_graph_changeset = indexed_tx_graph.initial_changeset();
                graph.apply_changeset(indexed_graph_changeset.clone());
                let chain_changeset = chain.apply_update(local_chain::Update {
                    tip,
                    introduce_older_blocks: true,
                })?;
                let mut db = db.lock().unwrap();
                db.stage((chain_changeset, indexed_graph_changeset));
                db.commit()?;
            }
        }
    }

    println!("Finished sync in {}s", start.elapsed().as_secs());

    let chain = chain.lock().unwrap();
    let cp = chain.tip();
    let graph = graph.lock().unwrap();

    let indexed_outpoints = graph.index.outpoints();
    let balance = graph.graph().balance(
        &*chain,
        cp.block_id(),
        indexed_outpoints.iter().cloned(),
        |_, _| true,
    );
    println!("Local tip: {} : {}", cp.height(), cp.hash());
    println!("Balance: {:#?}", balance);

    Ok(())
}
