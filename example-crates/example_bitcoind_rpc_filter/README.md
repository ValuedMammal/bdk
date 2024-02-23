# README

### Usage - example_bitcoind_rpc_filter

The cli has one option `sync` as well as the normal `example_cli` commands. By default the first sync will act
as a full chain rescan, after which subsequent `sync`s will try to sync at most the revealed SPKs of all keychains
starting from the last local checkpoint. To do a full scan later, you can pass in a `--start` height of 1
and specify the number of `--lookahead` scripts to derive for each keychain. Final note that Bitcoin Core
must be configured to serve compact filters, see `blockfilterindex` in bitcoin.conf.

Below is the help text for `sync`.

```sh
$ cargo run --bin example_bitcoind_rpc_filter -- sync --help
```

```sh
Sync from latest checkpoint

USAGE:
    example_bitcoind_rpc_filter <DESCRIPTOR> sync [OPTIONS]

OPTIONS:
    -d, --lookahead <LOOKAHEAD>
            Minimum number of SPKs to derive for all keychains

    -f, --start <START>
            Assume a wallet birthday (this will insert a fake checkpoint at the given height)

    -h, --help
            Print help information

        --rpc-cookie <RPC_COOKIE>
            RPC auth cookie file [env: RPC_COOKIE=]

        --rpc-password <RPC_PASSWORD>
            RPC auth password [env: RPC_PASS=]

        --rpc-user <RPC_USER>
            RPC auth username [env: RPC_USER=]

        --url <URL>
            RPC URL [env: RPC_URL=] [default: 127.0.0.1:38332]

    -V, --version
            Print version information

```
