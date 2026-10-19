#[allow(dead_code)]
#[path = "common/dsh.rs"]
mod native;
#[allow(dead_code)]
#[path = "common/directory_acl.rs"]
mod support;
use mindos_node::{app::State, projects};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn managed_dsh_uses_the_project_launcher_and_readonly_mounts() {
    let directory = tempfile::Builder::new()
        .prefix("mindos-node-dsh-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let (root, state) = support::fixture_at(directory);
    drop(state);
    let home = root.path().join("alice-dsh");
    std::fs::create_dir(&home).unwrap();
    let command = root.path().join("dsh-peer");
    std::fs::write(&command, "#!/bin/sh\nexit 3\n").unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = support::config(root.path(), false);
    config.execution = true;
    config.projects.as_mut().unwrap().network = true;
    config.exports[0].grants[0].access = mindos_node::config::Access::Ro;
    config.exports[0].grants[0].execute = true;
    config.harnesses=vec![toml::from_str(&format!("id='dsh'\nkind='dsh'\nprincipal_id='alice'\nprojects=true\ncommand={command:?}\nhome={home:?}")).unwrap()];
    config.harnesses[0].aliases = vec!["old-dsh".into()];
    let state = State::from_config(&config).unwrap();
    mindos_node::process::enable(&state).await.unwrap();
    let who = state.auth.index("alice").unwrap();
    let project = projects::call(
        &state,
        who,
        "project_register",
        json!({"name":"DSH","alias":"docs","path":"","access":"ro"}),
    )
    .await
    .unwrap()["project"]
        .clone();
    // Existing native sessions retain their original physical workspace path.
    let host = native::Host::new(&root.path().join("docs")).await;
    std::fs::write(&command,format!("#!/bin/sh\n[ -f readme.txt ] || exit 8\nif touch forbidden-write 2>/dev/null; then exit 9; fi\n[ -f {source:?}/readme.txt ] || exit 10\nif touch {source:?}/native-forbidden-write 2>/dev/null; then exit 11; fi\n[ ! -e {private:?}/secret.txt ] || exit 12\nprintf 'dsh web: {}?token=fixture\\n'\nexec sleep 300\n",host.url,source=root.path().join("docs"),private=root.path().join("private"))).unwrap();
    let mut args = json!({"profileId":"old-dsh","projectId":project["id"],"revision":project["revision"],"epoch":state.harness.profiles()["epoch"],"requestId":"create","workspaceId":project["id"]});
    let listed = state
        .harness
        .project_call(&state, who, "harness_sessions", args.clone())
        .await
        .unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["id"], "existing");
    let created = state
        .harness
        .project_call(&state, who, "harness_create", args.clone())
        .await
        .unwrap();
    assert_eq!(created["outcome"], "committed", "{created}");
    args["sessionId"] = created["result"]["session"]["id"].clone();
    args["requestId"] = json!("turn");
    args["prompt"] = json!("read only");
    let turn = state
        .harness
        .project_call(&state, who, "harness_turn", args.clone())
        .await
        .unwrap();
    assert_eq!(turn["outcome"], "committed", "{turn}");
    let id = args["sessionId"].as_str().unwrap();
    host.emit(
        id,
        json!({"type":"event","event":{"seq":0,"type":"turn/start","data":{"turn":1}}}),
    );
    host.emit(id,json!({"type":"event","event":{"seq":1,"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}}}));
    for _ in 0..40 {
        let page = state
            .harness
            .project_call(&state, who, "harness_events", args.clone())
            .await
            .unwrap();
        if page["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["message"]["method"] == "turn/completed")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!root.path().join("docs/forbidden-write").exists());
    assert!(!root.path().join("docs/native-forbidden-write").exists());
    let bob = state.auth.index("bob").unwrap();
    assert!(state
        .harness
        .project_call(&state, bob, "harness_sessions", args)
        .await
        .is_err());
    state.harness.close().await;
    assert_eq!(
        support::request(
            &state,
            Some(support::ALICE),
            "PUT",
            "/v1/fs/docs/content?path=new",
            Value::Null
        )
        .await
        .0,
        axum::http::StatusCode::FORBIDDEN
    );
}
