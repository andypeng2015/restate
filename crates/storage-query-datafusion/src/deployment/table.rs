// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::sync::Arc;

use datafusion::{
    arrow::{datatypes::SchemaRef, record_batch::RecordBatch},
    logical_expr::Expr,
    physical_plan::{stream::RecordBatchReceiverStream, SendableRecordBatchStream},
};
use restate_types::{
    identifiers::ServiceRevision,
    live::Live,
    schema::deployment::{Deployment, DeploymentResolver},
};
use tokio::sync::mpsc::Sender;

use super::schema::SysDeploymentBuilder;
use crate::{
    context::QueryContext,
    deployment::row::append_deployment_row,
    table_providers::{GenericTableProvider, Scan},
    table_util::Builder,
};

pub(crate) fn register_self(
    ctx: &QueryContext,
    resolver: Live<impl DeploymentResolver + Send + Sync + 'static>,
) -> datafusion::common::Result<()> {
    let deployment_table = GenericTableProvider::new(
        SysDeploymentBuilder::schema(),
        Arc::new(DeploymentMetadataScanner(resolver)),
    );

    ctx.as_ref()
        .register_table("sys_deployment", Arc::new(deployment_table))
        .map(|_| ())
}

#[derive(Clone, derive_more::Debug)]
#[debug("DeploymentMetadataScanner")]
struct DeploymentMetadataScanner<DMR>(Live<DMR>);

impl<DMR: DeploymentResolver + Sync + Send + 'static> Scan for DeploymentMetadataScanner<DMR> {
    fn scan(
        &self,
        projection: SchemaRef,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> SendableRecordBatchStream {
        let schema = projection.clone();
        let mut stream_builder = RecordBatchReceiverStream::builder(projection, 16);
        let tx = stream_builder.tx();

        let rows = self.0.pinned().get_deployments();
        stream_builder.spawn(async move {
            for_each_state(schema, tx, rows).await;
            Ok(())
        });
        stream_builder.build()
    }
}

async fn for_each_state(
    schema: SchemaRef,
    tx: Sender<datafusion::common::Result<RecordBatch>>,
    rows: Vec<(Deployment, Vec<(String, ServiceRevision)>)>,
) {
    let mut builder = SysDeploymentBuilder::new(schema.clone());
    let mut temp = String::new();
    for (deployment, _) in rows {
        append_deployment_row(&mut builder, &mut temp, deployment);
        if builder.full() {
            let batch = builder.finish();
            if tx.send(batch).await.is_err() {
                // not sure what to do here?
                // the other side has hung up on us.
                // we probably don't want to panic, is it will cause the entire process to exit
                return;
            }
            builder = SysDeploymentBuilder::new(schema.clone());
        }
    }
    if !builder.empty() {
        let result = builder.finish();
        let _ = tx.send(result).await;
    }
}
