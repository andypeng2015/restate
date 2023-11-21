// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Build information

/// The version of restate CLI.
pub const RESTATE_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const RESTATE_CLI_VERSION_MAJOR: &str = env!("CARGO_PKG_VERSION_MAJOR");
pub const RESTATE_CLI_VERSION_MINOR: &str = env!("CARGO_PKG_VERSION_MINOR");
pub const RESTATE_CLI_VERSION_PATCH: &str = env!("CARGO_PKG_VERSION_PATCH");
/// Pre-release version of restate.
pub const RESTATE_CLI_VERSION_PRE: &str = env!("CARGO_PKG_VERSION_PRE");

pub const RESTATE_CLI_BUILD_DATE: &str = env!("VERGEN_BUILD_DATE");
pub const RESTATE_CLI_BUILD_TIME: &str = env!("VERGEN_BUILD_TIMESTAMP");
pub const RESTATE_CLI_COMMIT_SHA: &str = env!("VERGEN_GIT_SHA");
pub const RESTATE_CLI_COMMIT_DATE: &str = env!("VERGEN_GIT_COMMIT_DATE");
pub const RESTATE_CLI_BRANCH: &str = env!("VERGEN_GIT_BRANCH");
// /// The target triple.
pub const RESTATE_CLI_TARGET_TRIPLE: &str = env!("VERGEN_CARGO_TARGET_TRIPLE");
// /// The profile used in build.
pub const RESTATE_CLI_DEBUG: &str = env!("VERGEN_CARGO_DEBUG");
