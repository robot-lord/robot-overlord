//! Wire format: newline-delimited JSON frames over the Unix socket.
//! Request:  {"id": u64, "method": "...", "params": {...}}
//! Response: {"id": u64, "result": ...} | {"id": u64, "error": {"code", "message"}}

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl Response {
    pub fn ok(id: u64, result: Value) -> Self {
        Response {
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: u64, code: i32, message: impl Into<String>) -> Self {
        Response {
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

pub const ERR_METHOD_NOT_FOUND: i32 = -32601;
pub const ERR_INTERNAL: i32 = -32603;
pub const ERR_BAD_REQUEST: i32 = -32600;
