use crate::common::{app_export, body_json, mutate_request, request};
use axum::{body::Body, http::StatusCode};
use mindos_node::fs::Export;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn metadata_uses_authenticated_mutations_and_normal_stat_reads() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note.md"), "Text").unwrap();
    let app = app_export(Export::exclusive(root.path()).unwrap());
    let response = app.clone().oneshot(mutate_request("meta", &json!({"action":"metadata","path":"note.md","metadata":{"_custom":{"anchor":"a"}},"removeKeys":[]}).to_string())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["outcome"], "committed");
    let response = app
        .clone()
        .oneshot(
            request("/v1/fs/docs/stat")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(json!({"paths":["note.md"]}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        body_json(response).await["results"][0]["stat"]["metadata"]["_custom"]["anchor"],
        "a"
    );
    let response = app
        .clone()
        .oneshot(mutate_request(
            "traversal",
            &json!({"action":"metadata","path":"../escape","metadata":{}}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(mutate_request(
            "private",
            &json!({"action":"metadata","path":".mindos/meta.sqlite","metadata":{}}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    drop(app);
    let readonly = app_export(Export::open(root.path()).unwrap());
    let response = readonly
        .oneshot(mutate_request(
            "denied",
            &json!({"action":"metadata","path":"note.md","metadata":{"x":1}}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
