// Copyright (c) 2024 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// todo(asoli): remove once this is fleshed out
#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tracing::debug;

use restate_core::network::{Networking, TransportConnect};
use restate_core::ShutdownError;
use restate_types::logs::metadata::SegmentIndex;
use restate_types::logs::{KeyFilter, LogId, LogletOffset, Record, SequenceNumber, TailState};
use restate_types::replicated_loglet::ReplicatedLogletParams;

use crate::loglet::util::TailOffsetWatch;
use crate::loglet::{Loglet, LogletCommit, OperationError, SendableLogletReadStream};
use crate::providers::replicated_loglet::replication::spread_selector::SelectorStrategy;
use crate::providers::replicated_loglet::sequencer::Sequencer;

use super::record_cache::RecordCache;
use super::rpc_routers::{LogServersRpc, SequencersRpc};

#[derive(derive_more::Debug)]
pub(super) struct ReplicatedLoglet<T> {
    /// This is used only to populate header of outgoing request to a remotely owned sequencer.
    /// Otherwise, it's unused.
    log_id: LogId,
    /// This is used only to populate header of outgoing request to a remotely owned sequencer.
    /// Otherwise, it's unused.
    segment_index: SegmentIndex,
    my_params: ReplicatedLogletParams,
    #[debug(skip)]
    networking: Networking<T>,
    #[debug(skip)]
    logservers_rpc: LogServersRpc,
    #[debug(skip)]
    record_cache: RecordCache,
    /// A shared watch for the last known global tail of the loglet.
    /// Note that this comes with a few caveats:
    /// - On startup, this defaults to `Open(OLDEST)`
    /// - find_tail() should use this value iff we have a local sequencer for all other cases, we
    ///   should run a proper tail search.
    known_global_tail: TailOffsetWatch,
    sequencer: SequencerAccess<T>,
}

impl<T: TransportConnect> ReplicatedLoglet<T> {
    pub fn start(
        log_id: LogId,
        segment_index: SegmentIndex,
        my_params: ReplicatedLogletParams,
        networking: Networking<T>,
        logservers_rpc: LogServersRpc,
        sequencers_rpc: &SequencersRpc,
        record_cache: RecordCache,
    ) -> Result<Self, ShutdownError> {
        let known_global_tail = TailOffsetWatch::new(TailState::Open(LogletOffset::OLDEST));

        let sequencer = if networking.my_node_id() == my_params.sequencer {
            debug!(
                loglet_id = %my_params.loglet_id,
                "We are the sequencer node for this loglet"
            );
            // todo(asoli): Potentially configurable or controllable in tests either in
            // ReplicatedLogletParams or in the config file.
            let selector_strategy = SelectorStrategy::Flood;

            SequencerAccess::Local {
                handle: Sequencer::new(
                    my_params.clone(),
                    selector_strategy,
                    networking.clone(),
                    logservers_rpc.store.clone(),
                    known_global_tail.clone(),
                ),
            }
        } else {
            SequencerAccess::Remote {
                sequencers_rpc: sequencers_rpc.clone(),
            }
        };
        Ok(Self {
            log_id,
            segment_index,
            my_params,
            networking,
            logservers_rpc,
            record_cache,
            known_global_tail,
            sequencer,
        })
    }
}

#[derive(derive_more::Debug, derive_more::IsVariant)]
pub enum SequencerAccess<T> {
    /// The sequencer is remote (or retired/preempted)
    #[debug("Remote")]
    Remote { sequencers_rpc: SequencersRpc },
    /// We are the loglet leaders
    #[debug("Local")]
    Local { handle: Sequencer<T> },
}

#[async_trait]
impl<T: TransportConnect> Loglet for ReplicatedLoglet<T> {
    async fn create_read_stream(
        self: Arc<Self>,
        _filter: KeyFilter,
        _from: LogletOffset,
        _to: Option<LogletOffset>,
    ) -> Result<SendableLogletReadStream, OperationError> {
        todo!()
    }

    fn watch_tail(&self) -> BoxStream<'static, TailState<LogletOffset>> {
        // It's acceptable for watch_tail to return an outdated value in the beginning,
        // but if the loglet is unsealed, we need to ensure that we have a mechanism to update
        // this value if we don't have a local sequencer.
        Box::pin(self.known_global_tail.to_stream())
    }

    async fn enqueue_batch(&self, payloads: Arc<[Record]>) -> Result<LogletCommit, ShutdownError> {
        match self.sequencer {
            SequencerAccess::Local { ref handle } => handle.enqueue_batch(payloads).await,
            SequencerAccess::Remote { .. } => {
                todo!("Access to remote sequencers is not implemented yet")
            }
        }
    }

    async fn find_tail(&self) -> Result<TailState<LogletOffset>, OperationError> {
        match self.sequencer {
            SequencerAccess::Local { .. } => Ok(*self.known_global_tail.get()),
            SequencerAccess::Remote { .. } => {
                todo!("find_tail() is not implemented yet")
            }
        }
    }

    async fn get_trim_point(&self) -> Result<Option<LogletOffset>, OperationError> {
        todo!()
    }

    /// Trim the log to the minimum of new_trim_point and last_committed_offset
    /// new_trim_point is inclusive (will be trimmed)
    async fn trim(&self, _new_trim_point: LogletOffset) -> Result<(), OperationError> {
        todo!()
    }

    async fn seal(&self) -> Result<(), OperationError> {
        todo!()
    }
}
