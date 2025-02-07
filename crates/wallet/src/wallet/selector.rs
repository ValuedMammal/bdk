//! [`Selector`]

use alloc::vec::Vec;
use bdk_coin_select::{
    metrics, Candidate, ChangePolicy, CoinSelector, Drain, DrainWeights, FeeRate,
    InsufficientFunds, Target, TargetFee, TargetOutputs,
};
use bitcoin::{Amount, Weight};

use crate::CoinSelectionStrategy::{self, *};

/// Minimum value of a change output in satoshis
const MIN_VALUE: u64 = 1_000;
/// Maximum number of rounds to run branch and bound
const BNB_MAX_ROUNDS: usize = 1_000;

#[derive(Debug, Copy, Clone)]
pub struct Options {
    pub fee: TargetFee,
    pub change_policy: ChangePolicy,
    pub strategy: CoinSelectionStrategy,
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
            strategy: CoinSelectionStrategy::BranchAndBound,
            select_to: 0,
        }
    }
}

/// A helper struct for doing coin selection
#[derive(Debug)]
pub struct Selector<'a, T> {
    utxos: &'a [T],
    outputs: TargetOutputs,
    pub options: Options,
}

impl<'a, T> Selector<'a, T> {
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

impl<T> Selector<'_, T> {
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

    /// Select from the available candidates according to the selection strategy and
    /// return the selection and drain if selection was successful or else an
    /// insufficient funds error.
    ///
    /// See [`Options`] for configurable options including the target feerate, change
    /// policy, and selection strategy. The default selection strategy is [`BranchAndBound`]
    /// which is suitable in most cases unless a special selection order is required
    /// in which case this method will not reorder candidates but select them in the order
    /// in which they were passed in via [`Selector::new`].
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

        let mut selector = CoinSelector::new(&candidates);

        let Options {
            fee,
            change_policy,
            strategy,
            select_to,
        } = self.options;

        let target = Target {
            fee,
            outputs: self.outputs,
        };

        for _ in 0..select_to {
            if !selector.select_next() {
                break;
            }
        }

        if selector.unselected().next().is_none() {
            let value_sum = selector.selected_value() - change_policy.min_value;
            let mut trg = target;
            if let TargetOutputs {
                value_sum: 0,
                weight_sum: 0,
                n_outputs: 0,
            } = trg.outputs
            {
                // All candidates are selected but we have no recipients, which means we
                // could be doing a sweep. We treat the drain as a recipient in the next
                // calculation, if the implied fee given the target exceeds the total value,
                // then we fail selection. This can happen if the feerate is high enough to
                // consume the entire drain amount.
                trg.outputs = TargetOutputs {
                    value_sum,
                    weight_sum: change_policy.drain_weights.output_weight,
                    n_outputs: 1,
                };
            }
            let fee = selector.implied_fee(trg, change_policy.drain_weights);
            if fee > value_sum {
                let missing = fee.saturating_sub(value_sum);
                return Err(InsufficientFunds { missing });
            }
        }
        if let BranchAndBound = strategy {
            let metric = metrics::Changeless {
                target,
                change_policy,
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

        let drain = selector.drain(target, change_policy);

        Ok((selection, drain))
    }
}
