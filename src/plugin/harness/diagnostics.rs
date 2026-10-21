//! Log lifecycle metadata, never transcript items or native error messages.
use crate::core::events::{emit, Level};
use serde_json::{json, Value};

pub(super) fn record(message: &Value) {
    if let Some((level, name, fields)) = event(message) {
        emit(level, name, fields);
    }
}
fn event(message: &Value) -> Option<(Level, &'static str, Value)> {
    let (level, name) = match message["method"].as_str()? {
        "turn/started" => (Level::Info, "harness.turn.started"),
        "turn/completed" if message["params"]["turn"]["status"] == "failed" => {
            (Level::Warn, "harness.turn.failed")
        }
        "turn/completed" => (Level::Info, "harness.turn.completed"),
        "error" => (Level::Warn, "harness.stream.error"),
        "harness/disconnected" => (Level::Warn, "harness.disconnected"),
        _ => return None,
    };
    Some((level, name, fields(&message["params"])))
}
fn fields(p: &Value) -> Value {
    let error = p
        .get("error")
        .filter(|v| !v.is_null())
        .unwrap_or(&p["turn"]["error"]);
    json!({"sessionId":label(&p["threadId"]),
        "turnId":label(p.get("turnId").unwrap_or(&p["turn"]["id"])),
        "status":label(&p["turn"]["status"]), "willRetry":p["willRetry"].as_bool(),
        "errorInfo":error_info(error), "rpcCode":error["code"].as_i64()})
}
fn label(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| {
        s.len() <= 128
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
    })
}
fn error_info(error: &Value) -> Value {
    let info = &error["codexErrorInfo"];
    if info.is_string() {
        return json!({"kind":label(info)});
    }
    let Some((kind, detail)) = info.as_object().and_then(|o| o.iter().next()) else {
        return Value::Null;
    };
    json!({"kind":label(&json!(kind)), "httpStatusCode":detail["httpStatusCode"].as_u64()})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_turn_keeps_identifiers_and_transport_category_without_native_details() {
        let message = json!({"method":"turn/completed","params":{"threadId":"session-1","turn":{"id":"turn-1","status":"failed","items":[{"text":"private-message"}],
            "error":{"message":"private-error","additionalDetails":"private-details","codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":403,"body":"private-response"}}}}}});
        let (level, name, fields) = event(&message).unwrap();
        assert_eq!(level, Level::Warn);
        assert_eq!(name, "harness.turn.failed");
        assert_eq!(fields["sessionId"], "session-1");
        assert_eq!(fields["errorInfo"]["kind"], "httpConnectionFailed");
        assert_eq!(fields["errorInfo"]["httpStatusCode"], 403);
        assert!(!fields.to_string().contains("private-"));
    }
    #[test]
    fn normal_reads_and_content_deltas_remain_silent() {
        for method in [
            "item/agentMessage/delta",
            "item/completed",
            "thread/status/changed",
        ] {
            assert!(event(&json!({"method":method})).is_none());
        }
        let (_, name, fields) = event(&json!({"method":"error","params":{"willRetry":true,"error":{"message":"private-error","codexErrorInfo":"responseStreamDisconnected"}}})).unwrap();
        assert_eq!(name, "harness.stream.error");
        assert_eq!(fields["willRetry"], true);
        assert_eq!(fields["errorInfo"]["kind"], "responseStreamDisconnected");
    }
}
