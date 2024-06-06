#![allow(unused)]
use std::str::FromStr;

use bdk_wallet::KeychainKind;
use bdk_wallet::SignOptions;
use bitcoin::{psbt, transaction, Address, Amount, OutPoint};

/** How to create a peel chain without broadcasting using BDK. */

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Get a wallet with a single UTXO
    let mut wallet = bdk_wallet::doctest_wallet!();
    assert_eq!(wallet.list_unspent().collect::<Vec<_>>().len(), 1);
    assert_eq!(wallet.balance().total().to_sat(), 500_000);

    // We must know the satisfaction weight to spend our change output
    let satisfaction_weight = wallet
        .public_descriptor(KeychainKind::Internal)
        .clone()
        .max_weight_to_satisfy()?;

    // Each tx will send to one of these recipients
    let recipients: Vec<Address> = [
        "bcrt1puy4u8y2xxym37e2j3dcg6whel0mfer0tkxx9fz0qmmawpkt9nd5q30huyu",
        "bcrt1p3eq947mtuy908n0u97tkhw4fvtx7qwq8ltskrujpazy00cl5n8tqnyaf90",
        "bcrt1p77yy7gsvutr70lqph7ufzu2kxpt9xg9ljrgprz0z2wwqywqylu5q27lw8h",
        "bcrt1pu42tmz6gr50kjdd29zvu5sg9ct2rl5jm5w5nfq6nct3rll6rfl2qzlrpeu",
        "bcrt1pwzmkecfah3l8jkxdh49h23hvnaxuggfdklsfa9kkwmxh5s6nr6hspelkqk",
        "bcrt1pc75r2x4x07ytwy2499fld4cj259e0v4wpkrw5fq7p4gsjl9rcfjsryw5hf",
        "bcrt1pzhslydnzrsrh6wak5wzz8hadg04arlt2xgp9dmmd046568xxwvsq3e99cv",
        "bcrt1padrwx4668yd6z59dglypkaxqzvxyanqupm6z7p4pa3r4shmp6r3svttwm4",
        "bcrt1prcvu4cmt7juk049nz5cc6lsejyzz7xh6za3magljsjyhfjfvrc3s63ndqc",
        "bcrt1ph78svd6fvpcpr2pgmwlel3hzygjgpgh8ax9gyjv3ttf50h72va4q5y2vwp",
    ]
    .into_iter()
    .map(|s| Address::from_str(s).unwrap().assume_checked())
    .collect();

    // Tx parameters
    let amt = Amount::from_sat(10_000);
    let fee = Amount::from_sat(150);

    // Create tx 0
    let mut builder = wallet.build_tx();
    builder
        .add_recipient(recipients[0].script_pubkey(), amt)
        .fee_absolute(fee);
    let mut psbt = builder.finish()?;
    let finalized = wallet.sign(&mut psbt, SignOptions::default())?;
    assert!(finalized);

    let mut tx = psbt.extract_tx()?;
    let mut txs = vec![tx.clone()];

    for n in 1..10 {
        // Find our change output
        let txid = tx.txid();
        #[rustfmt::skip]
        let (vout, txout) = tx.output.iter().cloned().enumerate().find(|(_, txout)| wallet.is_mine(&txout.script_pubkey))
            .expect("must have change output");
        let outpoint = OutPoint {
            txid,
            vout: vout as u32,
        };
        let psbt_input = psbt::Input {
            witness_utxo: Some(txout),
            ..Default::default()
        };

        // Create tx n
        let mut builder = wallet.build_tx();
        builder
            .add_foreign_utxo(outpoint, psbt_input, satisfaction_weight)?
            .add_recipient(recipients[n].script_pubkey(), amt)
            .manually_selected_only()
            .fee_absolute(fee);
        let mut psbt = builder.finish()?;
        let finalized = wallet.sign(&mut psbt, SignOptions::default())?;
        assert!(finalized);

        // Extract tx
        tx = psbt.extract_tx()?;
        if tx.output.len() < 2 {
            break;
        }
        txs.push(tx.clone());
    }

    // Do stuff with txs
    for tx in txs {
        assert_eq!(tx.input.len(), 1);

        let txin = &tx.input[0];
        assert!(!txin.witness.is_empty());
    }

    // Every tx should reveal a new internal address.
    assert_eq!(wallet.derivation_index(KeychainKind::External).unwrap(), 0);
    assert_eq!(wallet.derivation_index(KeychainKind::Internal).unwrap(), 9);

    Ok(())
}
