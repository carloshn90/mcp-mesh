//! JSON-RPC 2.0 and MCP wire types shared by the mesh's core and CLI crates.

pub mod codec;
pub mod jsonrpc;
pub mod mcp;

pub use codec::{MessageReader, MessageWriter};
pub use jsonrpc::{ErrorObject, Id, Incoming, Notification, Outcome, Request, Response, V2};
