// Copyright (c) 2024 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::sync::Arc;

use restate_rocksdb::{
    CfExactPattern, CfName, DbName, DbSpecBuilder, RocksDb, RocksDbManager, RocksError,
};
use restate_types::arc_util::Updateable;
use restate_types::config::{LocalLogletOptions, RocksDbOptions};
use restate_types::storage::{StorageDecodeError, StorageEncodeError};
use rocksdb::{BoundColumnFamily, DBCompressionType, SliceTransform, DB};

use super::keys::{MetadataKey, MetadataKind, DATA_KEY_PREFIX_LENGTH};
use super::log_state::{log_state_full_merge, log_state_partial_merge, LogState};
use super::log_store_writer::LogStoreWriter;

// matches the default directory name
pub(crate) const DB_NAME: &str = "local-loglet";

pub(crate) const DATA_CF: &str = "logstore_data";
pub(crate) const METADATA_CF: &str = "logstore_metadata";

#[derive(Debug, Clone, thiserror::Error)]
pub enum LogStoreError {
    #[error(transparent)]
    // unfortunately, we have to use Arc here, because the storage encode error is not Clone.
    Encode(#[from] Arc<StorageEncodeError>),
    #[error(transparent)]
    // unfortunately, we have to use Arc here, because the storage decode error is not Clone.
    Decode(#[from] Arc<StorageDecodeError>),
    #[error(transparent)]
    Rocksdb(#[from] rocksdb::Error),
    #[error(transparent)]
    RocksDbManager(#[from] RocksError),
}

#[derive(Debug, Clone)]
pub struct RocksDbLogStore {
    rocksdb: Arc<RocksDb>,
}

impl RocksDbLogStore {
    pub fn new(
        options: &LocalLogletOptions,
        updateable_options: impl Updateable<RocksDbOptions> + Send + 'static,
    ) -> Result<Self, LogStoreError> {
        let db_manager = RocksDbManager::get();

        let cfs = vec![CfName::new(DATA_CF), CfName::new(METADATA_CF)];

        let data_dir = options.data_dir();

        let db_spec = DbSpecBuilder::new(DbName::new(DB_NAME), data_dir, db_options(options))
            .add_cf_pattern(CfExactPattern::new(DATA_CF), cf_data_options)
            .add_cf_pattern(CfExactPattern::new(METADATA_CF), cf_metadata_options)
            // not very important but it's to reduce the number of merges by flushing.
            // it's also a small cf so it should be quick.
            .add_to_flush_on_shutdown(CfExactPattern::new(METADATA_CF))
            .ensure_column_families(cfs)
            .build_as_db();
        let db_name = db_spec.name().clone();
        // todo: use the returned rocksdb object when open_db returns Arc<RocksDb>
        let _ = db_manager.open_db(updateable_options, db_spec)?;
        let rocksdb = db_manager.get_db(db_name).unwrap();
        Ok(Self { rocksdb })
    }

    pub fn data_cf(&self) -> Arc<BoundColumnFamily> {
        self.rocksdb
            .inner()
            .cf_handle(DATA_CF)
            .expect("DATA_CF exists")
    }

    pub fn metadata_cf(&self) -> Arc<BoundColumnFamily> {
        self.rocksdb
            .inner()
            .cf_handle(METADATA_CF)
            .expect("METADATA_CF exists")
    }

    pub fn get_log_state(&self, log_id: u64) -> Result<Option<LogState>, LogStoreError> {
        let metadata_cf = self.metadata_cf();
        let value = self.rocksdb.inner().as_raw_db().get_pinned_cf(
            &metadata_cf,
            MetadataKey::new(log_id, MetadataKind::LogState).to_bytes(),
        )?;

        if let Some(value) = value {
            Ok(Some(LogState::from_slice(&value)?))
        } else {
            Ok(None)
        }
    }

    pub fn create_writer(&self, manual_wal_flush: bool) -> LogStoreWriter {
        LogStoreWriter::new(self.rocksdb.clone(), manual_wal_flush)
    }

    pub fn db(&self) -> &DB {
        self.rocksdb.inner().as_raw_db()
    }
}

fn db_options(options: &LocalLogletOptions) -> rocksdb::Options {
    let mut opts = rocksdb::Options::default();
    //
    // no need to retain 1000 log files by default.
    //
    opts.set_keep_log_file_num(10);

    if !options.rocksdb.rocksdb_disable_wal() {
        opts.set_manual_wal_flush(options.batch_wal_flushes);
    }

    // unconditionally enable atomic flushes to not persist inconsistent data in case WAL
    // is disabled
    opts.set_atomic_flush(true);

    opts
}

// todo: optimize
fn cf_data_options(mut opts: rocksdb::Options) -> rocksdb::Options {
    //
    // Set compactions per level
    //
    opts.set_max_write_buffer_number(10);
    opts.set_num_levels(7);
    opts.set_compression_per_level(&[
        DBCompressionType::None,
        DBCompressionType::Snappy,
        DBCompressionType::Zstd,
        DBCompressionType::Zstd,
        DBCompressionType::Zstd,
        DBCompressionType::Zstd,
        DBCompressionType::Zstd,
    ]);

    opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(DATA_KEY_PREFIX_LENGTH));
    opts.set_memtable_prefix_bloom_ratio(0.2);
    // most reads are sequential
    opts.set_advise_random_on_open(false);
    //
    opts
}

// todo: optimize
fn cf_metadata_options(mut opts: rocksdb::Options) -> rocksdb::Options {
    //
    // Set compactions per level
    //
    opts.set_num_levels(3);
    opts.set_compression_per_level(&[
        DBCompressionType::None,
        DBCompressionType::Snappy,
        DBCompressionType::Zstd,
    ]);
    opts.set_max_write_buffer_number(10);
    opts.set_max_successive_merges(10);
    // Merge operator for log state updates
    opts.set_merge_operator(
        "LogStateMerge",
        log_state_full_merge,
        log_state_partial_merge,
    );
    opts
}
