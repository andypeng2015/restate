// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

fn main() -> std::io::Result<()> {
    prost_build::Config::new()
        .bytes(["."])
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_protos(
            &[
                "service-protocol/dev/restate/service/protocol.proto",
                "service-protocol/dev/restate/service/discovery.proto",
            ],
            &["service-protocol"],
        )
}
