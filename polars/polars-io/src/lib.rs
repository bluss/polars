#![cfg_attr(docsrs, feature(doc_cfg))]
pub mod csv;
pub mod csv_core;
#[cfg(feature = "ipc")]
#[cfg_attr(docsrs, doc(cfg(feature = "ipc")))]
pub mod ipc;
#[cfg(feature = "json")]
#[cfg_attr(docsrs, doc(cfg(feature = "json")))]
pub mod json;
#[cfg(feature = "parquet")]
#[cfg_attr(docsrs, doc(cfg(feature = "feature")))]
pub mod parquet;
pub mod prelude;

use arrow::{
    csv::Reader as ArrowCsvReader, error::Result as ArrowResult, json::Reader as ArrowJsonReader,
    record_batch::RecordBatch,
};
use polars_core::prelude::*;
use polars_core::utils::accumulate_dataframes_vertical;
use std::convert::TryFrom;
use std::io::{Read, Seek, Write};
use std::sync::Arc;

pub trait PhysicalIoExpr: Send + Sync {
    fn evaluate(&self, df: &DataFrame) -> Result<Series>;
}

pub trait SerReader<R>
where
    R: Read + Seek,
{
    fn new(reader: R) -> Self;

    /// Rechunk to a single chunk after Reading file.
    fn set_rechunk(self, _rechunk: bool) -> Self
    where
        Self: std::marker::Sized,
    {
        self
    }

    /// Take the SerReader and return a parsed DataFrame.
    fn finish(self) -> Result<DataFrame>;
}

pub trait SerWriter<'a, W>
where
    W: Write,
{
    fn new(writer: &'a mut W) -> Self;
    fn finish(self, df: &mut DataFrame) -> Result<()>;
}

pub trait ArrowReader {
    fn next_record_batch(&mut self) -> ArrowResult<Option<RecordBatch>>;

    fn schema(&self) -> Arc<Schema>;
}

impl<R: Read> ArrowReader for ArrowCsvReader<R> {
    fn next_record_batch(&mut self) -> ArrowResult<Option<RecordBatch>> {
        self.next().map_or(Ok(None), |v| v.map(Some))
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::new((&*self.schema()).into())
    }
}

impl<R: Read> ArrowReader for ArrowJsonReader<R> {
    fn next_record_batch(&mut self) -> ArrowResult<Option<RecordBatch>> {
        self.next()
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::new((&*self.schema()).into())
    }
}

pub(crate) fn finish_reader<R: ArrowReader>(
    mut reader: R,
    rechunk: bool,
    stop_after_n_rows: Option<usize>,
    predicate: Option<Arc<dyn PhysicalIoExpr>>,
    aggregate: Option<&[ScanAggregation]>,
) -> Result<DataFrame> {
    let mut n_rows = 0;
    let mut parsed_dfs = Vec::with_capacity(1024);

    while let Some(batch) = reader.next_record_batch()? {
        n_rows += batch.num_rows();

        let mut df = DataFrame::try_from(batch)?;

        if let Some(predicate) = &predicate {
            let s = predicate.evaluate(&df)?;
            let mask = s.bool().expect("filter predicates was not of type boolean");
            df = df.filter(mask)?;
        }

        if let Some(aggregate) = aggregate {
            let cols = aggregate
                .iter()
                .map(|scan_agg| scan_agg.evaluate_batch(&df).unwrap())
                .collect();
            if cfg!(debug_assertions) {
                df = DataFrame::new(cols).unwrap();
            } else {
                df = DataFrame::new_no_checks(cols)
            }
        }

        parsed_dfs.push(df);
        if let Some(n) = stop_after_n_rows {
            if n_rows >= n {
                break;
            }
        }
    }
    let mut df = accumulate_dataframes_vertical(parsed_dfs)?;

    if let Some(aggregate) = aggregate {
        let cols = aggregate
            .iter()
            .map(|scan_agg| scan_agg.finish(&df).unwrap())
            .collect();
        df = DataFrame::new_no_checks(cols)
    }

    match rechunk {
        true => Ok(df.agg_chunks()),
        false => Ok(df),
    }
}

pub enum ScanAggregation {
    Sum {
        column: String,
        alias: Option<String>,
    },
    Min {
        column: String,
        alias: Option<String>,
    },
    Max {
        column: String,
        alias: Option<String>,
    },
    First {
        column: String,
        alias: Option<String>,
    },
    Last {
        column: String,
        alias: Option<String>,
    },
}

impl ScanAggregation {
    /// Evaluate the aggregations per batch.
    pub(crate) fn evaluate_batch(&self, df: &DataFrame) -> Result<Series> {
        use ScanAggregation::*;
        let s = match self {
            Sum { column, .. } => df.column(column)?.sum_as_series(),
            Min { column, .. } => df.column(column)?.min_as_series(),
            Max { column, .. } => df.column(column)?.max_as_series(),
            First { column, .. } => df.column(column)?.head(Some(1)),
            Last { column, .. } => df.column(column)?.tail(Some(1)),
        };
        Ok(s)
    }

    /// After all batches are concatenated the aggregation is determined for the whole set.
    pub(crate) fn finish(&self, df: &DataFrame) -> Result<Series> {
        use ScanAggregation::*;
        match self {
            Sum { column, alias } => {
                let mut s = df.column(column)?.sum_as_series();
                if let Some(alias) = alias {
                    s.rename(alias);
                }
                Ok(s)
            }
            Min { column, alias } => {
                let mut s = df.column(column)?.min_as_series();
                if let Some(alias) = alias {
                    s.rename(alias);
                }
                Ok(s)
            }
            Max { column, alias } => {
                let mut s = df.column(column)?.max_as_series();
                if let Some(alias) = alias {
                    s.rename(alias);
                }
                Ok(s)
            }
            First { column, alias } => {
                let mut s = df.column(column)?.head(Some(1));
                if let Some(alias) = alias {
                    s.rename(alias);
                }
                Ok(s)
            }
            Last { column, alias } => {
                let mut s = df.column(column)?.tail(Some(1));
                if let Some(alias) = alias {
                    s.rename(alias);
                }
                Ok(s)
            }
        }
    }
}
