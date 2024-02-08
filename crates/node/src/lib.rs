// Copyright (c) 2024 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

pub mod cluster_controller;
mod options;
pub mod worker;

use codederror::CodedError;
use futures::future::OptionFuture;
use restate_types::NodeId;
use std::convert::Infallible;
use std::str::FromStr;
use std::time::Duration;
use tokio::task::JoinError;
use tonic::codegen::http::uri::InvalidUri;
use tonic::transport::{Channel, Uri};
use tracing::{info, instrument};

use crate::cluster_controller::ClusterControllerRole;
use crate::worker::WorkerRole;
pub use options::{Options, OptionsBuilder as NodeOptionsBuilder};
pub use restate_admin::OptionsBuilder as AdminOptionsBuilder;
use restate_cluster_controller::proto::cluster_controller_client::ClusterControllerClient;
use restate_cluster_controller::proto::AttachmentRequest;
pub use restate_meta::OptionsBuilder as MetaOptionsBuilder;
use restate_types::retries::RetryPolicy;
pub use restate_worker::{OptionsBuilder as WorkerOptionsBuilder, RocksdbOptionsBuilder};

#[derive(Debug, thiserror::Error, CodedError)]
pub enum Error {
    #[error("worker failed: {0}")]
    Worker(
        #[from]
        #[code]
        worker::WorkerRoleError,
    ),
    #[error("controller failed: {0}")]
    Controller(
        #[from]
        #[code]
        cluster_controller::ClusterControllerRoleError,
    ),
    #[error("failed to attach to cluster at '{0}': {1}")]
    #[code(unknown)]
    Attachment(Uri, tonic::Status),
    #[error("component '{0}' panicked: {1}")]
    #[code(unknown)]
    Panic(&'static str, JoinError),
}

impl Error {
    fn panic(component: &'static str, err: JoinError) -> Self {
        Error::Panic(component, err)
    }
}

#[derive(Debug, thiserror::Error, CodedError)]
pub enum BuildError {
    #[error("building worker failed: {0}")]
    Worker(
        #[from]
        #[code]
        worker::WorkerRoleBuildError,
    ),
    #[error("invalid controller endpoint: {0}")]
    #[code(unknown)]
    InvalidControllerEndpoint(#[from] InvalidUri),
}

pub struct Node {
    node_id: NodeId,
    cluster_controller_endpoint: Uri,

    cluster_controller_role: Option<ClusterControllerRole>,
    worker_role: WorkerRole,
}

impl Node {
    pub fn new(
        node_id: impl Into<NodeId>,
        cluster_controller_location: ClusterControllerLocation,
        options: Options,
    ) -> Result<Self, BuildError> {
        let (cluster_controller_role, cluster_controller_endpoint) =
            match cluster_controller_location {
                ClusterControllerLocation::Local => {
                    let cluster_controller = ClusterControllerRole::try_from(options.clone())
                        .expect("should be infallible");
                    let cluster_controller_endpoint =
                        cluster_controller.controller_endpoint().clone();
                    (Some(cluster_controller), cluster_controller_endpoint)
                }
                ClusterControllerLocation::Remote(controller_endpoint) => {
                    (None, controller_endpoint.parse()?)
                }
            };

        let worker_role = WorkerRole::try_from(options)?;

        Ok(Node {
            node_id: node_id.into(),
            cluster_controller_endpoint,
            cluster_controller_role,
            worker_role,
        })
    }

    #[instrument(level = "debug", skip_all, fields(node.id = %self.node_id))]
    pub async fn run(self, shutdown_watch: drain::Watch) -> Result<(), Error> {
        let shutdown_signal = shutdown_watch.signaled();
        tokio::pin!(shutdown_signal);

        let (component_shutdown_signal, component_shutdown_watch) = drain::channel();

        let mut cluster_controller_handle: OptionFuture<_> = self
            .cluster_controller_role
            .map(|cluster_controller| {
                tokio::spawn(cluster_controller.run(component_shutdown_watch.clone()))
            })
            .into();

        tokio::select! {
            _ = &mut shutdown_signal => {
                drop(component_shutdown_watch);
                let _ = tokio::join!(component_shutdown_signal.drain(), &mut cluster_controller_handle);
                return Ok(());
            },
            Some(cluster_controller_result) = &mut cluster_controller_handle => {
                cluster_controller_result.map_err(|err| Error::panic("cluster controller role", err))??;
                panic!("Unexpected termination of cluster controller role.");
            },
            attachment_result = Self::attach_node(self.node_id, self.cluster_controller_endpoint) => {
                attachment_result?
            }
        }

        let mut worker_handle = tokio::spawn(self.worker_role.run(component_shutdown_watch));

        tokio::select! {
            // todo: node should also run the node-ctrl endpoint and forward signal to components
            _ = shutdown_signal => {
                info!("Shutting node down");
                let _ = tokio::join!(component_shutdown_signal.drain(), worker_handle, cluster_controller_handle);
            },
            worker_result = &mut worker_handle => {
                worker_result.map_err(|err| Error::panic("worker role", err))??;
                panic!("Unexpected termination of worker role.");
            },
            Some(cluster_controller_result) = &mut cluster_controller_handle => {
                cluster_controller_result.map_err(|err| Error::panic("cluster controller role", err))??;
                panic!("Unexpected termination of cluster controller role.");
            },
        }

        Ok(())
    }

    async fn attach_node(node_id: NodeId, cluster_controller_endpoint: Uri) -> Result<(), Error> {
        info!("Attach to cluster at '{cluster_controller_endpoint}'");
        let channel = Channel::builder(cluster_controller_endpoint.clone())
            .connect_timeout(Duration::from_secs(5))
            .connect_lazy();
        let cc_client = ClusterControllerClient::new(channel);

        RetryPolicy::exponential(Duration::from_millis(50), 2.0, 10, None)
            .retry_operation(|| async {
                cc_client
                    .clone()
                    .attach_node(AttachmentRequest {
                        node_id: Some(node_id.into()),
                    })
                    .await
            })
            .await
            .map_err(|err| Error::Attachment(cluster_controller_endpoint, err))?;

        Ok(())
    }
}

/// Specifying where the cluster controller runs. Options are:
///
/// * Local: Spawning the controller as part of this process
/// * Remote: The controller runs on a remote host
#[derive(Debug)]
pub enum ClusterControllerLocation {
    Local,
    Remote(String),
}

impl FromStr for ClusterControllerLocation {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let result = if s.to_lowercase() == "local" {
            ClusterControllerLocation::Local
        } else {
            ClusterControllerLocation::Remote(s.to_string())
        };

        Ok(result)
    }
}
