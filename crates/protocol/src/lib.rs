//! JSON-RPC 2.0 and MCP wire types shared by the mesh's core and CLI crates.

pub mod jsonrpc;
pub mod mcp;

pub use jsonrpc::{ErrorObject, Id, Notification, Outcome, Request, Response, V2};
