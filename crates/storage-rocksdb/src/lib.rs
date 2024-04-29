// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

pub mod deduplication_table;
pub mod fsm_table;
pub mod idempotency_table;
pub mod inbox_table;
pub mod invocation_status_table;
pub mod journal_table;
pub mod keys;
pub mod outbox_table;
mod owned_iter;
pub mod scan;
pub mod service_status_table;
pub mod state_table;
pub mod timer_table;

use crate::keys::TableKey;
use crate::scan::{PhysicalScan, TableScan};
use crate::TableKind::{
    Deduplication, Idempotency, Inbox, InvocationStatus, Journal, Outbox, PartitionStateMachine,
    ServiceStatus, State, Timers,
};

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use codederror::CodedError;
use rocksdb::DBCompressionType;
use rocksdb::DBPinnableSlice;
use rocksdb::DBRawIteratorWithThreadMode;
use rocksdb::MultiThreaded;
use rocksdb::PrefixRange;
use rocksdb::ReadOptions;
use rocksdb::{BoundColumnFamily, SliceTransform};
use static_assertions::const_assert_eq;

use restate_core::ShutdownError;
use restate_rocksdb::{
    CfName, CfPrefixPattern, DbName, DbSpecBuilder, Owner, RocksDbManager, RocksError,
};
use restate_storage_api::{Storage, StorageError, Transaction};
use restate_types::arc_util::Updateable;
use restate_types::config::{RocksDbOptions, StorageOptions};
use restate_types::identifiers::{PartitionId, PartitionKey};
use restate_types::storage::{StorageCodec, StorageDecode, StorageEncode};

use self::keys::KeyKind;

pub type DB = rocksdb::OptimisticTransactionDB<MultiThreaded>;
type TransactionDB<'a> = rocksdb::Transaction<'a, DB>;

pub type DBIterator<'b> = DBRawIteratorWithThreadMode<'b, DB>;
pub type DBIteratorTransaction<'b> = DBRawIteratorWithThreadMode<'b, rocksdb::Transaction<'b, DB>>;

// matches the default directory name
const DB_NAME: &str = "db";

pub const PARTITION_CF: &str = "data-unpartitioned";

//Key prefix is 10 bytes (KeyKind(2) + PartitionKey/Id(8))
const DB_PREFIX_LENGTH: usize = KeyKind::SERIALIZED_LENGTH + std::mem::size_of::<PartitionKey>();

// If this changes, we need to know.
const_assert_eq!(DB_PREFIX_LENGTH, 10);

// Ensures that both types have the same length, this makes it possible to
// share prefix extractor in rocksdb.
const_assert_eq!(
    std::mem::size_of::<PartitionKey>(),
    std::mem::size_of::<PartitionId>(),
);

pub(crate) type Result<T> = std::result::Result<T, StorageError>;

pub enum TableScanIterationDecision<R> {
    Emit(Result<R>),
    Continue,
    Break,
    BreakWith(Result<R>),
}

#[inline]
const fn cf_name(_kind: TableKind) -> &'static str {
    PARTITION_CF
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum TableKind {
    // By Partition ID
    PartitionStateMachine,
    Deduplication,
    Outbox,
    Timers,
    // By Partition Key
    State,
    InvocationStatus,
    ServiceStatus,
    Idempotency,
    Inbox,
    Journal,
}

impl TableKind {
    pub fn all() -> core::slice::Iter<'static, TableKind> {
        static VARIANTS: &[TableKind] = &[
            State,
            InvocationStatus,
            ServiceStatus,
            Idempotency,
            Inbox,
            Outbox,
            Deduplication,
            PartitionStateMachine,
            Timers,
            Journal,
        ];
        VARIANTS.iter()
    }

    pub const fn key_kinds(self) -> &'static [KeyKind] {
        match self {
            State => &[KeyKind::State],
            InvocationStatus => &[KeyKind::InvocationStatus],
            ServiceStatus => &[KeyKind::ServiceStatus],
            Idempotency => &[KeyKind::Idempotency],
            Inbox => &[KeyKind::Inbox],
            Outbox => &[KeyKind::Outbox],
            Deduplication => &[KeyKind::Deduplication],
            PartitionStateMachine => &[KeyKind::Fsm],
            Timers => &[KeyKind::Timers],
            Journal => &[KeyKind::Journal],
        }
    }

    pub fn has_key_kind(self, prefix: &[u8]) -> bool {
        self.extract_key_kind(prefix).is_some()
    }

    pub fn extract_key_kind(self, prefix: &[u8]) -> Option<KeyKind> {
        if prefix.len() < KeyKind::SERIALIZED_LENGTH {
            return None;
        }
        let slice = prefix[..KeyKind::SERIALIZED_LENGTH].try_into().unwrap();
        let Some(kind) = KeyKind::from_bytes(slice) else {
            // warning
            return None;
        };
        self.key_kinds().iter().find(|k| **k == kind).copied()
    }
}

#[derive(Debug, thiserror::Error, CodedError)]
pub enum BuildError {
    #[error(transparent)]
    RocksDbManager(
        #[from]
        #[code]
        RocksError,
    ),
    #[error("db contains no storage format version")]
    #[code(restate_errors::RT0009)]
    MissingStorageFormatVersion,
    #[error(transparent)]
    #[code(unknown)]
    Other(#[from] rocksdb::Error),
    #[error(transparent)]
    #[code(unknown)]
    Shutdown(#[from] ShutdownError),
}

pub struct RocksDBStorage {
    db: Arc<DB>,
    key_buffer: BytesMut,
    value_buffer: BytesMut,
}

impl std::fmt::Debug for RocksDBStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksDBStorage")
            .field("db", &self.db)
            .field("key_buffer", &self.key_buffer)
            .field("value_buffer", &self.value_buffer)
            .finish()
    }
}

impl Clone for RocksDBStorage {
    fn clone(&self) -> Self {
        RocksDBStorage {
            db: self.db.clone(),
            key_buffer: BytesMut::default(),
            value_buffer: BytesMut::default(),
        }
    }
}

fn db_options() -> rocksdb::Options {
    let mut db_options = rocksdb::Options::default();
    // no need to retain 1000 log files by default.
    //
    db_options.set_keep_log_file_num(1);

    // we always need to enable atomic flush in case that the user disables wal at runtime
    db_options.set_atomic_flush(true);

    // we always enable manual wal flushing in case that the user enables wal at runtime
    db_options.set_manual_wal_flush(true);

    db_options
}

fn cf_options(mut cf_options: rocksdb::Options) -> rocksdb::Options {
    // Actually, we would love to use CappedPrefixExtractor but unfortunately it's neither exposed
    // in the C API nor the rust binding. That's okay and we can change it later.
    cf_options.set_prefix_extractor(SliceTransform::create_fixed_prefix(DB_PREFIX_LENGTH));
    cf_options.set_memtable_prefix_bloom_ratio(0.2);
    // Most of the changes are highly temporal, we try to delay flushing
    // As much as we can to increase the chances to observe a deletion.
    //
    cf_options.set_max_write_buffer_number(3);
    cf_options.set_min_write_buffer_number_to_merge(2);
    //
    // Set compactions per level
    //
    cf_options.set_num_levels(7);
    cf_options.set_compression_per_level(&[
        DBCompressionType::None,
        DBCompressionType::Snappy,
        DBCompressionType::Snappy,
        DBCompressionType::Snappy,
        DBCompressionType::Snappy,
        DBCompressionType::Snappy,
        DBCompressionType::Zstd,
    ]);

    cf_options
}

impl RocksDBStorage {
    /// Returns the raw rocksdb handle, this should only be used for server operations that
    /// require direct access to rocksdb.
    pub fn inner(&self) -> Arc<DB> {
        self.db.clone()
    }

    pub async fn open(
        mut storage_opts: impl Updateable<StorageOptions> + Send + 'static,
        updateable_opts: impl Updateable<RocksDbOptions> + Send + 'static,
    ) -> std::result::Result<Self, BuildError> {
        let cfs = vec![CfName::new(PARTITION_CF)];

        let options = storage_opts.load();
        let db_spec = DbSpecBuilder::new(
            DbName::new(DB_NAME),
            Owner::PartitionProcessor,
            options.data_dir(),
            db_options(),
        )
        // At the moment, all CFs get the same options, that might change in the future.
        .add_cf_pattern(CfPrefixPattern::ANY, cf_options)
        .ensure_column_families(cfs)
        .build_as_optimistic_db();

        // todo remove this when open_db is async
        let rdb = tokio::task::spawn_blocking(move || {
            RocksDbManager::get().open_db(updateable_opts, db_spec)
        })
        .await
        .map_err(|_| ShutdownError)??;

        Ok(Self {
            db: rdb,
            key_buffer: BytesMut::default(),
            value_buffer: BytesMut::default(),
        })
    }

    fn table_handle(&self, table_kind: TableKind) -> Arc<BoundColumnFamily> {
        self.db.cf_handle(cf_name(table_kind)).expect(
            "This should not happen, this is a Restate bug. Please contact the restate developers.",
        )
    }

    fn prefix_iterator(&self, table: TableKind, _key_kind: KeyKind, prefix: Bytes) -> DBIterator {
        let table = self.table_handle(table);
        let mut opts = ReadOptions::default();
        opts.set_prefix_same_as_start(true);
        opts.set_iterate_range(PrefixRange(prefix.clone()));
        opts.set_async_io(true);
        opts.set_total_order_seek(false);
        let mut it = self.db.raw_iterator_cf_opt(&table, opts);
        it.seek(prefix);
        it
    }

    fn range_iterator(
        &self,
        table: TableKind,
        _key: KeyKind,
        scan_mode: ScanMode,
        from: Bytes,
        to: Bytes,
    ) -> DBIterator {
        let table = self.table_handle(table);
        let mut opts = ReadOptions::default();
        // todo: use auto_prefix_mode, at the moment, rocksdb doesn't expose this through the C
        // binding.
        opts.set_total_order_seek(scan_mode == ScanMode::TotalOrder);
        opts.set_iterate_range(from.clone()..to);
        opts.set_async_io(true);

        let mut it = self.db.raw_iterator_cf_opt(&table, opts);
        it.seek(from);
        it
    }

    #[track_caller]
    fn iterator_from<K: TableKey>(
        &self,
        scan: TableScan<K>,
    ) -> DBRawIteratorWithThreadMode<'_, DB> {
        let scan: PhysicalScan = scan.into();
        match scan {
            PhysicalScan::Prefix(table, key_kind, prefix) => {
                assert!(table.has_key_kind(&prefix));
                self.prefix_iterator(table, key_kind, prefix.freeze())
            }
            PhysicalScan::RangeExclusive(table, key_kind, scan_mode, start, end) => {
                assert!(table.has_key_kind(&start));
                self.range_iterator(table, key_kind, scan_mode, start.freeze(), end.freeze())
            }
            PhysicalScan::RangeOpen(table, key_kind, start) => {
                // We delayed the generate the synthetic iterator upper bound until this point
                // because we might have different prefix length requirements based on the
                // table+key_kind combination and we should keep this knowledge as low-level as
                // possible.
                //
                // make the end has the same length as all prefixes to ensure rocksdb key
                // comparator can leverage bloom filters when applicable
                // (if auto_prefix_mode is enabled)
                let mut end = BytesMut::zeroed(DB_PREFIX_LENGTH);
                // We want to ensure that Range scans fall within the same key kind.
                // So, we limit the iterator to the upper bound of this prefix
                let kind_upper_bound = K::KEY_KIND.exclusive_upper_bound();
                end[..kind_upper_bound.len()].copy_from_slice(&kind_upper_bound);
                self.range_iterator(
                    table,
                    key_kind,
                    ScanMode::TotalOrder,
                    start.freeze(),
                    end.freeze(),
                )
            }
        }
    }

    #[allow(clippy::needless_lifetimes)]
    pub fn transaction(&mut self) -> RocksDBTransaction {
        let db = self.db.clone();

        RocksDBTransaction {
            txn: self.db.transaction(),
            db,
            key_buffer: &mut self.key_buffer,
            value_buffer: &mut self.value_buffer,
        }
    }
}

impl Storage for RocksDBStorage {
    type TransactionType<'a> = RocksDBTransaction<'a>;

    fn transaction(&mut self) -> Self::TransactionType<'_> {
        RocksDBStorage::transaction(self)
    }
}

impl StorageAccess for RocksDBStorage {
    type DBAccess<'a>
    = DB where
        Self: 'a,;

    fn iterator_from<K: TableKey>(
        &self,
        scan: TableScan<K>,
    ) -> DBRawIteratorWithThreadMode<'_, Self::DBAccess<'_>> {
        self.iterator_from(scan)
    }

    #[inline]
    fn cleared_key_buffer_mut(&mut self, min_size: usize) -> &mut BytesMut {
        self.key_buffer.clear();
        self.key_buffer.reserve(min_size);
        &mut self.key_buffer
    }

    #[inline]
    fn cleared_value_buffer_mut(&mut self, min_size: usize) -> &mut BytesMut {
        self.value_buffer.clear();
        self.value_buffer.reserve(min_size);
        &mut self.value_buffer
    }

    #[inline]
    fn get<K: AsRef<[u8]>>(&self, table: TableKind, key: K) -> Result<Option<DBPinnableSlice>> {
        let table = self.table_handle(table);
        self.db
            .get_pinned_cf(&table, key)
            .map_err(|error| StorageError::Generic(error.into()))
    }

    #[inline]
    fn put_cf(&mut self, table: TableKind, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) {
        let table = self.table_handle(table);
        self.db.put_cf(&table, key, value).unwrap();
    }

    #[inline]
    fn delete_cf(&mut self, table: TableKind, key: impl AsRef<[u8]>) {
        let table = self.table_handle(table);
        self.db.delete_cf(&table, key).unwrap();
    }
}

pub struct RocksDBTransaction<'a> {
    txn: rocksdb::Transaction<'a, DB>,
    db: Arc<DB>,
    key_buffer: &'a mut BytesMut,
    value_buffer: &'a mut BytesMut,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ScanMode {
    /// Scan is bound to a single fixed key prefix (partition id, or a single partition key).
    WithinPrefix,
    /// Scan/iterator requires total order seek, this means that the iterator is not bound to a
    /// fixed prefix that matches the column family prefix extractor length. For instance, if
    /// scanning data across multiple partition IDs or multiple partition keys.
    TotalOrder,
}

impl<'a> RocksDBTransaction<'a> {
    pub(crate) fn prefix_iterator(
        &self,
        table: TableKind,
        _key_kind: KeyKind,
        prefix: Bytes,
    ) -> DBIteratorTransaction {
        let table = self.table_handle(table);
        let mut opts = ReadOptions::default();
        opts.set_iterate_range(PrefixRange(prefix.clone()));
        opts.set_prefix_same_as_start(true);
        opts.set_async_io(true);
        opts.set_total_order_seek(false);

        let mut it = self.txn.raw_iterator_cf_opt(&table, opts);
        it.seek(prefix);
        it
    }

    pub(crate) fn range_iterator(
        &self,
        table: TableKind,
        _key_kind: KeyKind,
        scan_mode: ScanMode,
        from: Bytes,
        to: Bytes,
    ) -> DBIteratorTransaction {
        let table = self.table_handle(table);
        let mut opts = ReadOptions::default();
        // todo: use auto_prefix_mode, at the moment, rocksdb doesn't expose this through the C
        // binding.
        opts.set_total_order_seek(scan_mode == ScanMode::TotalOrder);
        opts.set_iterate_range(from.clone()..to);
        opts.set_async_io(true);
        let mut it = self.txn.raw_iterator_cf_opt(&table, opts);
        it.seek(from);
        it
    }

    pub(crate) fn table_handle(&self, table_kind: TableKind) -> Arc<BoundColumnFamily> {
        self.db.cf_handle(cf_name(table_kind)).expect(
            "This should not happen, this is a Restate bug. Please contact the restate developers.",
        )
    }
}

impl<'a> Transaction for RocksDBTransaction<'a> {
    async fn commit(self) -> Result<()> {
        // We cannot directly commit the txn because it might fail because of unrelated concurrent
        // writes to RocksDB. However, it is safe to write the WriteBatch for a given partition,
        // because there can only be a single writer (the leading PartitionProcessor).
        let write_batch = self.txn.get_writebatch();
        // todo: make async and use configuration to control use of WAL
        if write_batch.is_empty() {
            return Ok(());
        }
        let mut opts = rocksdb::WriteOptions::default();
        // We disable WAL since bifrost is our durable distributed log.
        opts.disable_wal(true);
        self.db
            .write_opt(&write_batch, &rocksdb::WriteOptions::default())
            .map_err(|error| StorageError::Generic(error.into()))
    }
}

impl<'a> StorageAccess for RocksDBTransaction<'a> {
    type DBAccess<'b> = TransactionDB<'b> where Self: 'b;

    fn iterator_from<K: TableKey>(
        &self,
        scan: TableScan<K>,
    ) -> DBRawIteratorWithThreadMode<'_, Self::DBAccess<'_>> {
        let scan: PhysicalScan = scan.into();
        match scan {
            PhysicalScan::Prefix(table, key_kind, prefix) => {
                self.prefix_iterator(table, key_kind, prefix.freeze())
            }
            PhysicalScan::RangeExclusive(table, key_kind, scan_mode, start, end) => {
                self.range_iterator(table, key_kind, scan_mode, start.freeze(), end.freeze())
            }
            PhysicalScan::RangeOpen(table, key_kind, start) => {
                // We delayed the generate the synthetic iterator upper bound until this point
                // because we might have different prefix length requirements based on the
                // table+key_kind combination and we should keep this knowledge as low-level as
                // possible.
                //
                // make the end has the same length as all prefixes to ensure rocksdb key
                // comparator can leverage bloom filters when applicable
                // (if auto_prefix_mode is enabled)
                let mut end = BytesMut::zeroed(DB_PREFIX_LENGTH);
                // We want to ensure that Range scans fall within the same key kind.
                // So, we limit the iterator to the upper bound of this prefix
                let kind_upper_bound = K::KEY_KIND.exclusive_upper_bound();
                end[..kind_upper_bound.len()].copy_from_slice(&kind_upper_bound);
                self.range_iterator(
                    table,
                    key_kind,
                    ScanMode::WithinPrefix,
                    start.freeze(),
                    end.freeze(),
                )
            }
        }
    }

    #[inline]
    fn cleared_key_buffer_mut(&mut self, min_size: usize) -> &mut BytesMut {
        self.key_buffer.clear();
        self.key_buffer.reserve(min_size);
        self.key_buffer
    }

    #[inline]
    fn cleared_value_buffer_mut(&mut self, min_size: usize) -> &mut BytesMut {
        self.value_buffer.clear();
        self.value_buffer.reserve(min_size);
        self.value_buffer
    }

    #[inline]
    fn get<K: AsRef<[u8]>>(&self, table: TableKind, key: K) -> Result<Option<DBPinnableSlice>> {
        let table = self.table_handle(table);
        self.txn
            .get_pinned_cf(&table, key)
            .map_err(|error| StorageError::Generic(error.into()))
    }

    #[inline]
    fn put_cf(&mut self, table: TableKind, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) {
        let table = self.table_handle(table);
        self.txn.put_cf(&table, key, value).unwrap();
    }

    #[inline]
    fn delete_cf(&mut self, table: TableKind, key: impl AsRef<[u8]>) {
        let table = self.table_handle(table);
        self.txn.delete_cf(&table, key).unwrap();
    }
}

trait StorageAccess {
    type DBAccess<'a>: rocksdb::DBAccess
    where
        Self: 'a;

    fn iterator_from<K: TableKey>(
        &self,
        scan: TableScan<K>,
    ) -> DBRawIteratorWithThreadMode<'_, Self::DBAccess<'_>>;

    fn cleared_key_buffer_mut(&mut self, min_size: usize) -> &mut BytesMut;

    fn cleared_value_buffer_mut(&mut self, min_size: usize) -> &mut BytesMut;

    fn get<K: AsRef<[u8]>>(&self, table: TableKind, key: K) -> Result<Option<DBPinnableSlice>>;

    fn put_cf(&mut self, table: TableKind, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>);

    fn delete_cf(&mut self, table: TableKind, key: impl AsRef<[u8]>);

    #[inline]
    fn put_kv_raw<K: TableKey, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        let key_buffer = self.cleared_key_buffer_mut(key.serialized_length());
        key.serialize_to(key_buffer);
        let key_buffer = key_buffer.split();

        self.put_cf(K::TABLE, key_buffer, value);
    }

    #[inline]
    fn put_kv<K: TableKey, V: StorageEncode>(&mut self, key: K, value: V) {
        let key_buffer = self.cleared_key_buffer_mut(key.serialized_length());
        key.serialize_to(key_buffer);
        let key_buffer = key_buffer.split();

        let value_buffer = self.cleared_value_buffer_mut(0);
        StorageCodec::encode(&value, value_buffer).unwrap();
        let value_buffer = value_buffer.split();

        self.put_cf(K::TABLE, key_buffer, value_buffer);
    }

    #[inline]
    fn delete_key<K: TableKey>(&mut self, key: &K) {
        let buffer = self.cleared_key_buffer_mut(key.serialized_length());
        key.serialize_to(buffer);
        let buffer = buffer.split();

        self.delete_cf(K::TABLE, buffer);
    }

    #[inline]
    fn get_value<K, V>(&mut self, key: K) -> Result<Option<V>>
    where
        K: TableKey,
        V: StorageDecode,
    {
        let mut buf = self.cleared_key_buffer_mut(key.serialized_length());
        key.serialize_to(&mut buf);
        let buf = buf.split();

        match self.get(K::TABLE, &buf) {
            Ok(value) => {
                let slice = value.as_ref().map(|v| v.as_ref());

                if let Some(mut slice) = slice {
                    Ok(Some(
                        StorageCodec::decode::<V, _>(&mut slice)
                            .map_err(|err| StorageError::Generic(err.into()))?,
                    ))
                } else {
                    Ok(None)
                }
            }
            Err(err) => Err(err),
        }
    }

    #[inline]
    fn get_first_blocking<K, F, R>(&mut self, scan: TableScan<K>, f: F) -> Result<R>
    where
        K: TableKey,
        F: FnOnce(Option<(&[u8], &[u8])>) -> Result<R>,
    {
        let iterator = self.iterator_from(scan);
        f(iterator.item())
    }

    #[inline]
    fn get_kv_raw<K, F, R>(&mut self, key: K, f: F) -> Result<R>
    where
        K: TableKey,
        F: FnOnce(&[u8], Option<&[u8]>) -> Result<R>,
    {
        let mut buf = self.cleared_key_buffer_mut(key.serialized_length());
        key.serialize_to(&mut buf);
        let buf = buf.split();

        match self.get(K::TABLE, &buf) {
            Ok(value) => {
                let slice = value.as_ref().map(|v| v.as_ref());
                f(&buf, slice)
            }
            Err(err) => Err(err),
        }
    }

    #[inline]
    fn for_each_key_value_in_place<K, F, R>(&self, scan: TableScan<K>, mut op: F) -> Vec<Result<R>>
    where
        K: TableKey,
        F: FnMut(&[u8], &[u8]) -> TableScanIterationDecision<R>,
    {
        let mut res = Vec::new();

        let mut iterator = self.iterator_from(scan);

        while let Some((k, v)) = iterator.item() {
            match op(k, v) {
                TableScanIterationDecision::Emit(result) => {
                    res.push(result);
                    iterator.next();
                }
                TableScanIterationDecision::BreakWith(result) => {
                    res.push(result);
                    break;
                }
                TableScanIterationDecision::Continue => {
                    iterator.next();
                    continue;
                }
                TableScanIterationDecision::Break => {
                    break;
                }
            };
        }

        res
    }
}
