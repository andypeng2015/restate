// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use codederror::{Code, CodedError};
use okapi_operation::{
    anyhow::Error,
    okapi,
    okapi::{map, openapi3::Responses},
    Components, ToMediaTypes, ToResponses,
};
use restate_core::ShutdownError;
use restate_types::{
    identifiers::{DeploymentId, SubscriptionId},
    invocation::ServiceType,
};
use schemars::JsonSchema;
use serde::Serialize;

use crate::schema_registry::error::{
    DeploymentError, SchemaError, SchemaRegistryError, ServiceError,
};

/// This error is used by handlers to propagate API errors,
/// and later converted to a response through the IntoResponse implementation
#[derive(Debug, thiserror::Error)]
pub enum MetaApiError {
    #[error("The request field '{0}' is invalid. Reason: {1}")]
    InvalidField(&'static str, String),
    #[error("The requested deployment '{0}' does not exist")]
    DeploymentNotFound(DeploymentId),
    #[error("The requested service '{0}' does not exist")]
    ServiceNotFound(String),
    #[error("The requested handler '{handler_name}' on service '{service_name}' does not exist")]
    HandlerNotFound {
        service_name: String,
        handler_name: String,
    },
    #[error("The requested subscription '{0}' does not exist")]
    SubscriptionNotFound(SubscriptionId),
    #[error("Cannot {0} for service type {1}")]
    UnsupportedOperation(&'static str, ServiceType),
    #[error(transparent)]
    Schema(#[from] SchemaError),
    #[error(transparent)]
    Discovery(#[from] restate_service_protocol::discovery::DiscoveryError),
    #[error("Internal server error: {0}")]
    Internal(String),
}

/// # Error description response
///
/// Error details of the response
#[derive(Debug, Serialize, JsonSchema)]
struct ErrorDescriptionResponse {
    message: String,
    /// # Restate code
    ///
    /// Restate error code describing this error
    restate_code: Option<&'static str>,
}

impl IntoResponse for MetaApiError {
    fn into_response(self) -> Response {
        let status_code = match &self {
            MetaApiError::ServiceNotFound(_)
            | MetaApiError::HandlerNotFound { .. }
            | MetaApiError::DeploymentNotFound(_)
            | MetaApiError::SubscriptionNotFound(_) => StatusCode::NOT_FOUND,
            MetaApiError::InvalidField(_, _) | MetaApiError::UnsupportedOperation(_, _) => {
                StatusCode::BAD_REQUEST
            }
            MetaApiError::Schema(schema_error) => match schema_error {
                SchemaError::NotFound(_) => StatusCode::NOT_FOUND,
                SchemaError::Override(_)
                | SchemaError::Service(ServiceError::DifferentType { .. })
                | SchemaError::Service(ServiceError::RemovedHandlers { .. })
                | SchemaError::Deployment(DeploymentError::IncorrectId { .. }) => {
                    StatusCode::CONFLICT
                }
                SchemaError::Service(_) => StatusCode::BAD_REQUEST,
                _ => StatusCode::BAD_REQUEST,
            },
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = Json(match &self {
            MetaApiError::Schema(m) => ErrorDescriptionResponse {
                message: m.decorate().to_string(),
                restate_code: m.code().map(Code::code),
            },
            MetaApiError::Discovery(err) => ErrorDescriptionResponse {
                message: err.decorate().to_string(),
                restate_code: err.code().map(Code::code),
            },
            e => ErrorDescriptionResponse {
                message: e.to_string(),
                restate_code: None,
            },
        });

        (status_code, body).into_response()
    }
}

impl ToResponses for MetaApiError {
    fn generate(components: &mut Components) -> Result<Responses, Error> {
        let error_media_type =
            <Json<ErrorDescriptionResponse> as ToMediaTypes>::generate(components)?;
        Ok(Responses {
            responses: map! {
                "400".into() => okapi::openapi3::RefOr::Object(
                    okapi::openapi3::Response { content: error_media_type.clone(), ..Default::default() }
                ),
                "403".into() => okapi::openapi3::RefOr::Object(
                    okapi::openapi3::Response { content: error_media_type.clone(), ..Default::default() }
                ),
                "404".into() => okapi::openapi3::RefOr::Object(
                    okapi::openapi3::Response { content: error_media_type.clone(), ..Default::default() }
                ),
                "409".into() => okapi::openapi3::RefOr::Object(
                    okapi::openapi3::Response { content: error_media_type.clone(), ..Default::default() }
                ),
                "500".into() => okapi::openapi3::RefOr::Object(
                    okapi::openapi3::Response { content: error_media_type.clone(), ..Default::default() }
                ),
                "503".into() => okapi::openapi3::RefOr::Object(
                    okapi::openapi3::Response { content: error_media_type, ..Default::default() }
                )
            },
            ..Default::default()
        })
    }
}

impl From<SchemaRegistryError> for MetaApiError {
    fn from(value: SchemaRegistryError) -> Self {
        match value {
            SchemaRegistryError::Schema(err) => MetaApiError::Schema(err),
            SchemaRegistryError::Internal(msg) => MetaApiError::Internal(msg),
            SchemaRegistryError::Shutdown(err) => MetaApiError::Internal(err.to_string()),
            SchemaRegistryError::Discovery(err) => MetaApiError::Discovery(err),
        }
    }
}

impl From<ShutdownError> for MetaApiError {
    fn from(value: ShutdownError) -> Self {
        MetaApiError::Internal(value.to_string())
    }
}
