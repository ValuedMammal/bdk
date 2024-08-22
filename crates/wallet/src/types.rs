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

use alloc::boxed::Box;
use bdk_transaction::CandidateUtxo;
use core::convert::AsRef;

use bdk_chain::ConfirmationTime;
use bitcoin::{
    psbt,
    transaction::{OutPoint, Sequence, TxOut},
    Weight,
};

use serde::{Deserialize, Serialize};

/// Types of keychains
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum KeychainKind {
    /// External keychain, used for deriving recipient addresses.
    External = 0,
    /// Internal keychain, used for deriving change addresses.
    Internal = 1,
}

impl KeychainKind {
    /// Return [`KeychainKind`] as a byte
    pub fn as_byte(&self) -> u8 {
        match self {
            KeychainKind::External => b'e',
            KeychainKind::Internal => b'i',
        }
    }
}

impl AsRef<[u8]> for KeychainKind {
    fn as_ref(&self) -> &[u8] {
        match self {
            KeychainKind::External => b"e",
            KeychainKind::Internal => b"i",
        }
    }
}

/// An unspent output owned by a [`Wallet`].
///
/// [`Wallet`]: crate::Wallet
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalOutput {
    /// Reference to a transaction output
    pub outpoint: OutPoint,
    /// Transaction output
    pub txout: TxOut,
    /// Type of keychain
    pub keychain: KeychainKind,
    /// Whether this UTXO is spent or not
    pub is_spent: bool,
    /// The derivation index for the script pubkey in the wallet
    pub derivation_index: u32,
    /// The confirmation time for transaction containing this utxo
    pub confirmation_time: ConfirmationTime,
}

/// A [`Utxo`] with its `satisfaction_weight`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WeightedUtxo {
    /// The weight of the witness data and `scriptSig` expressed in [weight units]. This is used to
    /// properly maintain the feerate when adding this input to a transaction during coin selection.
    ///
    /// [weight units]: https://en.bitcoin.it/wiki/Weight_units
    pub satisfaction_weight: Weight,
    /// The UTXO
    pub utxo: Utxo,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
/// An unspent transaction output (UTXO).
pub enum Utxo {
    /// A UTXO owned by the local wallet.
    Local(LocalOutput),
    /// A UTXO owned by another wallet.
    Foreign {
        /// The location of the output.
        outpoint: OutPoint,
        /// The nSequence value to set for this input.
        sequence: Option<Sequence>,
        /// The information about the input we require to add it to a PSBT.
        // Box it to stop the type being too big.
        psbt_input: Box<psbt::Input>,
    },
}

impl Utxo {
    /// Get the location of the UTXO
    pub fn outpoint(&self) -> OutPoint {
        match &self {
            Utxo::Local(local) => local.outpoint,
            Utxo::Foreign { outpoint, .. } => *outpoint,
        }
    }

    /// Get the `TxOut` of the UTXO
    pub fn txout(&self) -> &TxOut {
        match &self {
            Utxo::Local(local) => &local.txout,
            Utxo::Foreign {
                outpoint,
                psbt_input,
                ..
            } => {
                if let Some(prev_tx) = &psbt_input.non_witness_utxo {
                    return &prev_tx.output[outpoint.vout as usize];
                }

                if let Some(txout) = &psbt_input.witness_utxo {
                    return txout;
                }

                unreachable!("Foreign UTXOs will always have one of these set")
            }
        }
    }

    /// Get the sequence number if an explicit sequence number has to be set for this input.
    pub fn sequence(&self) -> Option<Sequence> {
        match self {
            Utxo::Local(_) => None,
            Utxo::Foreign { sequence, .. } => *sequence,
        }
    }
}

impl From<WeightedUtxo> for CandidateUtxo {
    fn from(utxo: WeightedUtxo) -> Self {
        CandidateUtxo {
            outpoint: utxo.utxo.outpoint(),
            satisfaction_weight: utxo.satisfaction_weight,
            txout: Some(utxo.utxo.txout().clone()),
            sequence: utxo.utxo.sequence(),
            confirmation_time: match utxo.utxo {
                Utxo::Local(ref output) => Some(output.confirmation_time),
                _ => None,
            },
            psbt_input: match utxo.utxo {
                Utxo::Local(_) => Box::new(psbt::Input::default()),
                Utxo::Foreign { psbt_input, .. } => psbt_input,
            },
        }
    }
}

impl From<CandidateUtxo> for WeightedUtxo {
    fn from(utxo: bdk_transaction::CandidateUtxo) -> Self {
        WeightedUtxo {
            satisfaction_weight: utxo.satisfaction_weight,
            utxo: Utxo::Foreign {
                outpoint: utxo.outpoint,
                sequence: utxo.sequence,
                psbt_input: utxo.psbt_input,
            },
        }
    }
}

impl LocalOutput {
    /// Satisfies change policy.
    pub(crate) fn satisfies_change_policy(
        &self,
        change_policy: &bdk_transaction::ChangeSpendPolicy,
    ) -> bool {
        use bdk_transaction::ChangeSpendPolicy::*;
        match change_policy {
            ChangeAllowed => true,
            OnlyChange => self.keychain == KeychainKind::Internal,
            ChangeForbidden => self.keychain == KeychainKind::External,
        }
    }
}

#[cfg(test)]
mod test {
    use crate::{KeychainKind, LocalOutput};

    use bdk_transaction::ChangeSpendPolicy;
    use bitcoin::{OutPoint, TxOut};
    use chain::ConfirmationTime;

    use std::vec::Vec;

    fn get_test_utxos() -> Vec<LocalOutput> {
        use bitcoin::hashes::Hash;

        vec![
            LocalOutput {
                outpoint: OutPoint {
                    txid: bitcoin::Txid::from_slice(&[0; 32]).unwrap(),
                    vout: 0,
                },
                txout: TxOut::NULL,
                keychain: KeychainKind::External,
                is_spent: false,
                confirmation_time: ConfirmationTime::Unconfirmed { last_seen: 0 },
                derivation_index: 0,
            },
            LocalOutput {
                outpoint: OutPoint {
                    txid: bitcoin::Txid::from_slice(&[0; 32]).unwrap(),
                    vout: 1,
                },
                txout: TxOut::NULL,
                keychain: KeychainKind::Internal,
                is_spent: false,
                confirmation_time: ConfirmationTime::Confirmed {
                    height: 32,
                    time: 42,
                },
                derivation_index: 1,
            },
        ]
    }

    #[test]
    fn test_change_spend_policy_default() {
        let change_spend_policy = ChangeSpendPolicy::default();
        let filtered = get_test_utxos()
            .into_iter()
            .filter(|u| u.satisfies_change_policy(&change_spend_policy))
            .count();

        assert_eq!(filtered, 2);
    }

    #[test]
    fn test_change_spend_policy_no_internal() {
        let change_spend_policy = ChangeSpendPolicy::ChangeForbidden;
        let filtered = get_test_utxos()
            .into_iter()
            .filter(|u| u.satisfies_change_policy(&change_spend_policy))
            .collect::<Vec<_>>();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].keychain, KeychainKind::External);
    }

    #[test]
    fn test_change_spend_policy_only_internal() {
        let change_spend_policy = ChangeSpendPolicy::OnlyChange;
        let filtered = get_test_utxos()
            .into_iter()
            .filter(|u| u.satisfies_change_policy(&change_spend_policy))
            .collect::<Vec<_>>();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].keychain, KeychainKind::Internal);
    }
}
