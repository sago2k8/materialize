// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// BEGIN LINT CONFIG
// DO NOT EDIT. Automatically generated by bin/gen-lints.
// Have complaints about the noise? See the note in misc/python/materialize/cli/gen-lints.py first.
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(clippy::double_neg)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::overflow_check_conditional)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::disallowed_types)]
#![warn(clippy::from_over_into)]
// END LINT CONFIG

//! An async wrapper around RocksDB, that does IO on a separate thread.
//!
//! This crate offers a limited API to communicate with RocksDB, to get
//! the best performance possible (most importantly, by batching operations).
//! Currently this API is only `upsert`, which replaces (or deletes) values for
//! a set of keys, and returns the previous values.

#![warn(missing_docs)]

use std::convert::AsRef;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::Instant;

use itertools::Itertools;
use rocksdb::{
    DBCompressionType, Env, Error as RocksDBError, Options as RocksDBOptions, WriteOptions, DB,
};
use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::{mpsc, oneshot};

use mz_ore::cast::{CastFrom, CastLossy};
use mz_ore::metrics::DeleteOnDropHistogram;

/// An error using this RocksDB wrapper.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An error from the underlying Kafka library.
    #[error(transparent)]
    RocksDB(#[from] RocksDBError),

    /// Error when using the instance after RocksDB as errored
    /// or been shutdown.
    #[error("RocksDB thread has been shut down or errored")]
    RocksDBThreadGoneAway,

    /// Error decoding a value previously written.
    #[error("failed to decode value")]
    DecodeError(#[from] bincode::Error),

    /// A tokio thread used by the implementation panicked.
    #[error("tokio thread panicked")]
    TokioPanic(#[from] tokio::task::JoinError),
}

/// Options to configure a [`RocksDBInstance`].
pub struct Options {
    /// Whether or not to clear state at the instance
    /// path before starting.
    pub cleanup_on_new: bool,

    /// Whether or not to write writes
    /// to the wal.
    pub use_wal: bool,

    /// Compression type for blocks and blobs.
    pub compression_type: DBCompressionType,

    /// A possibly shared RocksDB `Env`.
    pub env: Env,
}

/// Metrics about an instances usage of RocksDB. User-provided
/// so the user can choose the labels.
pub struct RocksDBMetrics {
    /// Latency of multi_gets, in fractional seconds.
    pub multi_get_latency: DeleteOnDropHistogram<'static, Vec<String>>,
    /// Size of multi_get batches.
    pub multi_get_size: DeleteOnDropHistogram<'static, Vec<String>>,
    /// Latency of write batch writes, in fractional seconds.
    pub multi_put_latency: DeleteOnDropHistogram<'static, Vec<String>>,
    /// Size of write batches.
    pub multi_put_size: DeleteOnDropHistogram<'static, Vec<String>>,
}

impl Options {
    /// A new `Options` object with reasonable defaults.
    pub fn new_with_defaults() -> Result<Self, RocksDBError> {
        Ok(Options {
            cleanup_on_new: true,
            use_wal: false,
            compression_type: DBCompressionType::Snappy,
            env: rocksdb::Env::new()?,
        })
    }

    fn as_rocksdb_options(&self) -> RocksDBOptions {
        let mut options = rocksdb::Options::default();
        options.create_if_missing(true);

        /*
        // Dumped every 600 seconds.
        rocks_options.enable_statistics();
        rocks_options.set_report_bg_io_stats(true);
        */

        options.set_compression_type(self.compression_type);
        options.set_blob_compression_type(self.compression_type);

        options.set_env(&self.env);
        options
    }

    fn as_rocksdb_write_options(&self) -> WriteOptions {
        let mut wo = rocksdb::WriteOptions::new();
        wo.disable_wal(!self.use_wal);
        wo
    }
}

#[derive(Debug)]
enum Command<K, V> {
    MultiGet {
        batch: Vec<K>,
        // Scratch vector to return results in.
        results_scratch: Vec<Option<V>>,
        response_sender: oneshot::Sender<
            Result<
                (
                    // The batch scratch vector being given back.
                    Vec<K>,
                    Vec<Option<V>>,
                ),
                Error,
            >,
        >,
    },
    MultiPut {
        batch: Vec<(K, Option<V>)>,
        // Scratch vector to return results in.
        response_sender: oneshot::Sender<Result<Vec<(K, Option<V>)>, Error>>,
    },
    Shutdown {
        done_sender: oneshot::Sender<()>,
    },
}

/// An async wrapper around RocksDB.
#[derive(Clone)]
pub struct RocksDBInstance<K, V> {
    tx: mpsc::Sender<Command<K, V>>,

    // Scratch vector to send keys to the RocksDB thread
    // during `MultiGet`.
    multi_get_scratch: Vec<K>,

    // Scratch vector to return results from the RocksDB thread
    // during `MultiGet`.
    multi_get_results_scratch: Vec<Option<V>>,

    // Scratch vector to send updates to the RocksDB thread
    // during `MultiPut`.
    multi_put_scratch: Vec<(K, Option<V>)>,
}

impl<K, V> RocksDBInstance<K, V>
where
    K: AsRef<[u8]> + Send + Sync + 'static,
    V: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// Start a new RocksDB instance at the path.
    pub async fn new<M: Deref<Target = RocksDBMetrics> + Send + 'static>(
        instance_path: &Path,
        options: Options,
        metrics: M,
    ) -> Result<Self, Error> {
        if options.cleanup_on_new && instance_path.exists() {
            let instance_path_owned = instance_path.to_owned();
            mz_ore::task::spawn_blocking(
                || format!("RocksDB instance at {}: cleanup", instance_path.display()),
                move || {
                    DB::destroy(&RocksDBOptions::default(), instance_path_owned).unwrap();
                },
            )
            .await?;
        }

        // The buffer can be small here, as all interactions with it take `&mut self`.
        let (tx, rx): (mpsc::Sender<Command<K, V>>, _) = mpsc::channel(10);

        let instance_path = instance_path.to_owned();

        std::thread::spawn(move || rocksdb_core_loop(options, instance_path, rx, metrics));

        Ok(Self {
            tx,
            multi_get_scratch: Vec::new(),
            multi_get_results_scratch: Vec::new(),
            multi_put_scratch: Vec::new(),
        })
    }

    /// For each _unique_ key in `gets`, place the stored value (if any) in `results_out`.
    ///
    /// Panics if `gets` and `results_out` are not the same length.
    pub async fn multi_get<'r, G, R>(&mut self, gets: G, results_out: R) -> Result<u64, Error>
    where
        G: IntoIterator<Item = K>,
        R: IntoIterator<Item = &'r mut Option<V>>,
    {
        let mut multi_get_vec = std::mem::take(&mut self.multi_get_scratch);
        let mut results_vec = std::mem::take(&mut self.multi_get_results_scratch);
        multi_get_vec.clear();
        results_vec.clear();

        multi_get_vec.extend(gets);
        if multi_get_vec.is_empty() {
            self.multi_get_scratch = multi_get_vec;
            self.multi_get_results_scratch = results_vec;
            return Ok(0);
        }

        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::MultiGet {
                batch: multi_get_vec,
                results_scratch: results_vec,
                response_sender: tx,
            })
            .await
            .map_err(|_| Error::RocksDBThreadGoneAway)?;

        // We also unwrap all rocksdb errors here.
        match rx.await.map_err(|_| Error::RocksDBThreadGoneAway)? {
            Ok(mut results) => {
                let size = u64::cast_from(results.1.len());

                for (place, get) in results_out.into_iter().zip_eq(results.1.drain(..)) {
                    *place = get;
                }
                self.multi_get_scratch = results.0;
                self.multi_get_results_scratch = results.1;
                Ok(size)
            }
            Err(e) => {
                // Note we don't attempt to preserve the scratch allocations here.
                Err(e)
            }
        }
    }

    /// For each key in puts, store the given value, or delete it if
    /// the value is `None`. If the same `key` appears multiple times,
    /// the last value for the key wins.
    pub async fn multi_put<P>(&mut self, puts: P) -> Result<u64, Error>
    where
        P: IntoIterator<Item = (K, Option<V>)>,
    {
        let mut multi_put_vec = std::mem::take(&mut self.multi_put_scratch);
        multi_put_vec.clear();

        multi_put_vec.extend(puts);
        if multi_put_vec.is_empty() {
            self.multi_put_scratch = multi_put_vec;
            return Ok(0);
        }

        let size = u64::cast_from(multi_put_vec.len());

        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::MultiPut {
                batch: multi_put_vec,
                response_sender: tx,
            })
            .await
            .map_err(|_| Error::RocksDBThreadGoneAway)?;

        // We also unwrap all rocksdb errors here.
        match rx.await.map_err(|_| Error::RocksDBThreadGoneAway)? {
            Ok(scratch) => {
                self.multi_put_scratch = scratch;
                Ok(size)
            }
            Err(e) => {
                // Note we don't attempt to preserve the allocation here.
                Err(e)
            }
        }
    }

    /// Gracefully shut down RocksDB. Can error if the instance
    /// is already shut down or errored.
    pub async fn close(self) -> Result<(), Error> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::Shutdown { done_sender: tx })
            .await
            .map_err(|_| Error::RocksDBThreadGoneAway)?;

        let _ = rx.await;

        Ok(())
    }
}

// TODO(guswynn): retry retryable rocksdb errors.
fn rocksdb_core_loop<K, V, M>(
    options: Options,
    instance_path: PathBuf,
    mut cmd_rx: mpsc::Receiver<Command<K, V>>,
    metrics: M,
) where
    K: AsRef<[u8]> + Send + Sync + 'static,
    V: Serialize + DeserializeOwned + Send + Sync + 'static,
    M: Deref<Target = RocksDBMetrics> + Send + 'static,
{
    let db: DB = DB::open(&options.as_rocksdb_options(), &instance_path).unwrap();
    let wo = options.as_rocksdb_write_options();

    while let Some(cmd) = cmd_rx.blocking_recv() {
        match cmd {
            Command::Shutdown { done_sender } => {
                db.cancel_all_background_work(true);
                drop(db);
                let _ = done_sender.send(());
                return;
            }
            Command::MultiGet {
                mut batch,
                mut results_scratch,
                response_sender,
            } => {
                let batch_size = batch.len();

                // Perform the multi_get and record metrics, if there wasn't an error.
                let now = Instant::now();
                let gets = db.multi_get(batch.drain(..));
                let latency = now.elapsed();

                let gets: Result<Vec<_>, _> = gets.into_iter().collect();
                match gets {
                    Ok(gets) => {
                        metrics.multi_get_latency.observe(latency.as_secs_f64());
                        metrics.multi_get_size.observe(f64::cast_lossy(batch_size));
                        for previous_value in gets {
                            let previous_value = match previous_value
                                .map(|v| bincode::deserialize(&v))
                                .transpose()
                            {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = response_sender.send(Err(Error::DecodeError(e)));
                                    return;
                                }
                            };
                            results_scratch.push(previous_value);
                        }

                        let _ = response_sender.send(Ok((batch, results_scratch)));
                    }
                    Err(e) => {
                        let _ = response_sender.send(Err(Error::RocksDB(e)));
                        return;
                    }
                };
            }
            Command::MultiPut {
                mut batch,
                response_sender,
            } => {
                let batch_size = batch.len();
                let mut writes = rocksdb::WriteBatch::default();
                let mut encode_buf = Vec::new();

                // TODO(guswynn): sort by key before writing.
                for (key, value) in batch.drain(..) {
                    match value {
                        Some(update) => {
                            encode_buf.clear();
                            match bincode::serialize_into::<&mut Vec<u8>, _>(
                                &mut encode_buf,
                                &update,
                            ) {
                                Ok(()) => {}
                                Err(e) => {
                                    let _ = response_sender.send(Err(Error::DecodeError(e)));
                                    return;
                                }
                            };
                            writes.put(&key, encode_buf.as_slice());
                        }
                        None => writes.delete(&key),
                    }
                }
                // Perform the multi_get and record metrics, if there wasn't an error.
                let now = Instant::now();
                match db.write_opt(writes, &wo) {
                    Ok(()) => {
                        let latency = now.elapsed();
                        metrics.multi_put_latency.observe(latency.as_secs_f64());
                        metrics.multi_put_size.observe(f64::cast_lossy(batch_size));
                        let _ = response_sender.send(Ok(batch));
                    }
                    Err(e) => {
                        let _ = response_sender.send(Err(Error::RocksDB(e)));
                        return;
                    }
                }
            }
        }
    }

    // Gracefully cleanup if the `RocksDBInstance` has gone away.
    db.cancel_all_background_work(true);
    drop(db);
}
