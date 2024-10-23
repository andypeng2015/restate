// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::{env, io, io::Write, time::Duration};

use anyhow::bail;
use reqwest::header::ACCEPT;
use restate_admin::service::AdminService;
use restate_bifrost::Bifrost;
use restate_core::{
    network::protobuf::node_svc::node_svc_client::NodeSvcClient, TaskKind, TestCoreEnv,
};
use restate_service_client::{AssumeRoleCacheMode, ServiceClient};
use restate_service_protocol::discovery::ServiceDiscovery;
use restate_storage_query_datafusion::table_docs;
use restate_types::{
    config::Configuration,
    identifiers::SubscriptionId,
    invocation::InvocationTermination,
    live::Constant,
    retries::RetryPolicy,
    schema::subscriptions::{Subscription, SubscriptionValidator},
    state_mut::ExternalStateMutation,
};
use restate_worker::{SubscriptionController, WorkerHandle, WorkerHandleError};
use schemars::gen::SchemaSettings;
use tonic::transport::{Channel, Uri};

fn generate_config_schema() -> anyhow::Result<()> {
    let schema = SchemaSettings::draft2019_09()
        .into_generator()
        .into_root_schema_for::<Configuration>();
    println!("{}", serde_json::to_string_pretty(&schema)?);
    Ok(())
}

fn generate_default_config() {
    println!(
        "{}",
        Configuration::default()
            .dump()
            .expect("default configuration")
    );
}

// Need this for worker Handle
#[derive(Clone)]
struct Mock;

impl WorkerHandle for Mock {
    async fn terminate_invocation(
        &self,
        _: InvocationTermination,
    ) -> Result<(), WorkerHandleError> {
        Ok(())
    }

    async fn external_state_mutation(
        &self,
        _mutation: ExternalStateMutation,
    ) -> Result<(), WorkerHandleError> {
        Ok(())
    }
}

impl SubscriptionController for Mock {
    async fn start_subscription(&self, _: Subscription) -> Result<(), WorkerHandleError> {
        Ok(())
    }

    async fn stop_subscription(&self, _: SubscriptionId) -> Result<(), WorkerHandleError> {
        Ok(())
    }

    async fn update_subscriptions(&self, _: Vec<Subscription>) -> Result<(), WorkerHandleError> {
        Ok(())
    }
}

impl SubscriptionValidator for Mock {
    type Error = WorkerHandleError;

    fn validate(&self, _: Subscription) -> Result<Subscription, Self::Error> {
        unimplemented!()
    }
}

async fn generate_rest_api_doc() -> anyhow::Result<()> {
    let config = Configuration::default();
    let openapi_address = format!(
        "http://localhost:{}/openapi",
        config.admin.bind_address.port()
    );

    // We start the Meta service, then download the openapi schema generated
    let node_env = TestCoreEnv::create_with_single_node(1, 1).await;
    let bifrost = node_env
        .tc
        .run_in_scope(
            "bifrost init",
            None,
            Bifrost::init_in_memory(node_env.metadata.clone()),
        )
        .await;

    let admin_service = AdminService::new(
        node_env.metadata_writer.clone(),
        node_env.metadata_store_client.clone(),
        Mock,
        ServiceDiscovery::new(
            RetryPolicy::default(),
            ServiceClient::from_options(&config.common.service_client, AssumeRoleCacheMode::None)
                .unwrap(),
        ),
    );

    node_env.tc.spawn(
        TaskKind::TestRunner,
        "doc-gen",
        None,
        admin_service.run(
            Constant::new(config.admin),
            NodeSvcClient::new(Channel::builder(Uri::default()).connect_lazy()),
            bifrost,
        ),
    )?;

    let res = RetryPolicy::fixed_delay(Duration::from_millis(100), Some(20))
        .retry(|| async {
            reqwest::Client::builder()
                .build()?
                .get(openapi_address.clone())
                .header(ACCEPT, "application/json")
                .send()
                .await?
                .text()
                .await
        })
        .await
        .unwrap();

    println!("{}", res);

    node_env.tc.shutdown_node("completed", 0).await;

    Ok(())
}

fn render_table_docs(mut write: impl Write) -> io::Result<()> {
    for table_doc in restate_storage_query_datafusion::table_docs::ALL_TABLE_DOCS {
        render_table_doc(table_doc, &mut write)?;
    }

    // sys_invocation is a view which was not registered at table_docs::TABLE_DOCS
    render_table_doc(&table_docs::sys_invocation_table_docs(), &mut write)?;

    Ok(())
}

fn render_table_doc(table_doc: &impl table_docs::TableDocs, w: &mut impl Write) -> io::Result<()> {
    writeln!(w, "## Table: `{}`\n", table_doc.name())?;
    writeln!(w, "| Column name | Type | Description |")?;
    writeln!(w, "|-------------|------|-------------|")?;
    for column in table_doc.columns() {
        writeln!(
            w,
            "| `{}` | `{}` | {} |",
            column.name,
            column.column_type,
            column.description.trim()
        )?;
    }
    writeln!(w)?;

    Ok(())
}

fn generate_table_docs() -> anyhow::Result<()> {
    let mut dest = io::stdout();

    // File header
    writeln!(
        &mut dest,
        r"# SQL Introspection API

This page contains the reference of the introspection tables.
To learn how to access the instrospection interface, check out the [instrospection documentation](/operate/introspection).
"
    )?;

    render_table_docs(&mut dest)?;

    Ok(())
}

fn print_help() {
    println!(
        "
Usage: Run with `cargo xtask <task>`, eg. `cargo xtask generate-config-schema`.
Tasks:
    generate-config-schema: Generate config schema for restate configuration.
    generate-default-config: Generate default configuration.
    generate-rest-api-doc: Generate Rest API documentation. Make sure to have the port 8081 open.
    generate-table-docs: Generate default configuration.
"
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let task = env::args().nth(1);
    match task {
        None => print_help(),
        Some(t) => match t.as_str() {
            "generate-config-schema" => generate_config_schema()?,
            "generate-default-config" => generate_default_config(),
            "generate-rest-api-doc" => generate_rest_api_doc().await?,
            "generate-table-docs" => generate_table_docs()?,
            invalid => {
                print_help();
                bail!("Invalid task name: {}", invalid)
            }
        },
    };
    Ok(())
}
