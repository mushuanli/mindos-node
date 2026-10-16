//! Loopback fixture for the documented DSH 0.2 HTTP/WS protocol.
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;

pub struct Host {
    pub url: String,
    pub state: Arc<Data>,
    task: tokio::task::JoinHandle<()>,
}
pub struct Data {
    pub rows: Mutex<BTreeMap<String, Value>>,
    pub records: Mutex<BTreeMap<String, Vec<Value>>>,
    pub calls: Mutex<Vec<Value>>,
    pub bus: broadcast::Sender<(String, Value)>,
    pub errors: Mutex<BTreeMap<String, Value>>,
    workspaces: Mutex<BTreeMap<String, String>>,
}
impl Drop for Host {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Host {
    pub async fn new(root: &std::path::Path) -> Self {
        let state = Arc::new(Data {
            rows: Mutex::default(),
            errors: Mutex::default(),
            records: Mutex::default(),
            calls: Mutex::default(),
            bus: broadcast::channel(1024).0,
            workspaces: Mutex::new(BTreeMap::from([(
                "w1".into(),
                root.to_str().unwrap().into(),
            )])),
        });
        state.rows.lock().unwrap().insert(
            "existing".into(),
            json!({"sessionId":"existing","cwd":root,"running":false,"updatedAt":1000}),
        );
        state.rows.lock().unwrap().insert(
            "private".into(),
            json!({"sessionId":"private","cwd":"/unshared","running":false}),
        );
        state.records.lock().unwrap().insert(
            "existing".into(),
            vec![json!({"type":"event","event":{
            "seq":0,"type":"user/message","data":{"content":[{"type":"text","text":"older"}]}}})],
        );
        let router = Router::new()
            .route("/", get(login))
            .route("/api/remote.mux", get(upgrade))
            .route("/api/:namespace/:method", post(rpc))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { url, state, task }
    }
    pub fn emit(&self, session: &str, frame: Value) {
        if frame["type"] == "event" {
            self.state
                .records
                .lock()
                .unwrap()
                .entry(session.into())
                .or_default()
                .push(frame.clone());
        }
        let _ = self.state.bus.send((session.into(), frame));
    }
    pub fn event(&self, frame: Value) {
        let _ = self.state.bus.send(("$events".into(), frame));
    }
}
async fn login() -> Response {
    (
        StatusCode::SEE_OTHER,
        [
            (
                "set-cookie",
                "dsh-auth-test=fixture; Path=/; HttpOnly; Max-Age=3600",
            ),
            ("location", "/"),
        ],
    )
        .into_response()
}
async fn upgrade(
    State(state): State<Arc<Data>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if headers.get("cookie").and_then(|v| v.to_str().ok()) != Some("dsh-auth-test=fixture") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(|socket| socket_loop(socket, state))
}
async fn socket_loop(socket: WebSocket, state: Arc<Data>) {
    let (mut sink, mut source) = socket.split();
    let mut bus = state.bus.subscribe();
    let mut streams = BTreeMap::<String, String>::new();
    loop {
        tokio::select! {
            message=source.next()=>{
                let text=match message {Some(Ok(Message::Text(text)))=>text,Some(Ok(Message::Ping(data)))=>{let _=sink.send(Message::Pong(data)).await;continue;},_=>break};
                let frame:Value=serde_json::from_str(&text).unwrap();let id=frame["streamId"].as_str().unwrap().to_owned();
                if frame["type"]=="cancel" {streams.remove(&id);continue;}
                let endpoint=frame["endpoint"].as_str().unwrap();
                let (key,value)=match endpoint {
                    "$events"=>("$events".into(),json!({"type":"ready","clientId":"fixture-client"})),
                    "workspace/follow"=>{
                        ("workspace".into(),workspace_baseline(&state))
                    }
                    "session/follow"=>{
                        let session=frame["payload"]["args"]["request"]["address"]["sessionId"].as_str().unwrap().to_owned();
                        let records=state.records.lock().unwrap().get(&session).cloned().unwrap_or_default();
                        let cursor=records.last().and_then(|r|r["event"]["seq"].as_i64()).unwrap_or(-1);
                        let limit=frame["payload"]["args"]["request"]["maxMessages"].as_u64().unwrap_or(80) as usize;
                        let start=records.len().saturating_sub(limit);
                        (session,json!({"type":"snapshot","cursor":cursor,"hasMore":start>0,"records":records[start..]}))
                    }
                    _=>break,
                };
                streams.insert(id.clone(),key);
                if sink.send(Message::Text(json!({"type":"item","streamId":id,"value":value}).to_string())).await.is_err(){break;}
            }
            value=bus.recv()=>{
                let Ok((key,value))=value else {break;};
                for (id,target) in &streams {if target==&key {
                    if sink.send(Message::Text(json!({"type":"item","streamId":id,"value":value}).to_string())).await.is_err(){return;}
                }}
            }
        }
    }
}
async fn rpc(
    State(state): State<Arc<Data>>,
    Path((namespace, method)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if headers.get("cookie").and_then(|v| v.to_str().ok()) != Some("dsh-auth-test=fixture") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let endpoint = format!("{namespace}/{method}");
    let args = &body["payload"]["args"];
    state.calls.lock().unwrap().push(body.clone());
    if let Some(error) = state.errors.lock().unwrap().get(&endpoint) {
        return Json(json!({"type":"server-response","rpcId":body["rpcId"],"result":{"ok":false,"error":error}})).into_response();
    }
    let value = match endpoint.as_str() {
        "session/list" => {
            json!({"items":state.rows.lock().unwrap().values().cloned().collect::<Vec<_>>()})
        }
        "workspace/create" => {
            let mut workspaces = state.workspaces.lock().unwrap();
            let id = format!("w{}", workspaces.len() + 1);
            workspaces.insert(id.clone(), args["request"]["path"].as_str().unwrap().into());
            json!({"workspace":{"workspaceId":id}})
        }
        "session/create" => {
            let workspace = args["request"]["workspaceId"].as_str().unwrap();
            let cwd = state.workspaces.lock().unwrap()[workspace].clone();
            let mut rows = state.rows.lock().unwrap();
            let id = args["request"]["sessionId"].as_str().unwrap().to_owned();
            rows.insert(
                id.clone(),
                json!({"sessionId":id,"cwd":cwd,"running":false}),
            );
            json!({"sessionId":id})
        }
        "session/prompt" => {
            if args["request"]["content"][0]["text"] == "slow-admission" {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            json!({"accepted":true})
        }
        "session/cancel" => json!({"accepted":true}),
        "session/page" => {
            let request = &args["request"];
            let id = request["address"]["sessionId"].as_str().unwrap();
            let before = request["beforeSeq"].as_i64().unwrap();
            let through = request["throughSeq"].as_i64().unwrap();
            let records = state
                .records
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|r| {
                    r["event"]["seq"]
                        .as_i64()
                        .is_some_and(|seq| seq < before && seq <= through)
                })
                .collect::<Vec<_>>();
            let start = records.len().saturating_sub(80);
            json!({"records":records[start..],"hasMore":start>0})
        }
        "session/rename" => json!({"title":args["request"]["title"],"seq":0}),
        "workspace/archiveSession" => json!({"archivedSessionIds":[args["request"]["sessionId"]]}),
        "$events/result" => json!({}),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    Json(json!({"type":"server-response","rpcId":body["rpcId"],"result":{"ok":true,"value":value}}))
        .into_response()
}

fn workspace_baseline(state: &Data) -> Value {
    let workspaces = state.workspaces.lock().unwrap().clone();
    let rows = state.rows.lock().unwrap();
    let items = workspaces
        .iter()
        .map(|(id, path)| {
            let ids = rows
                .values()
                .filter(|row| row["cwd"].as_str() == Some(path.as_str()))
                .map(|row| row["sessionId"].clone())
                .collect::<Vec<_>>();
            json!({"workspaceId":id,"path":path,"sessionIds":ids})
        })
        .collect::<Vec<_>>();
    json!({"type":"baseline","value":{"items":items,"archivedSessionIds":[]}})
}
