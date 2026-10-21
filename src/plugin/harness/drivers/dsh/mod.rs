//! DSH Remote adapter; native policies and persistence remain owned by DSH.
mod catalog;
mod commands;
pub mod config;
mod interactions;
mod models;
mod process;
mod protocol;
mod sessions;
mod transport;
mod turn;
use crate::{
    core::error::Error,
    harness::{
        config::{ProfileConfig, WorkspaceConfig},
        driver::HarnessDriver,
        events::Events,
        service::string,
    },
    projects::runtime::ProjectRuntime,
};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

struct Live {
    cursor: Mutex<protocol::Cursor>,
    session: Mutex<Value>,
    stop: CancellationToken,
    _slot: tokio::sync::OwnedSemaphorePermit,
}
pub struct Dsh {
    config: ProfileConfig,
    runtime: Option<Arc<ProjectRuntime>>,
    connection: tokio::sync::Mutex<Option<process::Connection>>,
    sessions: Arc<Mutex<BTreeMap<String, Arc<Live>>>>,
    events: Arc<Mutex<Events>>,
    stop: CancellationToken,
    slots: Arc<tokio::sync::Semaphore>,
}
impl Dsh {
    pub fn new(mut config: ProfileConfig, runtime: Option<Arc<ProjectRuntime>>) -> Self {
        if let Some(runtime) = &runtime {
            config.workspaces = vec![WorkspaceConfig {
                id: runtime.project.id.clone(),
                path: runtime.cwd().into(),
            }];
        }
        let mut events = Events::new();
        events.runtime = runtime.clone();
        let slots = Arc::new(tokio::sync::Semaphore::new(
            config.dsh.clone().unwrap_or_default().max_sessions,
        ));
        Self {
            config,
            runtime,
            connection: tokio::sync::Mutex::new(None),
            slots,
            sessions: Arc::default(),
            events: Arc::new(Mutex::new(events)),
            stop: CancellationToken::new(),
        }
    }
    async fn client(&self) -> Result<Arc<transport::Client>, Error> {
        if self.stop.is_cancelled() {
            return Err(Error::unavailable());
        }
        let mut connection = self.connection.lock().await;
        if let Some(connection) = connection.as_ref() {
            if !connection.client.stop.is_cancelled() {
                return Ok(connection.client.clone());
            }
            return Err(Error::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "DSH_DISCONNECTED",
            ));
        }
        let current =
            process::Connection::start(&self.config, self.runtime.clone(), self.stop.child_token())
                .await?;
        interactions::start(
            current.client.clone(),
            self.sessions.clone(),
            self.events.clone(),
        )
        .await?;
        let client = current.client.clone();
        *connection = Some(current);
        Ok(client)
    }
}
fn follow(
    id: String,
    live: Arc<Live>,
    mut subscription: transport::Subscription,
    events: Arc<Mutex<Events>>,
    client: Arc<transport::Client>,
) {
    tokio::spawn(async move {
        loop {
            let frame = tokio::select! {
                _=live.stop.cancelled()=>return,
                frame=subscription.next()=>match frame {Ok(frame)=>frame,Err(_)=>break},
            };
            let messages = protocol::frame(&frame, &mut live.cursor.lock().unwrap(), &id);
            for message in messages {
                update_session(&live, &message);
                if events.lock().unwrap().push(message).is_err() {
                    client.stop.cancel();
                    return;
                }
            }
        }
        live.session.lock().unwrap()["owned"] = json!(false);
        let _ = events
            .lock()
            .unwrap()
            .push(json!({"method":"harness/disconnected","params":{"threadId":id}}));
        client.stop.cancel();
    });
}
impl HarnessDriver for Dsh {
    fn descriptor(&self) -> Value {
        json!({"id":self.config.id,"kind":"dsh","projectRuntime":self.config.projects,
            "workspaces":self.config.workspaces.iter().map(|w|json!({"id":w.id})).collect::<Vec<_>>(),
            "capabilities":{"models":true,"slashCommands":true,"history":true,"create":true,"resume":true,"interrupt":true,"interactions":true,
                "usage":false,"fork":false,"search":true,"searchPrompts":true,"searchKeywords":true,"rename":true,"archive":true,"unarchive":false,"delete":false,"attachments":[]}})
    }
    fn read<'a>(&'a self, name: &'a str, args: Value) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move { self.read_native(name, args).await })
    }
    fn execute<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move { self.execute(name, args).await })
    }
    fn stop(&self) {
        self.stop.cancel();
    }
    fn idle_since(&self) -> Option<std::time::Instant> {
        if self
            .sessions
            .lock()
            .unwrap()
            .values()
            .any(|live| !live.cursor.lock().unwrap().turn.is_empty())
        {
            return None;
        }
        self.events.lock().unwrap().idle_since()
    }
    fn drain(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.stop.cancel();
            if let Some(connection) = self.connection.lock().await.as_ref() {
                connection.drain().await;
            }
        })
    }
}

fn update_session(live: &Live, message: &Value) {
    let mut session = live.session.lock().unwrap();
    match message["method"].as_str() {
        Some("turn/started") => {
            session["status"] = json!("active");
            session["activeTurnId"] = message["params"]["turn"]["id"].clone();
        }
        Some("turn/completed") => {
            session["status"] = json!("idle");
            session["activeTurnId"] = Value::Null;
            session["lastTurnResult"] = if message["params"]["turn"]["status"] == "interrupted" {
                json!("cancelled")
            } else {
                message["params"]["turn"]["status"].clone()
            };
        }
        _ => (),
    }
}
