// Copyright (c) 2023 - 2025 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use futures::Stream;
use std::fmt::Debug;
use std::ops::RangeInclusive;
use std::sync::Arc;

use restate_partition_store::{PartitionStore, PartitionStoreManager};
use restate_storage_api::journal_table::{JournalEntry, ReadOnlyJournalTable};
use restate_types::identifiers::{JournalEntryId, PartitionKey};

use crate::context::{QueryContext, SelectPartitions};
use crate::journal::row::append_journal_row;
use crate::journal::schema::SysJournalBuilder;
use crate::partition_filter::FirstMatchingPartitionKeyExtractor;
use crate::partition_store_scanner::{LocalPartitionsScanner, ScanLocalPartition};
use crate::table_providers::{PartitionedTableProvider, ScanPartition};

const NAME: &str = "sys_journal";

pub(crate) fn register_self(
    ctx: &QueryContext,
    partition_selector: impl SelectPartitions,
    local_partition_store_manager: Option<PartitionStoreManager>,
) -> datafusion::common::Result<()> {
    let local_scanner = local_partition_store_manager.map(|partition_store_manager| {
        Arc::new(LocalPartitionsScanner::new(
            partition_store_manager,
            JournalScanner,
        )) as Arc<dyn ScanPartition>
    });
    let journal_table = PartitionedTableProvider::new(
        partition_selector,
        SysJournalBuilder::schema(),
        ctx.create_distributed_scanner(NAME, local_scanner),
        FirstMatchingPartitionKeyExtractor::default().with_invocation_id("id"),
    );
    ctx.register_partitioned_table(NAME, Arc::new(journal_table))
}

#[derive(Debug, Clone)]
struct JournalScanner;

impl ScanLocalPartition for JournalScanner {
    type Builder = SysJournalBuilder;
    type Item = (JournalEntryId, JournalEntry);

    fn scan_partition_store(
        partition_store: &PartitionStore,
        range: RangeInclusive<PartitionKey>,
    ) -> impl Stream<Item = restate_storage_api::Result<Self::Item>> + Send {
        partition_store.all_journals(range)
    }

    fn append_row(row_builder: &mut Self::Builder, string_buffer: &mut String, value: Self::Item) {
        append_journal_row(row_builder, string_buffer, value.0, value.1);
    }
}
