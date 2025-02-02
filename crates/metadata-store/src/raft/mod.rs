// Copyright (c) 2023 - 2025 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

mod kv_memory_storage;
mod storage;
mod store;

use crate::network::{MetadataStoreNetworkHandler, MetadataStoreNetworkSvcServer, NetworkMessage};
use crate::raft::store::BuildError;
use crate::{network, MemberId, MetadataStoreRunner};
use anyhow::Context;
use bytes::{Buf, BufMut};
use protobuf::Message as ProtobufMessage;
use restate_core::network::NetworkServerBuilder;
use restate_core::MetadataWriter;
use restate_types::config::RocksDbOptions;
use restate_types::health::HealthStatus;
use restate_types::live::BoxedLiveLoad;
use restate_types::protobuf::common::MetadataServerStatus;
pub use store::RaftMetadataStore;

pub(crate) async fn create_store(
    rocksdb_options: BoxedLiveLoad<RocksDbOptions>,
    health_status: HealthStatus<MetadataServerStatus>,
    metadata_writer: Option<MetadataWriter>,
    server_builder: &mut NetworkServerBuilder,
) -> Result<MetadataStoreRunner<RaftMetadataStore>, BuildError> {
    let store = RaftMetadataStore::create(rocksdb_options, metadata_writer, health_status).await?;

    server_builder.register_grpc_service(
        MetadataStoreNetworkSvcServer::new(MetadataStoreNetworkHandler::new(
            store.connection_manager(),
            Some(store.join_cluster_handle()),
        )),
        network::FILE_DESCRIPTOR_SET,
    );

    Ok(MetadataStoreRunner::new(store, server_builder))
}

impl NetworkMessage for raft::prelude::Message {
    fn to(&self) -> u64 {
        self.to
    }

    fn serialize<B: BufMut>(&self, buffer: &mut B) {
        let mut writer = buffer.writer();
        self.write_to_writer(&mut writer)
            .expect("should be able to write message");
    }

    fn deserialize<B: Buf>(buffer: &mut B) -> anyhow::Result<Self> {
        ProtobufMessage::parse_from_reader(&mut buffer.reader())
            .context("failed deserializing message")
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct RaftConfiguration {
    my_member_id: MemberId,
}
