// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Execution plan for reading Parquet files

use std::fmt;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::{any::Any, convert::TryInto};

use crate::{
    error::{DataFusionError, Result},
    logical_plan::{Column, Expr},
    physical_optimizer::pruning::{PruningPredicate, PruningStatistics},
    physical_plan::{
        DisplayFormatType, ExecutionPlan, Partitioning, RecordBatchStream,
        SendableRecordBatchStream,
    },
    scalar::ScalarValue,
};

use arrow::{
    array::ArrayRef,
    datatypes::{Schema, SchemaRef},
    error::{ArrowError, Result as ArrowResult},
    record_batch::RecordBatch,
};
use hashbrown::HashMap;
use log::debug;
use parquet::file::{
    metadata::RowGroupMetaData,
    reader::{FileReader, SerializedFileReader},
    statistics::Statistics as ParquetStatistics,
};

use fmt::Debug;
use parquet::arrow::{ArrowReader, ParquetFileArrowReader};

use tokio::{
    sync::mpsc::{channel, Receiver, Sender},
    task,
};
use tokio_stream::wrappers::ReceiverStream;

use crate::datasource::datasource::{ColumnStatistics, Statistics};
use async_trait::async_trait;
use futures::stream::{Stream, StreamExt};

use super::SQLMetric;
use crate::datasource::object_store::ObjectStore;
use crate::datasource::parquet_desc::{ObjectReaderWrapper, ParquetRootDesc};
use crate::datasource::{get_statistics_with_limit, FilePartition, PartitionedFile};

/// Execution plan for scanning one or more Parquet partitions
#[derive(Debug, Clone)]
pub struct ParquetExec {
    /// Parquet partitions to read
    partitions: Vec<ParquetPartition>,
    /// Source used for get reader for partitions
    object_store: Arc<dyn ObjectStore>,
    /// Schema after projection is applied
    schema: SchemaRef,
    /// Projection for which columns to load
    projection: Vec<usize>,
    /// Batch size
    batch_size: usize,
    /// Statistics for the data set (sum of statistics for all partitions)
    statistics: Statistics,
    /// metrics for the overall execution
    metrics: ParquetExecMetrics,
    /// Optional predicate builder
    predicate_builder: Option<PruningPredicate>,
    /// Optional limit of the number of rows
    limit: Option<usize>,
}

/// Represents one partition of a Parquet data set and this currently means one Parquet file.
///
/// In the future it would be good to support subsets of files based on ranges of row groups
/// so that we can better parallelize reads of large files across available cores (see
/// [ARROW-10995](https://issues.apache.org/jira/browse/ARROW-10995)).
///
/// We may also want to support reading Parquet files that are partitioned based on a key and
/// in this case we would want this partition struct to represent multiple files for a given
/// partition key (see [ARROW-11019](https://issues.apache.org/jira/browse/ARROW-11019)).
#[derive(Debug, Clone)]
pub struct ParquetPartition {
    /// The Parquet filename for this partition
    pub file_partition: FilePartition,
    /// Execution metrics
    metrics: ParquetPartitionMetrics,
}

/// Stores metrics about the overall parquet execution
#[derive(Debug, Clone)]
pub struct ParquetExecMetrics {
    /// Numer of times the pruning predicate could not be created
    pub predicate_creation_errors: Arc<SQLMetric>,
}

/// Stores metrics about the parquet execution for a particular ParquetPartition
#[derive(Debug, Clone)]
struct ParquetPartitionMetrics {
    /// Numer of times the predicate could not be evaluated
    pub predicate_evaluation_errors: Arc<SQLMetric>,
    /// Number of row groups pruned using
    pub row_groups_pruned: Arc<SQLMetric>,
}

impl ParquetExec {
    /// Create a new Parquet reader execution plan based on the specified Parquet filename or
    /// directory containing Parquet files
    pub fn try_from_path(
        path: &str,
        projection: Option<Vec<usize>>,
        predicate: Option<Expr>,
        batch_size: usize,
        max_concurrency: usize,
        limit: Option<usize>,
    ) -> Result<Self> {
        // build a list of filenames from the specified path, which could be a single file or
        // a directory containing one or more parquet files
        let root_desc = ParquetRootDesc::new(path);
        Self::try_new(
            Arc::new(root_desc?),
            projection,
            predicate,
            batch_size,
            max_concurrency,
            limit,
        )
    }

    pub fn try_new(
        desc: Arc<ParquetRootDesc>,
        projection: Option<Vec<usize>>,
        predicate: Option<Expr>,
        batch_size: usize,
        max_concurrency: usize,
        limit: Option<usize>,
    ) -> Result<Self> {
        debug!("Creating ParquetExec, desc: {:?}, projection {:?}, predicate: {:?}, limit: {:?}",
               desc, projection, predicate, limit);

        let (all_files, statistics) = get_statistics_with_limit(&desc.descriptor, limit);
        let schema = desc.schema();

        let mut partitions = Vec::with_capacity(max_concurrency);
        let mut chunk_size = all_files.len() / max_concurrency;
        if all_files.len() % max_concurrency > 0 {
            chunk_size += 1;
        }
        let chunked_files = all_files.chunks(chunk_size);
        for (index, group) in chunked_files.enumerate() {
            partitions.push(ParquetPartition::new(Vec::from(group), index));
        }

        let metrics = ParquetExecMetrics::new();

        let predicate_builder = predicate.and_then(|predicate_expr| {
            match PruningPredicate::try_new(&predicate_expr, schema.clone()) {
                Ok(predicate_builder) => Some(predicate_builder),
                Err(e) => {
                    debug!(
                        "Could not create pruning predicate for {:?}: {}",
                        predicate_expr, e
                    );
                    metrics.predicate_creation_errors.add(1);
                    None
                }
            }
        });

        Ok(Self::new(
            partitions,
            desc.object_store.clone(),
            schema,
            projection,
            statistics,
            metrics,
            predicate_builder,
            batch_size,
            limit,
        ))
    }

    /// Create a new Parquet reader execution plan with provided partitions and schema
    pub fn new(
        partitions: Vec<ParquetPartition>,
        object_store: Arc<dyn ObjectStore>,
        schema: SchemaRef,
        projection: Option<Vec<usize>>,
        statistics: Statistics,
        metrics: ParquetExecMetrics,
        predicate_builder: Option<PruningPredicate>,
        batch_size: usize,
        limit: Option<usize>,
    ) -> Self {
        let projection = match projection {
            Some(p) => p,
            None => (0..schema.fields().len()).collect(),
        };

        let projected_schema = Schema::new(
            projection
                .iter()
                .map(|i| schema.field(*i).clone())
                .collect(),
        );

        let mut new_column_statistics: Option<Vec<ColumnStatistics>> = None;

        if let Some(column_stats) = statistics.column_statistics {
            new_column_statistics = Some(Vec::with_capacity(projection.len()));
            for proj in projection {
                new_column_statistics.map(|mut x| x.push(column_stats[proj].clone()));
            }
        };

        let statistics = Statistics {
            num_rows: statistics.num_rows,
            total_byte_size: statistics.total_byte_size,
            column_statistics: new_column_statistics,
        };

        Self {
            partitions,
            object_store: object_store,
            schema: Arc::new(projected_schema),
            projection,
            metrics,
            predicate_builder,
            batch_size,
            statistics,
            limit,
        }
    }

    /// Parquet partitions to read
    pub fn partitions(&self) -> &[ParquetPartition] {
        &self.partitions
    }

    /// Projection for which columns to load
    pub fn projection(&self) -> &[usize] {
        &self.projection
    }

    /// Batch size
    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    /// Statistics for the data set (sum of statistics for all partitions)
    pub fn statistics(&self) -> &Statistics {
        &self.statistics
    }
}

impl ParquetPartition {
    /// Create a new parquet partition
    pub fn new(files: Vec<PartitionedFile>, index: usize) -> Self {
        Self {
            file_partition: FilePartition { index, files },
            metrics: ParquetPartitionMetrics::new(),
        }
    }

    /// The Parquet filename for this partition
    pub fn filenames(&self) -> &[String] {
        todo!()
    }

    /// Statistics for this partition
    pub fn statistics(&self) -> &Statistics {
        todo!()
    }
}

impl ParquetExecMetrics {
    /// Create new metrics
    pub fn new() -> Self {
        Self {
            predicate_creation_errors: SQLMetric::counter(),
        }
    }
}

impl ParquetPartitionMetrics {
    /// Create new metrics
    pub fn new() -> Self {
        Self {
            predicate_evaluation_errors: SQLMetric::counter(),
            row_groups_pruned: SQLMetric::counter(),
        }
    }
}

#[async_trait]
impl ExecutionPlan for ParquetExec {
    /// Return a reference to Any that can be used for downcasting
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        // this is a leaf node and has no children
        vec![]
    }

    /// Get the output partitioning of this plan
    fn output_partitioning(&self) -> Partitioning {
        Partitioning::UnknownPartitioning(self.partitions.len())
    }

    fn with_new_children(
        &self,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if children.is_empty() {
            Ok(Arc::new(self.clone()))
        } else {
            Err(DataFusionError::Internal(format!(
                "Children cannot be replaced in {:?}",
                self
            )))
        }
    }

    async fn execute(&self, partition: usize) -> Result<SendableRecordBatchStream> {
        // because the parquet implementation is not thread-safe, it is necessary to execute
        // on a thread and communicate with channels
        let (response_tx, response_rx): (
            Sender<ArrowResult<RecordBatch>>,
            Receiver<ArrowResult<RecordBatch>>,
        ) = channel(2);

        let partition = self.partitions[partition].clone();
        let metrics = partition.metrics.clone();
        let projection = self.projection.clone();
        let predicate_builder = self.predicate_builder.clone();
        let batch_size = self.batch_size;
        let limit = self.limit;

        task::spawn_blocking(move || {
            if let Err(e) = read_files(
                self.object_store.clone(),
                partition,
                metrics,
                &projection,
                &predicate_builder,
                batch_size,
                response_tx,
                limit,
            ) {
                println!("Parquet reader thread terminated due to error: {:?}", e);
            }
        });

        Ok(Box::pin(ParquetStream {
            schema: self.schema.clone(),
            inner: ReceiverStream::new(response_rx),
        }))
    }

    fn fmt_as(
        &self,
        t: DisplayFormatType,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default => {
                let files: Vec<_> = self
                    .partitions
                    .iter()
                    .map(|pp| format!("{}", pp.file_partition))
                    .collect();

                write!(
                    f,
                    "ParquetExec: batch_size={}, limit={:?}, partitions=[{}]",
                    self.batch_size,
                    self.limit,
                    files.join(", ")
                )
            }
        }
    }

    fn metrics(&self) -> HashMap<String, SQLMetric> {
        self.partitions
            .iter()
            .flat_map(|p| {
                vec![
                    (
                        format!(
                            "numPredicateEvaluationErrors for {}",
                            p.filenames.join(",")
                        ),
                        p.metrics.predicate_evaluation_errors.as_ref().clone(),
                    ),
                    (
                        format!("numRowGroupsPruned for {}", p.filenames.join(",")),
                        p.metrics.row_groups_pruned.as_ref().clone(),
                    ),
                ]
            })
            .chain(std::iter::once((
                "numPredicateCreationErrors".to_string(),
                self.metrics.predicate_creation_errors.as_ref().clone(),
            )))
            .collect()
    }
}

fn send_result(
    response_tx: &Sender<ArrowResult<RecordBatch>>,
    result: ArrowResult<RecordBatch>,
) -> Result<()> {
    // Note this function is running on its own blockng tokio thread so blocking here is ok.
    response_tx
        .blocking_send(result)
        .map_err(|e| DataFusionError::Execution(e.to_string()))?;
    Ok(())
}

/// Wraps parquet statistics in a way
/// that implements [`PruningStatistics`]
struct RowGroupPruningStatistics<'a> {
    row_group_metadata: &'a [RowGroupMetaData],
    parquet_schema: &'a Schema,
}

/// Extract the min/max statistics from a `ParquetStatistics` object
macro_rules! get_statistic {
    ($column_statistics:expr, $func:ident, $bytes_func:ident) => {{
        if !$column_statistics.has_min_max_set() {
            return None;
        }
        match $column_statistics {
            ParquetStatistics::Boolean(s) => Some(ScalarValue::Boolean(Some(*s.$func()))),
            ParquetStatistics::Int32(s) => Some(ScalarValue::Int32(Some(*s.$func()))),
            ParquetStatistics::Int64(s) => Some(ScalarValue::Int64(Some(*s.$func()))),
            // 96 bit ints not supported
            ParquetStatistics::Int96(_) => None,
            ParquetStatistics::Float(s) => Some(ScalarValue::Float32(Some(*s.$func()))),
            ParquetStatistics::Double(s) => Some(ScalarValue::Float64(Some(*s.$func()))),
            ParquetStatistics::ByteArray(s) => {
                let s = std::str::from_utf8(s.$bytes_func())
                    .map(|s| s.to_string())
                    .ok();
                Some(ScalarValue::Utf8(s))
            }
            // type not supported yet
            ParquetStatistics::FixedLenByteArray(_) => None,
        }
    }};
}

// Extract the min or max value calling `func` or `bytes_func` on the ParquetStatistics as appropriate
macro_rules! get_min_max_values {
    ($self:expr, $column:expr, $func:ident, $bytes_func:ident) => {{
        let (column_index, field) = if let Some((v, f)) = $self.parquet_schema.column_with_name(&$column.name) {
            (v, f)
        } else {
            // Named column was not present
            return None
        };

        let data_type = field.data_type();
        let null_scalar: ScalarValue = if let Ok(v) = data_type.try_into() {
            v
        } else {
            // DataFusion doesn't have support for ScalarValues of the column type
            return None
        };

        let scalar_values : Vec<ScalarValue> = $self.row_group_metadata
            .iter()
            .flat_map(|meta| {
                meta.column(column_index).statistics()
            })
            .map(|stats| {
                get_statistic!(stats, $func, $bytes_func)
            })
            .map(|maybe_scalar| {
                // column either did't have statistics at all or didn't have min/max values
                maybe_scalar.unwrap_or_else(|| null_scalar.clone())
            })
            .collect();

        // ignore errors converting to arrays (e.g. different types)
        ScalarValue::iter_to_array(scalar_values).ok()
    }}
}

impl<'a> PruningStatistics for RowGroupPruningStatistics<'a> {
    fn min_values(&self, column: &Column) -> Option<ArrayRef> {
        get_min_max_values!(self, column, min, min_bytes)
    }

    fn max_values(&self, column: &Column) -> Option<ArrayRef> {
        get_min_max_values!(self, column, max, max_bytes)
    }

    fn num_containers(&self) -> usize {
        self.row_group_metadata.len()
    }
}

fn build_row_group_predicate(
    predicate_builder: &PruningPredicate,
    metrics: ParquetPartitionMetrics,
    row_group_metadata: &[RowGroupMetaData],
) -> Box<dyn Fn(&RowGroupMetaData, usize) -> bool> {
    let parquet_schema = predicate_builder.schema().as_ref();

    let pruning_stats = RowGroupPruningStatistics {
        row_group_metadata,
        parquet_schema,
    };
    let predicate_values = predicate_builder.prune(&pruning_stats);

    match predicate_values {
        Ok(values) => {
            // NB: false means don't scan row group
            let num_pruned = values.iter().filter(|&v| !*v).count();
            metrics.row_groups_pruned.add(num_pruned);
            Box::new(move |_, i| values[i])
        }
        // stats filter array could not be built
        // return a closure which will not filter out any row groups
        Err(e) => {
            debug!("Error evaluating row group predicate values {}", e);
            metrics.predicate_evaluation_errors.add(1);
            Box::new(|_r, _i| true)
        }
    }
}

fn read_files(
    object_store: Arc<dyn ObjectStore>,
    partition: ParquetPartition,
    metrics: ParquetPartitionMetrics,
    projection: &[usize],
    predicate_builder: &Option<PruningPredicate>,
    batch_size: usize,
    response_tx: Sender<ArrowResult<RecordBatch>>,
    limit: Option<usize>,
) -> Result<()> {
    let mut total_rows = 0;
    let all_files = partition.file_partition.files;
    'outer: for partitioned_file in all_files {
        let reader = object_store.get_reader(partitioned_file.file_path.as_str())?;
        let mut file_reader =
            SerializedFileReader::new(ObjectReaderWrapper::new(reader))?;
        if let Some(predicate_builder) = predicate_builder {
            let row_group_predicate = build_row_group_predicate(
                predicate_builder,
                metrics.clone(),
                file_reader.metadata().row_groups(),
            );
            file_reader.filter_row_groups(&row_group_predicate);
        }
        let mut arrow_reader = ParquetFileArrowReader::new(Arc::new(file_reader));
        let mut batch_reader = arrow_reader
            .get_record_reader_by_columns(projection.to_owned(), batch_size)?;
        loop {
            match batch_reader.next() {
                Some(Ok(batch)) => {
                    total_rows += batch.num_rows();
                    send_result(&response_tx, Ok(batch))?;
                    if limit.map(|l| total_rows >= l).unwrap_or(false) {
                        break 'outer;
                    }
                }
                None => {
                    break;
                }
                Some(Err(e)) => {
                    let err_msg = format!(
                        "Error reading batch from {}: {}",
                        partitioned_file,
                        e.to_string()
                    );
                    // send error to operator
                    send_result(
                        &response_tx,
                        Err(ArrowError::ParquetError(err_msg.clone())),
                    )?;
                    // terminate thread with error
                    return Err(DataFusionError::Execution(err_msg));
                }
            }
        }
    }

    // finished reading files (dropping response_tx will close
    // channel)
    Ok(())
}

fn split_files(filenames: &[String], n: usize) -> Vec<&[String]> {
    let mut chunk_size = filenames.len() / n;
    if filenames.len() % n > 0 {
        chunk_size += 1;
    }
    filenames.chunks(chunk_size).collect()
}

struct ParquetStream {
    schema: SchemaRef,
    inner: ReceiverStream<ArrowResult<RecordBatch>>,
}

impl Stream for ParquetStream {
    type Item = ArrowResult<RecordBatch>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

impl RecordBatchStream for ParquetStream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::{DataType, Field};
    use futures::StreamExt;
    use parquet::{
        basic::Type as PhysicalType,
        file::{metadata::RowGroupMetaData, statistics::Statistics as ParquetStatistics},
        schema::types::SchemaDescPtr,
    };

    #[test]
    fn test_split_files() {
        let filenames = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
            "e".to_string(),
        ];

        let chunks = split_files(&filenames, 1);
        assert_eq!(1, chunks.len());
        assert_eq!(5, chunks[0].len());

        let chunks = split_files(&filenames, 2);
        assert_eq!(2, chunks.len());
        assert_eq!(3, chunks[0].len());
        assert_eq!(2, chunks[1].len());

        let chunks = split_files(&filenames, 5);
        assert_eq!(5, chunks.len());
        assert_eq!(1, chunks[0].len());
        assert_eq!(1, chunks[1].len());
        assert_eq!(1, chunks[2].len());
        assert_eq!(1, chunks[3].len());
        assert_eq!(1, chunks[4].len());

        let chunks = split_files(&filenames, 123);
        assert_eq!(5, chunks.len());
        assert_eq!(1, chunks[0].len());
        assert_eq!(1, chunks[1].len());
        assert_eq!(1, chunks[2].len());
        assert_eq!(1, chunks[3].len());
        assert_eq!(1, chunks[4].len());
    }

    #[tokio::test]
    async fn test() -> Result<()> {
        let testdata = crate::test_util::parquet_test_data();
        let filename = format!("{}/alltypes_plain.parquet", testdata);
        let parquet_exec = ParquetExec::try_from_path(
            &filename,
            Some(vec![0, 1, 2]),
            None,
            1024,
            4,
            None,
        )?;
        assert_eq!(parquet_exec.output_partitioning().partition_count(), 1);

        let mut results = parquet_exec.execute(0).await?;
        let batch = results.next().await.unwrap()?;

        assert_eq!(8, batch.num_rows());
        assert_eq!(3, batch.num_columns());

        let schema = batch.schema();
        let field_names: Vec<&str> =
            schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(vec!["id", "bool_col", "tinyint_col"], field_names);

        let batch = results.next().await;
        assert!(batch.is_none());

        let batch = results.next().await;
        assert!(batch.is_none());

        let batch = results.next().await;
        assert!(batch.is_none());

        Ok(())
    }

    #[test]
    fn row_group_predicate_builder_simple_expr() -> Result<()> {
        use crate::logical_plan::{col, lit};
        // int > 1 => c1_max > 1
        let expr = col("c1").gt(lit(15));
        let schema = Schema::new(vec![Field::new("c1", DataType::Int32, false)]);
        let predicate_builder = PruningPredicate::try_new(&expr, Arc::new(schema))?;

        let schema_descr = get_test_schema_descr(vec![("c1", PhysicalType::INT32)]);
        let rgm1 = get_row_group_meta_data(
            &schema_descr,
            vec![ParquetStatistics::int32(Some(1), Some(10), None, 0, false)],
        );
        let rgm2 = get_row_group_meta_data(
            &schema_descr,
            vec![ParquetStatistics::int32(Some(11), Some(20), None, 0, false)],
        );
        let row_group_metadata = vec![rgm1, rgm2];
        let row_group_predicate = build_row_group_predicate(
            &predicate_builder,
            ParquetPartitionMetrics::new(),
            &row_group_metadata,
        );
        let row_group_filter = row_group_metadata
            .iter()
            .enumerate()
            .map(|(i, g)| row_group_predicate(g, i))
            .collect::<Vec<_>>();
        assert_eq!(row_group_filter, vec![false, true]);

        Ok(())
    }

    #[test]
    fn row_group_predicate_builder_missing_stats() -> Result<()> {
        use crate::logical_plan::{col, lit};
        // int > 1 => c1_max > 1
        let expr = col("c1").gt(lit(15));
        let schema = Schema::new(vec![Field::new("c1", DataType::Int32, false)]);
        let predicate_builder = PruningPredicate::try_new(&expr, Arc::new(schema))?;

        let schema_descr = get_test_schema_descr(vec![("c1", PhysicalType::INT32)]);
        let rgm1 = get_row_group_meta_data(
            &schema_descr,
            vec![ParquetStatistics::int32(None, None, None, 0, false)],
        );
        let rgm2 = get_row_group_meta_data(
            &schema_descr,
            vec![ParquetStatistics::int32(Some(11), Some(20), None, 0, false)],
        );
        let row_group_metadata = vec![rgm1, rgm2];
        let row_group_predicate = build_row_group_predicate(
            &predicate_builder,
            ParquetPartitionMetrics::new(),
            &row_group_metadata,
        );
        let row_group_filter = row_group_metadata
            .iter()
            .enumerate()
            .map(|(i, g)| row_group_predicate(g, i))
            .collect::<Vec<_>>();
        // missing statistics for first row group mean that the result from the predicate expression
        // is null / undefined so the first row group can't be filtered out
        assert_eq!(row_group_filter, vec![true, true]);

        Ok(())
    }

    #[test]
    fn row_group_predicate_builder_partial_expr() -> Result<()> {
        use crate::logical_plan::{col, lit};
        // test row group predicate with partially supported expression
        // int > 1 and int % 2 => c1_max > 1 and true
        let expr = col("c1").gt(lit(15)).and(col("c2").modulus(lit(2)));
        let schema = Arc::new(Schema::new(vec![
            Field::new("c1", DataType::Int32, false),
            Field::new("c2", DataType::Int32, false),
        ]));
        let predicate_builder = PruningPredicate::try_new(&expr, schema.clone())?;

        let schema_descr = get_test_schema_descr(vec![
            ("c1", PhysicalType::INT32),
            ("c2", PhysicalType::INT32),
        ]);
        let rgm1 = get_row_group_meta_data(
            &schema_descr,
            vec![
                ParquetStatistics::int32(Some(1), Some(10), None, 0, false),
                ParquetStatistics::int32(Some(1), Some(10), None, 0, false),
            ],
        );
        let rgm2 = get_row_group_meta_data(
            &schema_descr,
            vec![
                ParquetStatistics::int32(Some(11), Some(20), None, 0, false),
                ParquetStatistics::int32(Some(11), Some(20), None, 0, false),
            ],
        );
        let row_group_metadata = vec![rgm1, rgm2];
        let row_group_predicate = build_row_group_predicate(
            &predicate_builder,
            ParquetPartitionMetrics::new(),
            &row_group_metadata,
        );
        let row_group_filter = row_group_metadata
            .iter()
            .enumerate()
            .map(|(i, g)| row_group_predicate(g, i))
            .collect::<Vec<_>>();
        // the first row group is still filtered out because the predicate expression can be partially evaluated
        // when conditions are joined using AND
        assert_eq!(row_group_filter, vec![false, true]);

        // if conditions in predicate are joined with OR and an unsupported expression is used
        // this bypasses the entire predicate expression and no row groups are filtered out
        let expr = col("c1").gt(lit(15)).or(col("c2").modulus(lit(2)));
        let predicate_builder = PruningPredicate::try_new(&expr, schema)?;
        let row_group_predicate = build_row_group_predicate(
            &predicate_builder,
            ParquetPartitionMetrics::new(),
            &row_group_metadata,
        );
        let row_group_filter = row_group_metadata
            .iter()
            .enumerate()
            .map(|(i, g)| row_group_predicate(g, i))
            .collect::<Vec<_>>();
        assert_eq!(row_group_filter, vec![true, true]);

        Ok(())
    }

    #[test]
    fn row_group_predicate_builder_unsupported_type() -> Result<()> {
        use crate::logical_plan::{col, lit};
        // test row group predicate with unsupported statistics type (boolean)
        // where a null array is generated for some statistics columns
        // int > 1 and bool = true => c1_max > 1 and null
        let expr = col("c1").gt(lit(15)).and(col("c2").eq(lit(true)));
        let schema = Arc::new(Schema::new(vec![
            Field::new("c1", DataType::Int32, false),
            Field::new("c2", DataType::Boolean, false),
        ]));
        let predicate_builder = PruningPredicate::try_new(&expr, schema)?;

        let schema_descr = get_test_schema_descr(vec![
            ("c1", PhysicalType::INT32),
            ("c2", PhysicalType::BOOLEAN),
        ]);
        let rgm1 = get_row_group_meta_data(
            &schema_descr,
            vec![
                ParquetStatistics::int32(Some(1), Some(10), None, 0, false),
                ParquetStatistics::boolean(Some(false), Some(true), None, 0, false),
            ],
        );
        let rgm2 = get_row_group_meta_data(
            &schema_descr,
            vec![
                ParquetStatistics::int32(Some(11), Some(20), None, 0, false),
                ParquetStatistics::boolean(Some(false), Some(true), None, 0, false),
            ],
        );
        let row_group_metadata = vec![rgm1, rgm2];
        let row_group_predicate = build_row_group_predicate(
            &predicate_builder,
            ParquetPartitionMetrics::new(),
            &row_group_metadata,
        );
        let row_group_filter = row_group_metadata
            .iter()
            .enumerate()
            .map(|(i, g)| row_group_predicate(g, i))
            .collect::<Vec<_>>();
        // no row group is filtered out because the predicate expression can't be evaluated
        // when a null array is generated for a statistics column,
        // because the null values propagate to the end result, making the predicate result undefined
        assert_eq!(row_group_filter, vec![true, true]);

        Ok(())
    }

    fn get_row_group_meta_data(
        schema_descr: &SchemaDescPtr,
        column_statistics: Vec<ParquetStatistics>,
    ) -> RowGroupMetaData {
        use parquet::file::metadata::ColumnChunkMetaData;
        let mut columns = vec![];
        for (i, s) in column_statistics.iter().enumerate() {
            let column = ColumnChunkMetaData::builder(schema_descr.column(i))
                .set_statistics(s.clone())
                .build()
                .unwrap();
            columns.push(column);
        }
        RowGroupMetaData::builder(schema_descr.clone())
            .set_num_rows(1000)
            .set_total_byte_size(2000)
            .set_column_metadata(columns)
            .build()
            .unwrap()
    }

    fn get_test_schema_descr(fields: Vec<(&str, PhysicalType)>) -> SchemaDescPtr {
        use parquet::schema::types::{SchemaDescriptor, Type as SchemaType};
        let mut schema_fields = fields
            .iter()
            .map(|(n, t)| {
                Arc::new(SchemaType::primitive_type_builder(n, *t).build().unwrap())
            })
            .collect::<Vec<_>>();
        let schema = SchemaType::group_type_builder("schema")
            .with_fields(&mut schema_fields)
            .build()
            .unwrap();

        Arc::new(SchemaDescriptor::new(Arc::new(schema)))
    }
}
