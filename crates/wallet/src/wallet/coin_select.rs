//! [`CoinSelector`]

use alloc::vec::Vec;
use bdk_coin_select::{
    metrics, Candidate, ChangePolicy, Drain, DrainWeights, FeeRate, InsufficientFunds, Target,
    TargetFee, TargetOutputs,
};
use bitcoin::{Amount, Weight};

use crate::UtxoOrdering;

/// Minimum value of a change output in satoshis
const MIN_VALUE: u64 = 1_000;
/// Maximum number of rounds to run branch and bound
const BNB_MAX_ROUNDS: usize = 10_000;

#[derive(Debug, Clone)]
pub struct Options {
    pub fee: TargetFee,
    pub change_policy: ChangePolicy,
    pub order: UtxoOrdering,
    /// minimum number of candidates that must be selected
    pub select_to: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            fee: TargetFee::default(),
            change_policy: ChangePolicy {
                min_value: MIN_VALUE,
                drain_weights: DrainWeights::TR_KEYSPEND,
            },
            order: UtxoOrdering::default(),
            select_to: 0,
        }
    }
}

/// A helper struct for doing coin selection
#[derive(Debug)]
pub struct CoinSelector<'a, T> {
    utxos: &'a [T],
    outputs: TargetOutputs,
    pub options: Options,
}

impl<'a, T> CoinSelector<'a, T> {
    /// New from candidate utxos and target outputs
    pub fn new(utxos: &'a [T], target_outputs: impl IntoIterator<Item = (Weight, Amount)>) -> Self {
        Self {
            utxos,
            outputs: TargetOutputs::fund_outputs(
                target_outputs
                    .into_iter()
                    .map(|(w, v)| (w.to_wu(), v.to_sat())),
            ),
            options: Options::default(),
        }
    }
}

impl<T> CoinSelector<'_, T> {
    /// Set the target feerate
    pub fn feerate(&mut self, feerate: FeeRate) {
        self.options.fee.rate = feerate;
    }

    /// Set the absolute fee
    pub fn fee_absolute(&mut self, amount: Amount) {
        self.outputs.value_sum += amount.to_sat();
    }

    /// Set the drain weights of the current change policy
    pub fn drain_weights(&mut self, drain_weights: DrainWeights) {
        self.options.change_policy.drain_weights = drain_weights;
    }

    /// Select from the list of candidate utxos and return a selection and drain if selection
    /// was successful or else an [`InsufficientFunds`] error.
    ///
    /// See [`Options`] for configurable options including the target feerate, change
    /// policy, and utxo ordering. The implementation must not reorder candidates if
    /// doing so would interfere with the chosen [`UtxoOrdering`].
    pub fn select_coins(&self) -> Result<(Vec<T>, Drain), InsufficientFunds>
    where
        T: Into<Candidate> + Clone,
    {
        let candidates = self
            .utxos
            .iter()
            .cloned()
            .map(Into::into)
            .collect::<Vec<_>>();

        let mut selector = bdk_coin_select::CoinSelector::new(&candidates);

        let opt = &self.options;

        let target = Target {
            fee: opt.fee,
            outputs: self.outputs,
        };

        for _ in 0..opt.select_to {
            if !selector.select_next() {
                break;
            }
        }

        // Attempt branch and bound if it doesn't interfere with the UtxoOrdering,
        // otherwise select from the sorted candidates until the target is met.
        if let UtxoOrdering::Shuffle = &opt.order {
            let metric = metrics::Changeless {
                target,
                change_policy: opt.change_policy,
            };
            if selector.run_bnb(metric, BNB_MAX_ROUNDS).is_err() {
                selector.select_until_target_met(target)?;
            }
        } else {
            selector.select_until_target_met(target)?;
        }

        let selection = selector
            .apply_selection(self.utxos)
            .cloned()
            .collect::<Vec<_>>();

        let drain = selector.drain(target, opt.change_policy);

        Ok((selection, drain))
    }
}
