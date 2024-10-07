// Bitcoin Dev Kit
// Written in 2020 by Alekos Filini <alekos.filini@gmail.com>
//
// Copyright (c) 2020-2021 Bitcoin Dev Kit Developers
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.
use bitcoin::psbt::Psbt;
use rand_core::RngCore;

use crate::error::CreateTxError;
use crate::Wallet;

use bdk_transaction::{coin_selection::CoinSelectionAlgorithm, CreateTx, TxBuilder};

/// Extends [`TxBuilder`] to allow building a transaction using the system RNG,
/// requires "std" feature.
pub trait TxBuilderExt {
    /// Create a new unsigned PSBT.
    fn finish(&mut self) -> Result<Psbt, CreateTxError>;
}

#[cfg(feature = "std")]
impl<'a, Cs: CoinSelectionAlgorithm> TxBuilderExt for TxBuilder<'a, Cs, Wallet> {
    fn finish(&mut self) -> Result<Psbt, CreateTxError> {
        self.finish_with_aux_rand(&mut bitcoin::key::rand::thread_rng())
    }
}

impl CreateTx for Wallet {
    type Error = CreateTxError;

    fn create_tx(
        &mut self,
        coin_selection: impl CoinSelectionAlgorithm,
        params: bdk_transaction::TxParams,
        rng: &mut impl RngCore,
    ) -> Result<Psbt, Self::Error> {
        self.create_tx(coin_selection, params, rng)
    }
}
