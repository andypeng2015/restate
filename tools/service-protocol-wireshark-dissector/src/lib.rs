// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use bytes::Bytes;
use mlua::{prelude::*, Table, Value};
use restate_service_protocol::{
    codec::ProtobufRawEntryCodec,
    message::{Decoder, MessageType, ProtocolMessage},
};
use restate_types::service_protocol::ServiceProtocolVersion;

#[derive(Debug, thiserror::Error)]
#[error("unexpected lua value received")]
pub struct UnexpectedLuaValue;

macro_rules! set_table_values {
    ($message_table:expr, $($name:expr => $val:expr),* $(,)?) => {
            $($message_table.set($name, $val)?;)*
    };
}

fn decode_packages<'lua>(lua: &'lua Lua, buf_lua: Value<'lua>) -> LuaResult<Table<'lua>> {
    let result_messages = lua.create_table()?;

    // We should store it somewhere, but right now wireshark doesn't support conversations in lua api
    // so we just keep it simple and assume all messages are self contained within the same http data frame
    // https://ask.wireshark.org/question/11650/lua-wireshark-dissector-combine-data-from-2-udp-packets
    let mut dec = Decoder::new(ServiceProtocolVersion::V1, usize::MAX, None);

    // Convert the buffer and push it to the decoder
    let buf = match buf_lua {
        Value::String(s) => Bytes::from(s.as_bytes().to_vec()),
        _ => return Err(LuaError::external(UnexpectedLuaValue)),
    };
    dec.push(buf);

    while let Some((header, message)) = dec.consume_next().map_err(LuaError::external)? {
        let message_table = lua.create_table()?;

        // Pass info
        set_table_values!(message_table,
            "ty" => u16::from(header.message_type()),
            "ty_name" => format_message_type(header.message_type()),
            "len" => header.frame_length(),
            "message" => match message {
                ProtocolMessage::Start(m) => {
                    format!("{:#?}", m)
                }
                ProtocolMessage::Completion(c) => {
                    format!("{:#?}", c)
                }
                ProtocolMessage::Suspension(s) => {
                    format!("{:#?}", s)
                }
                ProtocolMessage::EntryAck(a) => {
                    format!("{:#?}", a)
                }
                ProtocolMessage::End(e) => {
                    format!("{:?}", e)
                }
                ProtocolMessage::Error(e) => {
                    format!("{:?}", e)
                }
                ProtocolMessage::UnparsedEntry(e) => {
                    format!("{:#?}", e.deserialize_entry::<ProtobufRawEntryCodec>().map_err(LuaError::external)?)
                }
            }
        );

        // Optional flags
        if let Some(completed) = header.completed() {
            set_table_values!(message_table, "completed" => completed);
        }
        if let Some(requires_ack) = header.requires_ack() {
            set_table_values!(message_table, "requires_ack" => requires_ack);
        }

        result_messages.push(message_table)?;
    }

    Ok(result_messages)
}

fn format_message_type(msg_type: MessageType) -> String {
    match msg_type {
        mt @ MessageType::CustomEntry(_) => {
            format!("{:?}", mt)
        }
        mt => {
            format!("{:?}({:#06X})", mt, u16::from(mt))
        }
    }
}

#[mlua::lua_module]
fn restate_service_protocol_decoder(lua: &Lua) -> LuaResult<LuaTable> {
    let exports = lua.create_table()?;
    exports.set("decode_packages", lua.create_function(decode_packages)?)?;
    Ok(exports)
}
