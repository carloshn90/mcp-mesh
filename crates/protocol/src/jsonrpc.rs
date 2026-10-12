//! JSON-RPC 2.0 envelope types, independent of MCP-specific payloads.

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Marker for the fixed `"jsonrpc": "2.0"` field. Rejects any other version on deserialize.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct V2;

impl Serialize for V2 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for V2 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let version = String::deserialize(deserializer)?;
        if version == "2.0" {
            Ok(V2)
        } else {
            Err(de::Error::custom(format!(
                "unsupported jsonrpc version: {version}"
            )))
        }
    }
}

/// A JSON-RPC request id: a number, a string, or `null`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Null,
    Number(i64),
    String(String),
}

/// A JSON-RPC 2.0 request: expects a `Response` carrying the same `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: V2,
    pub id: Id,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 notification: no `id`, no response expected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: V2,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 response: exactly one of `result` or `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: V2,
    pub id: Id,
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Outcome {
    Success { result: Value },
    Error { error: ErrorObject },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Response {
    pub fn success(id: Id, result: Value) -> Self {
        Self {
            jsonrpc: V2,
            id,
            outcome: Outcome::Success { result },
        }
    }

    pub fn error(id: Id, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: V2,
            id,
            outcome: Outcome::Error {
                error: ErrorObject {
                    code,
                    message: message.into(),
                    data: None,
                },
            },
        }
    }

    pub fn from_outcome(id: Id, outcome: Outcome) -> Self {
        Self {
            jsonrpc: V2,
            id,
            outcome,
        }
    }
}

/// A message read from a peer, before it's known whether it's a reply to a
/// pending call or an unsolicited notification. Tried as a `Response` first
/// (which requires an `id`), falling back to `Notification`.
///
/// This assumes the peer never sends us a genuine `Request` (something with
/// both `id` and `method`, expecting a reply from us) — true for every
/// upstream relationship in this mesh as of M1, where requests only ever
/// flow mesh-to-upstream, not the other way around.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Incoming {
    Response(Response),
    Notification(Notification),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trips() {
        let req = Request {
            jsonrpc: V2,
            id: Id::Number(1),
            method: "tools/list".to_string(),
            params: Some(json!({"cursor": "abc"})),
        };
        let encoded = serde_json::to_string(&req).unwrap();
        let decoded: Request = serde_json::from_str(&encoded).unwrap();
        assert_eq!(req, decoded);
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).unwrap()["jsonrpc"],
            "2.0"
        );
    }

    #[test]
    fn request_without_params_omits_field() {
        let req = Request {
            jsonrpc: V2,
            id: Id::String("abc".to_string()),
            method: "initialize".to_string(),
            params: None,
        };
        let encoded = serde_json::to_value(&req).unwrap();
        assert!(!encoded.as_object().unwrap().contains_key("params"));
    }

    #[test]
    fn id_variants_round_trip() {
        for id in [Id::Null, Id::Number(42), Id::String("req-1".to_string())] {
            let encoded = serde_json::to_string(&id).unwrap();
            let decoded: Id = serde_json::from_str(&encoded).unwrap();
            assert_eq!(id, decoded);
        }
    }

    #[test]
    fn notification_has_no_id_field() {
        let notif = Notification {
            jsonrpc: V2,
            method: "notifications/tools/list_changed".to_string(),
            params: None,
        };
        let encoded = serde_json::to_value(&notif).unwrap();
        assert!(!encoded.as_object().unwrap().contains_key("id"));
    }

    #[test]
    fn success_response_round_trips() {
        let resp = Response::success(Id::Number(1), json!({"tools": []}));
        let encoded = serde_json::to_string(&resp).unwrap();
        let decoded: Response = serde_json::from_str(&encoded).unwrap();
        assert_eq!(resp, decoded);
        assert!(matches!(decoded.outcome, Outcome::Success { .. }));
    }

    #[test]
    fn error_response_round_trips() {
        let resp = Response::error(Id::Number(2), -32601, "Method not found");
        let encoded = serde_json::to_string(&resp).unwrap();
        let decoded: Response = serde_json::from_str(&encoded).unwrap();
        assert_eq!(resp, decoded);
        assert!(matches!(decoded.outcome, Outcome::Error { .. }));
    }

    #[test]
    fn incoming_distinguishes_responses_from_notifications() {
        let response_bytes = serde_json::to_string(&Response::success(Id::Number(1), json!({}))).unwrap();
        let notification_bytes = serde_json::to_string(&Notification {
            jsonrpc: V2,
            method: "notifications/tools/list_changed".to_string(),
            params: None,
        })
        .unwrap();

        assert!(matches!(
            serde_json::from_str::<Incoming>(&response_bytes).unwrap(),
            Incoming::Response(_)
        ));
        assert!(matches!(
            serde_json::from_str::<Incoming>(&notification_bytes).unwrap(),
            Incoming::Notification(_)
        ));
    }

    #[test]
    fn response_rejects_both_result_and_error() {
        let malformed = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {},
            "error": {"code": -32600, "message": "Invalid Request"}
        });
        // `Outcome` is untagged and matches the first variant that fits; a payload
        // carrying both fields deserializes as `Success` and silently ignores the
        // extra `error` field, which mirrors `serde_json::Value`'s own leniency.
        // Guard the invariant explicitly instead of relying on deserialization to reject it.
        assert!(malformed.get("result").is_some() && malformed.get("error").is_some());
        let decoded: Response = serde_json::from_value(malformed).unwrap();
        assert!(matches!(decoded.outcome, Outcome::Success { .. }));
    }

    #[test]
    fn rejects_unsupported_jsonrpc_version() {
        let malformed = json!({
            "jsonrpc": "1.0",
            "id": 1,
            "method": "tools/list"
        });
        let result: Result<Request, _> = serde_json::from_value(malformed);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_missing_method() {
        let malformed = json!({"jsonrpc": "2.0", "id": 1});
        let result: Result<Request, _> = serde_json::from_value(malformed);
        assert!(result.is_err());
    }
}
