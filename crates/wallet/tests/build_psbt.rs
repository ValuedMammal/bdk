use bdk_wallet::KeychainKind::External;
use bdk_wallet::{
    error::{BuildPsbtError, CreateTxError},
    test_utils::*,
    SignOptions,
};

use bitcoin::{Amount, FeeRate};

#[test]
fn build_psbt_success() -> anyhow::Result<()> {
    let (mut wallet, _) = get_funded_wallet_wpkh();

    let addr = wallet.reveal_next_address(External);
    let mut builder = wallet.build_tx();
    builder.add_recipient(addr.script_pubkey(), Amount::from_sat(25_000));
    let mut psbt = builder.finish_psbt()?;
    let fee = psbt.fee()?;

    // expect 1 input, 2 outputs
    assert_eq!(psbt.unsigned_tx.input.len(), 1);
    assert_eq!(psbt.unsigned_tx.output.len(), 2);

    // sign it
    assert!(wallet.sign(&mut psbt, SignOptions::default())?);
    let tx = psbt.extract_tx()?;

    // it was a self spend, so all that we spent was the fee
    let net_value = wallet.spk_index().net_value(&tx, ..);
    assert!(net_value.is_negative());
    assert_eq!(net_value.to_sat().abs(), fee.to_sat() as i64);

    Ok(())
}

#[test]
fn build_psbt_no_recipients_error() {
    let (mut wallet, _) = get_funded_wallet_wpkh();
    assert_eq!(wallet.balance().total().to_sat(), 50_000);

    assert!(matches!(
        wallet.build_tx().finish_psbt(),
        Err(BuildPsbtError::CreateTx(CreateTxError::NoRecipients))
    ));
}

#[test]
fn build_psbt_legacy_wallet() {
    let desc = "pkh(tprv8ZgxMBicQKsPdy6LMhUtFHAgpocR8GC6QmwMSFpZs7h6Eziw3SpThFfczTDh5rW2krkqffa11UpX3XkeTTB2FvzZKWXqPY54Y6Rq4AQ5R8L/0/*)";
    let (mut wallet, _) = get_funded_wallet_single(desc);

    let addr = wallet.reveal_next_address(External);
    let mut builder = wallet.build_tx();
    builder.add_recipient(addr.script_pubkey(), Amount::from_sat(25_000));
    builder.fee_rate(FeeRate::from_sat_per_kwu(625));
    let mut psbt = builder.finish_psbt().unwrap();
    assert!(wallet.sign(&mut psbt, SignOptions::default()).unwrap());
    let _tx = psbt.extract_tx().unwrap();
    let feerate = wallet.calculate_fee_rate(&_tx).unwrap();
    let feerate = bdk_wallet::floating_rate!(feerate);
    assert!(feerate > 2.5 && feerate < 3.0);
}
