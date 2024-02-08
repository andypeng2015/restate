// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use figment::providers::{Env, Format, Serialized, Yaml};
use figment::Figment;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_with::serde_as;
use std::ops::Div;
use std::path::Path;
use std::time::Duration;

pub use crate::rt::{
    Options as TokioOptions, OptionsBuilder as TokioOptionsBuilder,
    OptionsBuilderError as TokioOptionsBuilderError,
};
pub use restate_admin::Options as AdminOptions;
pub use restate_bifrost::Options as BifrostOptions;
pub use restate_meta::{
    Options as MetaOptions, OptionsBuilder as MetaOptionsBuilder,
    OptionsBuilderError as MetaOptionsBuilderError,
};
use restate_node::ClusterControllerLocation;
pub use restate_node_ctrl::Options as NodeCtrlOptions;
use restate_storage_rocksdb::TableKind;
pub use restate_tracing_instrumentation::{
    LogOptions, LogOptionsBuilder, LogOptionsBuilderError, Options as ObservabilityOptions,
    OptionsBuilder as ObservabilityOptionsBuilder,
    OptionsBuilderError as ObservabilityOptionsBuilderError, TracingOptions, TracingOptionsBuilder,
    TracingOptionsBuilderError,
};
use restate_types::PlainNodeId;

/// # Restate configuration file
///
/// Configuration for the Restate single binary deployment.
///
/// You can specify the configuration file to use through the `--config-file <PATH>` argument or
/// with `RESTATE_CONFIG=<PATH>` environment variable.
///
/// Each configuration entry can be overridden using environment variables,
/// prefixing them with `RESTATE_` and separating nested structs with `__` (double underscore).
/// For example, to configure `meta.rest_address`, the corresponding environment variable is `RESTATE_META__REST_ADDRESS`.
#[serde_as]
#[derive(Debug, Serialize, Deserialize, derive_builder::Builder)]
#[cfg_attr(feature = "options_schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "options_schema", schemars(default))]
#[builder(default)]
pub struct Configuration {
    pub node_id: PlainNodeId,

    /// # Shutdown grace timeout
    ///
    /// This timeout is used when shutting down the various Restate components to drain all the internal queues.
    ///
    /// Can be configured using the [`humantime`](https://docs.rs/humantime/latest/humantime/fn.parse_duration.html) format.
    #[serde_as(as = "serde_with::DisplayFromStr")]
    #[cfg_attr(feature = "options_schema", schemars(with = "String"))]
    pub shutdown_grace_period: humantime::Duration,
    pub observability: restate_tracing_instrumentation::Options,
    pub tokio_runtime: crate::rt::Options,

    #[serde(flatten)]
    pub node: restate_node::Options,

    /// Configures the cluster controller endpoint. Options are:
    ///
    /// * "Local": Cluster controller will be run locally
    /// * [Address]: Specifying the remote address of the cluster controller
    #[cfg_attr(feature = "options_schema", schemars(with = "String"))]
    pub cluster_controller_endpoint: ClusterControllerEndpoint,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            node_id: PlainNodeId::from(1),
            shutdown_grace_period: Duration::from_secs(60).into(),
            observability: Default::default(),
            tokio_runtime: Default::default(),
            cluster_controller_endpoint: ClusterControllerEndpoint::Local,
            node: Default::default(),
        }
    }
}

/// Global memory options. These may only be set by environment variable
#[derive(Serialize, Deserialize)]
pub struct MemoryOptions {
    /// Global memory limit, configured with `MEMORY_LIMIT` environment variable only.
    /// This controls rocksdb cache size defaults
    limit: usize,
}

impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            limit: 3 * (1 << 30), // 3 GiB
        }
    }
}

impl MemoryOptions {
    fn apply_defaults(self, figment: Figment) -> Figment {
        let table_count = TableKind::all().count();

        let write_buffer_size = self
            .limit
            // target 50% usage
            .div(2)
            // split across all the tables
            .div(table_count)
            // where there's at most 3 column families per table
            .div(3)
            // with 8 MiB min and 256 MiB max
            .clamp(8 * (1 << 20), 256 * (1 << 20));

        let cache_size = self.limit.div(3); // target 33% usage, no min or max

        figment
            .merge(Figment::from(Serialized::default(
                "worker.storage_rocksdb.write_buffer_size",
                write_buffer_size,
            )))
            .merge(Figment::from(Serialized::default(
                "worker.storage_rocksdb.cache_size",
                cache_size,
            )))
    }
}

#[derive(Debug, thiserror::Error, codederror::CodedError)]
#[code(restate_errors::RT0002)]
#[error("configuration error: {0}")]
pub struct Error(#[from] figment::Error);

impl Configuration {
    /// Load [`Configuration`] from yaml with overwrites from environment variables.
    pub fn load<P: AsRef<Path>>(config_file: P) -> Result<Self, Error> {
        Self::load_with_default(Configuration::default(), Some(config_file.as_ref()))
    }

    /// Load [`Configuration`] from an optional yaml with overwrites from environment
    /// variables based on a default configuration.
    pub fn load_with_default(
        default_configuration: Configuration,
        config_file: Option<&Path>,
    ) -> Result<Self, Error> {
        let figment = Figment::from(Serialized::defaults(default_configuration));

        println!("{:#?}", figment);

        // get memory options separately, and use them to set certain defaults
        let memory: MemoryOptions = Figment::from(Serialized::defaults(MemoryOptions::default()))
            .merge(Env::prefixed("MEMORY_").split("__"))
            .extract()?;
        let figment = memory.apply_defaults(figment);

        let figment = if let Some(config_file) = config_file {
            figment.merge(Yaml::file(config_file))
        } else {
            figment
        };

        let figment = figment
            .merge(Env::prefixed("RESTATE_").split("__"))
            // Override tracing.log with RUST_LOG, if present
            .merge(
                Env::raw()
                    .only(&["RUST_LOG"])
                    .map(|_| "observability.log.filter".into()),
            )
            .merge(
                Env::raw()
                    .only(&["HTTP_PROXY"])
                    .map(|_| "worker.invoker.service_client.http.proxy_uri".into()),
            )
            .merge(
                Env::raw()
                    .only(&["HTTP_PROXY"])
                    .map(|_| "meta.service_client.http.proxy_uri".into()),
            )
            .merge(
                Env::raw()
                    .only(&["AWS_EXTERNAL_ID"])
                    .map(|_| "meta.service_client.lambda.assume_role_external_id".into()),
            )
            .merge(
                Env::raw()
                    .only(&["AWS_EXTERNAL_ID"])
                    .map(|_| "worker.invoker.service_client.lambda.assume_role_external_id".into()),
            )
            .extract()?;

        Ok(figment)
    }
}

#[derive(Debug, Clone)]
pub enum ClusterControllerEndpoint {
    Local,
    Remote(String),
}

impl Serialize for ClusterControllerEndpoint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            ClusterControllerEndpoint::Local => serializer.serialize_str("local"),
            ClusterControllerEndpoint::Remote(address) => serializer.serialize_str(address),
        }
    }
}

impl<'de> Deserialize<'de> for ClusterControllerEndpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;

        let result = if value.to_lowercase() == "local" {
            ClusterControllerEndpoint::Local
        } else {
            ClusterControllerEndpoint::Remote(value)
        };

        Ok(result)
    }
}

impl From<ClusterControllerEndpoint> for ClusterControllerLocation {
    fn from(value: ClusterControllerEndpoint) -> Self {
        match value {
            ClusterControllerEndpoint::Local => ClusterControllerLocation::Local,
            ClusterControllerEndpoint::Remote(address) => {
                ClusterControllerLocation::Remote(address)
            }
        }
    }
}
