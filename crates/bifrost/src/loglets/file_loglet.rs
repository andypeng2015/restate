// Copyright (c) 2024 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::loglet::{Loglet, LogletBase, LogletOffset, LogletProvider};
use crate::metadata::LogletParams;
use crate::{AppendAttributes, DataRecord, Error, Options};

pub fn default_config() -> serde_json::Value {
    json!( {"path": "target/logs/"})
}

pub struct FileLogletProvider {}

impl FileLogletProvider {
    pub fn new(_options: &Options) -> Arc<Self> {
        Arc::new(Self {})
    }
}

#[async_trait]
impl LogletProvider for FileLogletProvider {
    async fn get_loglet(
        &self,
        _config: &LogletParams,
    ) -> Result<std::sync::Arc<dyn Loglet<Offset = LogletOffset>>, Error> {
        todo!()
    }
}

pub struct FileLoglet {
    _params: LogletParams,
}

#[async_trait]
impl LogletBase for FileLoglet {
    type Offset = LogletOffset;
    async fn append(
        &self,
        _record: DataRecord,
        _attributes: AppendAttributes,
    ) -> Result<LogletOffset, Error> {
        todo!()
    }
}
