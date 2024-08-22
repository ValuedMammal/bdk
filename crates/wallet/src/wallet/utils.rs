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

use bdk_transaction::CandidateUtxo;
use bitcoin::{
    absolute, relative,
    secp256k1::{All, Secp256k1},
    OutPoint, Sequence,
};
use miniscript::{MiniscriptKey, Satisfier, ToPublicKey};

pub struct After {
    pub current_height: Option<u32>,
    pub assume_height_reached: bool,
}

impl After {
    pub(crate) fn new(current_height: Option<u32>, assume_height_reached: bool) -> After {
        After {
            current_height,
            assume_height_reached,
        }
    }
}

pub(crate) fn check_nsequence_rbf(sequence: Sequence, csv: Sequence) -> bool {
    // The nSequence value must enable relative timelocks
    if !sequence.is_relative_lock_time() {
        return false;
    }

    // Both values should be represented in the same unit (either time-based or
    // block-height based)
    if sequence.is_time_locked() != csv.is_time_locked() {
        return false;
    }

    // The value should be at least `csv`
    if sequence < csv {
        return false;
    }

    true
}

impl<Pk: MiniscriptKey + ToPublicKey> Satisfier<Pk> for After {
    fn check_after(&self, n: absolute::LockTime) -> bool {
        if let Some(current_height) = self.current_height {
            current_height >= n.to_consensus_u32()
        } else {
            self.assume_height_reached
        }
    }
}

pub struct Older {
    pub current_height: Option<u32>,
    pub create_height: Option<u32>,
    pub assume_height_reached: bool,
}

impl Older {
    pub(crate) fn new(
        current_height: Option<u32>,
        create_height: Option<u32>,
        assume_height_reached: bool,
    ) -> Older {
        Older {
            current_height,
            create_height,
            assume_height_reached,
        }
    }
}

impl<Pk: MiniscriptKey + ToPublicKey> Satisfier<Pk> for Older {
    fn check_older(&self, n: relative::LockTime) -> bool {
        if let Some(current_height) = self.current_height {
            // TODO: test >= / >
            current_height
                >= self
                    .create_height
                    .unwrap_or(0)
                    .checked_add(n.to_consensus_u32())
                    .expect("Overflowing addition")
        } else {
            self.assume_height_reached
        }
    }
}

pub(crate) type SecpCtx = Secp256k1<All>;

/// Remove duplicate UTXOs.
///
/// If a UTXO appears in both `required` and `optional`, the appearance in `required` is kept.
pub(crate) fn filter_duplicates<I>(required: I, optional: I) -> (I, I)
where
    I: IntoIterator<Item = CandidateUtxo> + FromIterator<CandidateUtxo>,
{
    use crate::collections::HashSet;
    let mut visited = HashSet::<OutPoint>::new();
    let required = required
        .into_iter()
        .filter(|utxo| visited.insert(utxo.outpoint))
        .collect::<I>();
    let optional = optional
        .into_iter()
        .filter(|utxo| visited.insert(utxo.outpoint))
        .collect::<I>();
    (required, optional)
}

#[cfg(test)]
mod test {
    // When nSequence is lower than this flag the timelock is interpreted as block-height-based,
    // otherwise it's time-based
    pub(crate) const SEQUENCE_LOCKTIME_TYPE_FLAG: u32 = 1 << 22;

    use super::check_nsequence_rbf;
    use crate::bitcoin::Sequence;

    #[test]
    fn test_check_nsequence_rbf_msb_set() {
        let result = check_nsequence_rbf(Sequence(0x80000000), Sequence(5000));
        assert!(!result);
    }

    #[test]
    fn test_check_nsequence_rbf_lt_csv() {
        let result = check_nsequence_rbf(Sequence(4000), Sequence(5000));
        assert!(!result);
    }

    #[test]
    fn test_check_nsequence_rbf_different_unit() {
        let result =
            check_nsequence_rbf(Sequence(SEQUENCE_LOCKTIME_TYPE_FLAG + 5000), Sequence(5000));
        assert!(!result);
    }

    #[test]
    fn test_check_nsequence_rbf_mask() {
        let result = check_nsequence_rbf(Sequence(0x3f + 10_000), Sequence(5000));
        assert!(result);
    }

    #[test]
    fn test_check_nsequence_rbf_same_unit_blocks() {
        let result = check_nsequence_rbf(Sequence(10_000), Sequence(5000));
        assert!(result);
    }

    #[test]
    fn test_check_nsequence_rbf_same_unit_time() {
        let result = check_nsequence_rbf(
            Sequence(SEQUENCE_LOCKTIME_TYPE_FLAG + 10_000),
            Sequence(SEQUENCE_LOCKTIME_TYPE_FLAG + 5000),
        );
        assert!(result);
    }
}
