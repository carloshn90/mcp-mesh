//! Typed payloads for the MCP methods the mesh cares about.
//!
//! Only the methods the mesh actively interprets (`initialize`, `tools/list`,
//! `tools/call`, `notifications/tools/list_changed`) get typed structs;
//! everything else is passed through as raw `serde_json::Value`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const METHOD_INITIALIZE: &str = "initialize";
pub const METHOD_TOOLS_LIST: &str = "tools/list";
pub const METHOD_TOOLS_CALL: &str = "tools/call";
pub const METHOD_TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Implementation {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: Value,
    pub client_info: Implementation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: Value,
    pub server_info: Implementation,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolsListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsListResult {
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolsCallParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCallResult {
    pub content: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn initialize_params_use_camel_case_on_wire() {
        let params = InitializeParams {
            protocol_version: "2025-06-18".to_string(),
            capabilities: json!({}),
            client_info: Implementation {
                name: "mcp-mesh".to_string(),
                version: "0.1.0".to_string(),
            },
        };
        let encoded = serde_json::to_value(&params).unwrap();
        assert_eq!(encoded["protocolVersion"], "2025-06-18");
        assert_eq!(encoded["clientInfo"]["name"], "mcp-mesh");

        let decoded: InitializeParams = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, params);
    }

    #[test]
    fn tool_input_schema_uses_camel_case_on_wire() {
        let tool = Tool {
            name: "pg__query".to_string(),
            description: Some("Run a SQL query".to_string()),
            input_schema: json!({"type": "object"}),
        };
        let encoded = serde_json::to_value(&tool).unwrap();
        assert!(encoded.get("inputSchema").is_some());
        assert!(encoded.get("input_schema").is_none());
    }

    #[test]
    fn tools_list_result_round_trips_with_and_without_cursor() {
        let with_cursor = ToolsListResult {
            tools: vec![],
            next_cursor: Some("page-2".to_string()),
        };
        let encoded = serde_json::to_string(&with_cursor).unwrap();
        let decoded: ToolsListResult = serde_json::from_str(&encoded).unwrap();
        assert_eq!(with_cursor, decoded);

        let without_cursor = ToolsListResult {
            tools: vec![],
            next_cursor: None,
        };
        let encoded = serde_json::to_value(&without_cursor).unwrap();
        assert!(!encoded.as_object().unwrap().contains_key("nextCursor"));
    }

    #[test]
    fn tools_call_params_round_trip_without_arguments() {
        let params = ToolsCallParams {
            name: "fs__read".to_string(),
            arguments: None,
        };
        let encoded = serde_json::to_value(&params).unwrap();
        assert!(!encoded.as_object().unwrap().contains_key("arguments"));
        let decoded: ToolsCallParams = serde_json::from_value(encoded).unwrap();
        assert_eq!(params, decoded);
    }

    #[test]
    fn tools_call_result_round_trips_with_is_error() {
        let result = ToolsCallResult {
            content: vec![json!({"type": "text", "text": "boom"})],
            is_error: Some(true),
        };
        let encoded = serde_json::to_string(&result).unwrap();
        let decoded: ToolsCallResult = serde_json::from_str(&encoded).unwrap();
        assert_eq!(result, decoded);
    }

    #[test]
    fn list_changed_is_a_bare_method_name() {
        assert_eq!(METHOD_TOOLS_LIST_CHANGED, "notifications/tools/list_changed");
    }
}
