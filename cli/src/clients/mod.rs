// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

mod admin_client;
mod admin_interface;
#[cfg(feature = "cloud")]
pub mod cloud;
pub mod datafusion_helpers;
mod datafusion_http_client;
mod errors;

pub use self::{
    admin_client::{
        AdminClient, Error as MetasClientError, MAX_ADMIN_API_VERSION, MIN_ADMIN_API_VERSION,
    },
    admin_interface::AdminClientInterface,
    datafusion_http_client::DataFusionHttpClient,
};
