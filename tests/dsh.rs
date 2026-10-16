#[path = "common/dsh.rs"]
mod fixture;
use pi_agent::{config::Config, harness::Harnesses};
use serde_json::{json, Value};

fn service(root: &std::path::Path, url: &str) -> Harnesses {
    service_options(root, url, "")
}
fn service_options(root: &std::path::Path, url: &str, options: &str) -> Harnesses {
    // The fixture login accepts a fixed token; production configuration uses an environment reference.
    let command = root.join("dsh-fixture");
    std::fs::write(
        &command,
        format!("#!/bin/sh\nprintf 'dsh web: {url}?token=fixture\\n'\nexec sleep 300\n"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config: Config = toml::from_str(&format!(
        r#"
listen="127.0.0.1:0"
token="test-dsh-token-0123456789"
execution=false
[[harnesses]]
id="dsh"
kind="dsh"
command={command:?}
home={root:?}
[[harnesses.workspaces]]
id="project"
path={root:?}
[harnesses.dsh]
{options}
"#
    ))
    .unwrap();
    Harnesses::new(
        pi_agent::harness::config::validate(&config).unwrap(),
        "epoch".into(),
    )
    .unwrap()
}
async fn mutate(service: &Harnesses, name: &str, request: &str, args: Value) -> Value {
    let mut value = json!({"profileId":"dsh","epoch":"epoch","requestId":request});
    for (key, item) in args.as_object().unwrap() {
        value[key] = item.clone();
    }
    service.call(name, value).await.unwrap()
}
async fn events(service: &Harnesses) -> Value {
    service
        .call("harness_events", json!({"profileId":"dsh"}))
        .await
        .unwrap()
}
#[tokio::test]
async fn creates_resumes_streams_and_answers_approvals_using_native_receipts() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    let listed = service
        .call("harness_sessions", json!({"profileId":"dsh"}))
        .await
        .unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["id"], "existing");
    assert!(service
        .call(
            "harness_session_info",
            json!({"profileId":"dsh","sessionId":"private"})
        )
        .await
        .is_err());
    let created = mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = created["result"]["session"]["id"].as_str().unwrap();
    assert!(
        created["result"]["session"].get("updatedAt").is_some(),
        "Session DTO requires an explicit nullable updatedAt for independent clients"
    );
    assert_eq!(
        mutate(
            &service,
            "harness_create",
            "create",
            json!({"workspaceId":"project"})
        )
        .await,
        created
    );
    let resumed = mutate(
        &service,
        "harness_resume",
        "resume",
        json!({"sessionId":"existing"}),
    )
    .await;
    assert_eq!(resumed["result"]["session"]["owned"], true);
    let turn = mutate(
        &service,
        "harness_turn",
        "turn-1",
        json!({"sessionId":id,"prompt":"hello"}),
    )
    .await;
    assert_eq!(turn["result"]["turnId"], "turn-1");
    host.emit(
        id,
        json!({"type":"event","event":{"seq":0,"type":"turn/start","data":{"turn":1}}}),
    );
    host.emit(id,json!({"type":"assistant-stream","frame":{"type":"start","attemptId":"attempt-1","revision":1}}));
    host.emit(id,json!({"type":"assistant-stream","frame":{"type":"chunk","revision":2,"index":0,"chunk":{"type":"text-delta","index":0,"text":"partial"}}}));
    host.event(json!({"type":"waterfall","event":"approval/request","eventId":"approval-1","agentId":id,"request":{"toolName":"shell","displayReason":"test"}}));
    for _ in 0..40 {
        if !events(&service).await["requests"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let page = events(&service).await;
    assert!(page["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["message"]["params"]["delta"] == "partial"));
    assert_eq!(page["requests"][0]["id"], "approval-1");
    mutate(
        &service,
        "harness_respond",
        "approve",
        json!({"nativeRequestId":"approval-1","response":{"decision":"accept"}}),
    )
    .await;
    assert!(events(&service).await["requests"]
        .as_array()
        .unwrap()
        .is_empty());
    mutate(
        &service,
        "harness_interrupt",
        "stop",
        json!({"sessionId":id,"turnId":"turn-1"}),
    )
    .await;
    host.emit(id,json!({"type":"event","event":{"seq":1,"type":"turn/end","data":{"turn":1,"reason":{"kind":"cancelled"}}}}));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let history = service
        .call(
            "harness_session_read",
            json!({"profileId":"dsh","sessionId":"existing"}),
        )
        .await
        .unwrap();
    assert_eq!(
        history["turns"][0]["items"][0]["content"][0]["text"],
        "older"
    );
    assert_eq!(
        host.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c["method"] == "session/create")
            .count(),
        1
    );
    service.close().await;
}
#[tokio::test]
async fn independent_admissions_and_streams_do_not_wait_for_another_session() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    let a = mutate(
        &service,
        "harness_create",
        "a",
        json!({"workspaceId":"project"}),
    )
    .await;
    let b = mutate(
        &service,
        "harness_create",
        "b",
        json!({"workspaceId":"project"}),
    )
    .await;
    let a = a["result"]["session"]["id"].as_str().unwrap();
    let b = b["result"]["session"]["id"].as_str().unwrap();
    let slow = mutate(
        &service,
        "harness_turn",
        "slow",
        json!({"sessionId":a,"prompt":"slow-admission"}),
    );
    let fast = async {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        tokio::time::timeout(
            std::time::Duration::from_millis(300),
            mutate(
                &service,
                "harness_turn",
                "fast",
                json!({"sessionId":b,"prompt":"fast"}),
            ),
        )
        .await
        .unwrap()
    };
    let (_, result) = tokio::join!(slow, fast);
    assert_eq!(result["outcome"], "committed");
    service.close().await;
}
#[test]
fn rejects_external_servers_claiming_project_sandbox_support() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let config: Config = toml::from_str(&format!(
        r#"
listen="127.0.0.1:0"
token="test-dsh-token-0123456789"
[[harnesses]]
id="dsh"
kind="dsh"
home={root:?}
projects=true
[harnesses.dsh]
url="http://127.0.0.1:3080/"
"#
    ))
    .unwrap();
    assert!(pi_agent::harness::config::validate(&config)
        .err()
        .unwrap()
        .contains("projects = false"));
}

#[tokio::test]
async fn uncertain_prompt_receipts_are_never_submitted_twice() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service_options(root.path(), &host.url, "request_timeout_ms=100");
    let created = mutate(
        &service,
        "harness_create",
        "new",
        json!({"workspaceId":"project"}),
    )
    .await;
    let id = &created["result"]["session"]["id"];
    let args = json!({"sessionId":id,"prompt":"slow-admission"});
    let receipt = mutate(&service, "harness_turn", "uncertain", args.clone()).await;
    assert_eq!(receipt["outcome"], "unknown");
    assert_eq!(
        mutate(&service, "harness_turn", "uncertain", args).await,
        receipt
    );
    assert_eq!(
        host.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["method"] == "session/prompt")
            .count(),
        1
    );
    service.close().await;
}
#[tokio::test]
async fn native_internal_errors_keep_the_operation_unknown_and_the_original_prompt_private() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    let created = mutate(
        &service,
        "harness_create",
        "new",
        json!({"workspaceId":"project"}),
    )
    .await;
    host.state.errors.lock().unwrap().insert(
        "session/prompt".into(),
        json!({"code":"gateway/internal","message":"private upstream body"}),
    );
    let receipt = mutate(
        &service,
        "harness_turn",
        "failure",
        json!({"sessionId":created["result"]["session"]["id"],"prompt":"secret prompt"}),
    )
    .await;
    assert_eq!(receipt["outcome"], "unknown");
    assert!(!receipt.to_string().contains("private"));
    assert!(!receipt.to_string().contains("secret"));
    service.close().await;
}
#[tokio::test]
async fn session_capacity_is_reserved_before_native_creation() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service_options(root.path(), &host.url, "max_sessions=1");
    assert_eq!(
        mutate(
            &service,
            "harness_create",
            "one",
            json!({"workspaceId":"project"})
        )
        .await["outcome"],
        "committed"
    );
    let second = mutate(
        &service,
        "harness_create",
        "two",
        json!({"workspaceId":"project"}),
    )
    .await;
    assert_eq!(second["outcome"], "not-committed");
    assert_eq!(second["code"], "DSH_SESSION_LIMIT");
    assert_eq!(
        host.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["method"] == "session/create")
            .count(),
        1
    );
    service.close().await;
}
#[tokio::test]
async fn history_pages_keep_the_opening_watermark_when_new_messages_arrive() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    host.state.records.lock().unwrap().insert("existing".into(),(0..151).map(|seq|json!({"type":"event","event":{
        "seq":seq,"type":"user/message","data":{"content":[{"type":"text","text":format!("message {seq}")}]}}})).collect());
    let service = service(root.path(), &host.url);
    let first = service
        .call(
            "harness_session_read",
            json!({"profileId":"dsh","sessionId":"existing"}),
        )
        .await
        .unwrap();
    assert_eq!(first["turns"][0]["items"].as_array().unwrap().len(), 80);
    let older = service
        .call(
            "harness_session_read",
            json!({"profileId":"dsh","sessionId":"existing","cursor":first["nextCursor"]}),
        )
        .await
        .unwrap();
    assert_eq!(older["turns"][0]["items"].as_array().unwrap().len(), 71);
    assert!(older["nextCursor"].is_null());
    assert!(host
        .state
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c["method"] == "session/page"
            && c["payload"]["args"]["request"]["throughSeq"] == 150));
    service.close().await;
}

#[tokio::test]
async fn thirty_two_sessions_admit_work_and_deliver_deltas_independently() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture::Host::new(root.path()).await;
    let service = service(root.path(), &host.url);
    let created = futures_util::future::join_all((0..32).map(|i| {
        let service = &service;
        async move {
            mutate(
                service,
                "harness_create",
                &format!("create-{i}"),
                json!({"workspaceId":"project"}),
            )
            .await
        }
    }))
    .await;
    let ids = created
        .iter()
        .map(|result| {
            result["result"]["session"]["id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    let started = futures_util::future::join_all(ids.iter().enumerate().map(|(i, id)| {
        let service = &service;
        async move {
            mutate(
                service,
                "harness_turn",
                &format!("turn-{i}"),
                json!({"sessionId":id,"prompt":"parallel"}),
            )
            .await
        }
    }))
    .await;
    assert!(started
        .iter()
        .all(|result| result["outcome"] == "committed"));
    for (i, id) in ids.iter().enumerate() {
        host.emit(
            id,
            json!({"type":"event","event":{"seq":0,"type":"turn/start","data":{"turn":1}}}),
        );
        host.emit(id,json!({"type":"assistant-stream","frame":{"type":"start","attemptId":format!("attempt-{i}"),"revision":1}}));
        host.emit(id,json!({"type":"assistant-stream","frame":{"type":"chunk","revision":2,"index":0,"chunk":{"type":"text-delta","index":0,"text":"response"}}}));
    }
    let mut received = 0;
    for _ in 0..100 {
        received = events(&service).await["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["message"]["params"]["delta"] == "response")
            .count();
        if received == 32 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(received, 32);
    service.close().await;
}
