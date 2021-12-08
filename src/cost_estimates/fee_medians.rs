use std::cmp;
use std::cmp::Ordering;
use std::convert::TryFrom;
use std::{iter::FromIterator, path::Path};

use rusqlite::AndThenRows;
use rusqlite::Transaction as SqlTransaction;
use rusqlite::{
    types::{FromSql, FromSqlError},
    Connection, Error as SqliteError, OptionalExtension, ToSql,
};
use serde_json::Value as JsonValue;

use chainstate::stacks::TransactionPayload;
use util::db::sqlite_open;
use util::db::tx_begin_immediate_sqlite;
use util::db::u64_to_sql;

use vm::costs::ExecutionCost;

use chainstate::stacks::db::StacksEpochReceipt;
use chainstate::stacks::events::TransactionOrigin;

use crate::util::db::sql_pragma;
use crate::util::db::table_exists;

use super::metrics::CostMetric;
use super::FeeRateEstimate;
use super::{EstimatorError, FeeEstimator};

use super::metrics::PROPORTION_RESOLUTION;
use cost_estimates::StacksTransactionReceipt;

const SINGLETON_ROW_ID: i64 = 1;
const CREATE_TABLE: &'static str = "
CREATE TABLE median_fee_estimator (
    measure_key INTEGER PRIMARY KEY AUTOINCREMENT,
    high NUMBER NOT NULL,
    middle NUMBER NOT NULL,
    low NUMBER NOT NULL
)";

/// This struct estimates fee rates by translating a transaction's `ExecutionCost`
/// into a scalar using a `CostMetric` (type parameter `M`) and computing
/// the subsequent fee rate using the actual paid fee. The *weighted* 5th, 50th and 95th
/// percentile fee rates for each block are used as the low, middle, and high
/// estimates. The fee rates are weighted by the scalar value for each transaction.
/// Blocks which do not exceed at least 1-dimension of the block limit are filled
/// with a rate = 1.0 transaction. Estimates are updated via the median value over
/// a parameterized window.
pub struct WeightedMedianFeeRateEstimator<M: CostMetric> {
    db: Connection,
    /// We only look back `window_size` fee rates when averaging past estimates.
    window_size: u32,
    /// The weight of a "full block" in abstract scalar cost units. This is the weight of
    /// a block that is filled on each dimension.
    full_block_weight: u64,
    metric: M,
}

/// Convenience pair for return values.
#[derive(Debug)]
struct FeeRateAndWeight {
    pub fee_rate: f64,
    pub weight: u64,
}

impl<M: CostMetric> WeightedMedianFeeRateEstimator<M> {
    /// Open a fee rate estimator at the given db path. Creates if not existent.
    pub fn open(p: &Path, metric: M, window_size: u32) -> Result<Self, SqliteError> {
        let mut db = sqlite_open(
            p,
            rusqlite::OpenFlags::SQLITE_OPEN_CREATE | rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            false,
        )?;

        // check if the db needs to be instantiated regardless of whether or not
        //  it was newly created: the db itself may be shared with other fee estimators,
        //  which would not have created the necessary table for this estimator.
        let tx = tx_begin_immediate_sqlite(&mut db)?;
        Self::instantiate_db(&tx)?;
        tx.commit()?;

        Ok(Self {
            db,
            metric,
            window_size,
            full_block_weight: 6 * PROPORTION_RESOLUTION,
        })
    }

    /// Check if the SQL database was already created. Necessary to avoid races if
    ///  different threads open an estimator at the same time.
    fn db_already_instantiated(tx: &SqlTransaction) -> Result<bool, SqliteError> {
        table_exists(tx, "median_fee_estimator")
    }

    fn instantiate_db(tx: &SqlTransaction) -> Result<(), SqliteError> {
        if !Self::db_already_instantiated(tx)? {
            tx.execute(CREATE_TABLE, rusqlite::NO_PARAMS)?;
        }

        Ok(())
    }

    fn get_rate_estimates_from_sql(
        conn: &Connection,
        window_size: u32,
    ) -> Result<FeeRateEstimate, EstimatorError> {
        let sql =
            "SELECT high, middle, low FROM median_fee_estimator ORDER BY measure_key DESC LIMIT ?";
        let mut stmt = conn.prepare(sql).expect("SQLite failure");

        // shuttle high, low, middle estimates into these lists, and then sort and find median.
        let mut highs = Vec::with_capacity(window_size as usize);
        let mut mids = Vec::with_capacity(window_size as usize);
        let mut lows = Vec::with_capacity(window_size as usize);
        let results = stmt
            .query_and_then::<_, SqliteError, _, _>(&[window_size], |row| {
                let high: f64 = row.get(0)?;
                let middle: f64 = row.get(1)?;
                let low: f64 = row.get(2)?;
                Ok((low, middle, high))
            })
            .expect("SQLite failure");

        for result in results {
            let (low, middle, high) = result.expect("SQLite failure");
            highs.push(high);
            mids.push(middle);
            lows.push(low);
        }

        if highs.is_empty() || mids.is_empty() || lows.is_empty() {
            return Err(EstimatorError::NoEstimateAvailable);
        }

        fn median(len: usize, l: Vec<f64>) -> f64 {
            if len % 2 == 1 {
                l[len / 2]
            } else {
                // note, measures_len / 2 - 1 >= 0, because
                //  len % 2 == 0 and emptiness is checked above
                (l[len / 2] + l[len / 2 - 1]) / 2f64
            }
        }

        // sort our float arrays. for float values that do not compare easily,
        //  treat them as equals.
        highs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
        mids.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
        lows.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));

        Ok(FeeRateEstimate {
            high: median(highs.len(), highs),
            middle: median(mids.len(), mids),
            low: median(lows.len(), lows),
        })
    }

    fn update_estimate(&mut self, new_measure: FeeRateEstimate) {
        let tx = tx_begin_immediate_sqlite(&mut self.db).expect("SQLite failure");

        let insert_sql = "INSERT INTO median_fee_estimator
                          (high, middle, low) VALUES (?, ?, ?)";

        let deletion_sql = "DELETE FROM median_fee_estimator
                            WHERE measure_key <= (
                               SELECT MAX(measure_key) - ?
                               FROM median_fee_estimator )";

        tx.execute(
            insert_sql,
            rusqlite::params![new_measure.high, new_measure.middle, new_measure.low,],
        )
        .expect("SQLite failure");

        tx.execute(deletion_sql, rusqlite::params![self.window_size])
            .expect("SQLite failure");

        let estimate = Self::get_rate_estimates_from_sql(&tx, self.window_size);

        tx.commit().expect("SQLite failure");

        if let Ok(next_estimate) = estimate {
            debug!("Updating fee rate estimate for new block";
                   "new_measure_high" => new_measure.high,
                   "new_measure_middle" => new_measure.middle,
                   "new_measure_low" => new_measure.low,
                   "new_estimate_high" => next_estimate.high,
                   "new_estimate_middle" => next_estimate.middle,
                   "new_estimate_low" => next_estimate.low);
        }
    }
}

impl<M: CostMetric> FeeEstimator for WeightedMedianFeeRateEstimator<M> {
    /// Compute a FeeRateEstimate for this block. Update the
    /// running estimate using this rounds estimate.
    fn notify_block(
        &mut self,
        receipt: &StacksEpochReceipt,
        block_limit: &ExecutionCost,
    ) -> Result<(), EstimatorError> {
        // Calculate sorted fee rate for each transaction in the block.
        let mut working_fee_rates: Vec<FeeRateAndWeight> = receipt
            .tx_receipts
            .iter()
            .filter_map(|tx_receipt| {
                fee_rate_and_weight_from_receipt(&self.metric, &tx_receipt, block_limit)
            })
            .collect();

        // If necessary, add the "minimum" fee rate to fill the block.
        maybe_add_minimum_fee_rate(&mut working_fee_rates, self.full_block_weight);

        // If fee rates non-empty, then compute an update.
        if working_fee_rates.len() > 0 {
            working_fee_rates.sort_by(|a, b| {
                a.fee_rate
                    .partial_cmp(&b.fee_rate)
                    .unwrap_or(Ordering::Equal)
            });

            let block_estimate = fee_rate_estimate_from_sorted_weighted_fees(&working_fee_rates);
            self.update_estimate(block_estimate);
        }

        Ok(())
    }

    fn get_rate_estimates(&self) -> Result<FeeRateEstimate, EstimatorError> {
        Self::get_rate_estimates_from_sql(&self.db, self.window_size)
    }
}

/// `sorted_fee_rates` must be non-empty.
fn fee_rate_estimate_from_sorted_weighted_fees(
    sorted_fee_rates: &Vec<FeeRateAndWeight>,
) -> FeeRateEstimate {
    if sorted_fee_rates.is_empty() {
        panic!("`sorted_fee_rates` cannot be empty.");
    }

    let mut total_weight = 0u64;
    for rate_and_weight in sorted_fee_rates {
        total_weight += rate_and_weight.weight;
    }
    let mut cumulative_weight = 0u64;
    let mut percentiles = Vec::new();
    for rate_and_weight in sorted_fee_rates {
        cumulative_weight += rate_and_weight.weight;
        let percentile_n: f64 =
            (cumulative_weight as f64 - rate_and_weight.weight as f64 / 2f64) / total_weight as f64;
        percentiles.push(percentile_n);
    }

    let target_percentiles = vec![0.05, 0.5, 0.95];
    let mut fees_index = 0; // index into `sorted_fee_rates`
    let mut values_at_target_percentiles = Vec::new();
    warn!("percentiles {:?}", &percentiles);
    warn!("sorted_fee_rates {:?}", &sorted_fee_rates);
    warn!("percentiles {:?}", &percentiles);
    for target_percentile in target_percentiles {
        while fees_index < percentiles.len() && percentiles[fees_index] < target_percentile {
            fees_index += 1;
        }
        let v = if fees_index == 0 {
            warn!("fees_index == 0");
            sorted_fee_rates[0].fee_rate
        } else if fees_index == percentiles.len() {
            warn!("fees_index == percentiles.len()");
            sorted_fee_rates.last().unwrap().fee_rate
        } else {
            warn!("fees_index < percentiles.len()");
            // Notation mimics https://en.wikipedia.org/wiki/Percentile#Weighted_percentile
            let vk = sorted_fee_rates[fees_index - 1].fee_rate;
            let vk1 = sorted_fee_rates[fees_index].fee_rate;
            let pk = percentiles[fees_index - 1];
            let pk1 = percentiles[fees_index];
            vk + (target_percentile - pk) / (pk1 - pk) * (vk1 - vk)
        };
        values_at_target_percentiles.push(v);
    }

    FeeRateEstimate {
        high: values_at_target_percentiles[2],
        middle: values_at_target_percentiles[1],
        low: values_at_target_percentiles[0],
    }
}

fn maybe_add_minimum_fee_rate(working_rates: &mut Vec<FeeRateAndWeight>, full_block_weight: u64) {
    let mut total_weight = 0u64;
    for rate_and_weight in working_rates.into_iter() {
        total_weight += rate_and_weight.weight;
    }

    warn!(
        "total_weight {} full_block_weight {}",
        total_weight, full_block_weight
    );
    if total_weight < full_block_weight {
        let weight_remaining = full_block_weight - total_weight;
        working_rates.push(FeeRateAndWeight {
            fee_rate: 1f64,
            weight: weight_remaining,
        })
    }
}

/// The fee rate is the `fee_paid/cost_metric_used`
fn fee_rate_and_weight_from_receipt(
    metric: &dyn CostMetric,
    tx_receipt: &StacksTransactionReceipt,
    block_limit: &ExecutionCost,
) -> Option<FeeRateAndWeight> {
    let (payload, fee, tx_size) = match tx_receipt.transaction {
        TransactionOrigin::Stacks(ref tx) => {
            let fee = tx.get_tx_fee();
            warn!("fee_paid: {}", fee);
            Some((&tx.payload, tx.get_tx_fee(), tx.tx_len()))
        }
        TransactionOrigin::Burn(_) => None,
    }?;
    let scalar_cost = match payload {
        TransactionPayload::TokenTransfer(_, _, _) => {
            // TokenTransfers *only* contribute tx_len, and just have an empty ExecutionCost.
            warn!("check");
            metric.from_len(tx_size)
        }
        TransactionPayload::Coinbase(_) => {
            // Coinbase txs are "free", so they don't factor into the fee market.
            warn!("check");
            return None;
        }
        TransactionPayload::PoisonMicroblock(_, _)
        | TransactionPayload::ContractCall(_)
        | TransactionPayload::SmartContract(_) => {
            // These transaction payload types all "work" the same: they have associated ExecutionCosts
            // and contibute to the block length limit with their tx_len
            warn!("check {:?}", &tx_receipt.execution_cost);
            metric.from_cost_and_len(&tx_receipt.execution_cost, &block_limit, tx_size)
        }
    };
    warn!("scalar_cost {}", scalar_cost);
    let denominator = if scalar_cost >= 1 {
        scalar_cost as f64
    } else {
        1f64
    };
    let fee_rate = fee as f64 / denominator;
    warn!("fee_rate {}", fee_rate);
    let part1 = fee_rate >= 1f64;
    let part2 = fee_rate.is_finite();
    warn!("part1 {} part2 {}", part1, part2);
    if fee_rate >= 1f64 && fee_rate.is_finite() {
        Some(FeeRateAndWeight {
            fee_rate,
            weight: scalar_cost,
        })
    } else {
        Some(FeeRateAndWeight {
            fee_rate: 1f64,
            weight: scalar_cost,
        })
    }
}