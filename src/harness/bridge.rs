//! Supervised stdio JSON-RPC. The child owns Codex state; we never edit its database.
use super::{config::ProfileConfig, events::Events};
use crate::core::error::Error;
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::{
    collections::HashMap,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, ChildStdout},
    sync::{mpsc, oneshot, watch},
};
use tokio_util::sync::CancellationToken;

type Reply = oneshot::Sender<Result<Value, Error>>;
type PendingCalls = Arc<Mutex<HashMap<u64, Pending>>>;
struct Pending {
    reply: Reply,
    method: String,
    started: Instant,
}
enum Command {
    Call(String, Value, Reply),
    Respond(Value, Value, Reply),
}
pub struct Bridge {
    sender: mpsc::Sender<Command>,
    pub events: Arc<Mutex<Events>>,
    stop: CancellationToken,
    exited: watch::Receiver<bool>,
}

impl Bridge {
    pub async fn start(config: &ProfileConfig) -> Result<Arc<Self>, Error> {
        Self::start_scoped(config, None).await
    }
    pub async fn start_scoped(
        config: &ProfileConfig,
        runtime: Option<Arc<crate::projects::runtime::ProjectRuntime>>,
    ) -> Result<Arc<Self>, Error> {
        let mut prepared = runtime
            .as_ref()
            .map(|r| r.prepare(config, vec!["app-server".into(), "--stdio".into()]))
            .transpose()?;
        let mut command = match prepared.as_mut() {
            Some(p) => {
                std::mem::replace(&mut p.command, tokio::process::Command::new("/bin/false"))
            }
            None => tokio::process::Command::new(&config.command),
        };
        command.env_clear();
        command.as_std_mut().process_group(0);
        for key in [
            "PATH",
            "HOME",
            "USER",
            "LANG",
            "TMPDIR",
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        super::environment::inherit(&mut command);
        if runtime.is_none() {
            command.args(["app-server", "--stdio"]);
        }
        if let Some(runtime) = &runtime {
            command
                .env("HOME", "/tmp")
                .env("PI_AGENT_PROJECT", &runtime.project.id)
                .env("FS_AGENT_PROJECT", &runtime.project.id);
        }
        let mut child = command
            .env("CODEX_HOME", &config.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| Error::unavailable())?;
        let input = child.stdin.take().ok_or_else(Error::internal)?;
        let output = child.stdout.take().ok_or_else(Error::internal)?;
        let (sender, receiver) = mpsc::channel(128);
        let (exit, exited) = watch::channel(false);
        let bridge = Arc::new(Self {
            sender,
            events: Arc::new(Mutex::new(Events::new())),
            stop: CancellationToken::new(),
            exited,
        });
        bridge.events.lock().unwrap().runtime = runtime.clone();
        let events = bridge.events.clone();
        let stop = bridge.stop.clone();
        tokio::spawn(async move {
            supervise(child, input, output, receiver, events, stop).await;
            let _ = exit.send(true);
        });
        if let Err(error) = bridge.call("initialize", json!({"clientInfo":{"name":"itookit-agent","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await {
            bridge.close(); return Err(error);
        }
        // Notifications have no reply, but are written in the same ordered channel.
        bridge
            .respond(Value::Null, json!({"method":"initialized","params":{}}))
            .await?;
        Ok(bridge)
    }
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        tokio::time::timeout(
            Duration::from_secs(25),
            self.persistent_call(method, params),
        )
        .await
        .map_err(|_| Error::timed_out())?
    }
    pub async fn persistent_call(&self, method: &str, params: Value) -> Result<Value, Error> {
        if self.stop.is_cancelled() {
            return Err(Error::unavailable());
        }
        let (send, receive) = oneshot::channel();
        self.sender
            .try_send(Command::Call(method.into(), params, send))
            .map_err(|_| Error::unavailable())?;
        receive.await.map_err(|_| Error::unavailable())?
    }
    pub async fn respond(&self, id: Value, result: Value) -> Result<Value, Error> {
        let (send, receive) = oneshot::channel();
        self.sender
            .try_send(Command::Respond(id, result, send))
            .map_err(|_| Error::unavailable())?;
        await_reply(receive).await
    }
    pub fn close(&self) {
        self.stop.cancel();
    }
    pub async fn drain(&self) {
        self.close();
        let mut exited = self.exited.clone();
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while !*exited.borrow() {
                if exited.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    }
}

async fn await_reply(receive: oneshot::Receiver<Result<Value, Error>>) -> Result<Value, Error> {
    tokio::time::timeout(Duration::from_secs(25), receive)
        .await
        .map_err(|_| Error::timed_out())?
        .map_err(|_| Error::unavailable())?
}

async fn write(input: &mut (impl AsyncWrite + Unpin), message: &Value) -> Result<(), Error> {
    let mut bytes = serde_json::to_vec(message).map_err(|_| Error::internal())?;
    bytes.push(b'\n');
    input
        .write_all(&bytes)
        .await
        .map_err(|_| Error::unavailable())
}

async fn supervise(
    mut child: Child,
    input: tokio::process::ChildStdin,
    output: ChildStdout,
    receiver: mpsc::Receiver<Command>,
    events: Arc<Mutex<Events>>,
    stop: CancellationToken,
) {
    let pending = Arc::new(Mutex::new(HashMap::new()));
    pump(input, output, receiver, &pending, &events, &stop).await;
    // Reclaim this supervisor's process group before reaping its leader.
    if let Some(pid) = child
        .id()
        .and_then(|id| rustix::process::Pid::from_raw(id as i32))
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill().await;
    let clean = child.wait().await.is_ok();
    if let Some(runtime) = events.lock().unwrap().runtime.as_ref() {
        runtime.stopped(clean);
    }
    stop.cancel();
    for (_, reply) in pending.lock().unwrap().drain() {
        let _ = reply.reply.send(Err(Error::unavailable()));
    }
    let _ = events
        .lock()
        .unwrap()
        .push(json!({"method":"harness/disconnected","params":{}}));
}

/// Keep draining native output while a large submission waits for stdin capacity.
async fn pump(
    input: impl AsyncWrite + Unpin,
    mut output: impl AsyncRead + Unpin,
    receiver: mpsc::Receiver<Command>,
    pending: &PendingCalls,
    events: &Mutex<Events>,
    stop: &CancellationToken,
) {
    let writer = send_commands(input, receiver, pending);
    tokio::pin!(writer);
    let mut line = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = &mut writer => break,
            read = output.read(&mut chunk) => {
                let Ok(n) = read else { break; }; if n == 0 { break; }
                if consume(&chunk[..n], &mut line, pending, events).is_err() { break; }
            }
        }
    }
}

async fn send_commands(
    mut input: impl AsyncWrite + Unpin,
    mut receiver: mpsc::Receiver<Command>,
    pending: &PendingCalls,
) {
    let mut next = 0u64;
    while let Some(command) = receiver.recv().await {
        let sent = tokio::time::timeout(
            Duration::from_secs(25),
            send_command(command, &mut input, pending, &mut next),
        )
        .await;
        if !matches!(sent, Ok(Ok(()))) {
            break;
        }
    }
}

fn consume(
    chunk: &[u8],
    line: &mut Vec<u8>,
    pending: &PendingCalls,
    events: &Mutex<Events>,
) -> Result<(), Error> {
    for byte in chunk {
        if *byte == b'\n' {
            let message = serde_json::from_slice::<Value>(line).map_err(|_| Error::internal())?;
            line.clear();
            dispatch(message, pending, events)?;
        } else {
            if line.len() >= 8 * 1024 * 1024 {
                return Err(Error::too_large("EFBIG"));
            }
            line.push(*byte);
        }
    }
    Ok(())
}

async fn send_command(
    command: Command,
    input: &mut (impl AsyncWrite + Unpin),
    pending: &PendingCalls,
    next: &mut u64,
) -> Result<(), Error> {
    match command {
        Command::Call(method, params, reply) => {
            *next += 1;
            let message = json!({"id":next,"method":method,"params":params});
            if register(pending, *next, method, reply) {
                write(input, &message).await?;
            }
        }
        Command::Respond(id, result, reply) => {
            let message = if id.is_null() {
                result
            } else {
                json!({"id":id,"result":result})
            };
            let result = write(input, &message).await;
            let _ = reply.send(result.map(|_| json!({})));
            result?;
        }
    }
    Ok(())
}

fn register(pending: &PendingCalls, id: u64, method: String, reply: Reply) -> bool {
    let mut pending = pending.lock().unwrap();
    pending.retain(|_, reply| !reply.reply.is_closed());
    if pending.len() >= 128 {
        let _ = reply.send(Err(Error::too_many("HARNESS_CALL_LIMIT")));
        return false;
    }
    pending.insert(
        id,
        Pending {
            reply,
            method,
            started: Instant::now(),
        },
    );
    true
}

fn dispatch(message: Value, pending: &PendingCalls, events: &Mutex<Events>) -> Result<(), Error> {
    if message.get("method").is_some() {
        return events.lock().unwrap().push(message);
    }
    let reply = message["id"]
        .as_u64()
        .and_then(|id| pending.lock().unwrap().remove(&id));
    if let Some(reply) = reply {
        let result = if message.get("error").is_some() {
            let error = native_error(&message["error"]);
            crate::core::events::emit(
                crate::core::events::Level::Warn,
                "harness.native.failed",
                json!({
                    "method": reply.method, "rpcCode": message["error"]["code"].as_i64(),
                    "code": error.code, "durationMs": reply.started.elapsed().as_millis() as u64
                }),
            );
            Err(error)
        } else {
            let mut result = message["result"].clone();
            if reply.method == "thread/read" && result.is_object() {
                result["_piAgentEventCursor"] = events.lock().unwrap().position();
            }
            Ok(result)
        };
        let _ = reply.reply.send(result);
    }
    Ok(())
}

fn native_error(error: &Value) -> Error {
    if error["message"]
        .as_str()
        .is_some_and(|message| message.contains("already has an active writer"))
    {
        return Error::conflict("HARNESS_SESSION_BUSY");
    }
    if error["code"] == -32602 {
        return Error::conflict("HARNESS_INVALID_PARAMS");
    }
    if error["code"] == -32601 {
        return Error::unsupported();
    }
    if error["message"]
        .as_str()
        .is_some_and(|s| s.contains("not materialized yet"))
    {
        return Error::conflict("HARNESS_HISTORY_PENDING");
    }
    Error::conflict("HARNESS_RPC_ERROR")
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    async fn backpressure_peer(
        mut input: tokio::io::DuplexStream,
        mut output: tokio::io::DuplexStream,
        sender: mpsc::Sender<Command>,
        prior: oneshot::Receiver<Result<Value, Error>>,
        pending: &PendingCalls,
        events: &Mutex<Events>,
        stop: &CancellationToken,
    ) {
        let (reply, mut receive) = oneshot::channel();
        sender
            .send(Command::Call(
                "turn/start".into(),
                json!({"input":"x".repeat(65536)}),
                reply,
            ))
            .await
            .unwrap();
        // A byte proves the writer entered write_all; the peer then stops reading.
        input.read_u8().await.unwrap();
        write(&mut output, &json!({"method":"item/agentMessage/delta","params":{"threadId":"other","itemId":"item","delta":"y".repeat(4096)}})).await.unwrap();
        write(&mut output, &json!({"id":99,"result":{"ok":true}}))
            .await
            .unwrap();
        assert_eq!(prior.await.unwrap().unwrap()["ok"], true);
        assert_eq!(
            events.lock().unwrap().page(None, 0)["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(pending.lock().unwrap().contains_key(&1));
        assert!(receive.try_recv().is_err());
        stop.cancel();
    }
    #[tokio::test]
    async fn blocked_submission_keeps_draining_other_sessions_events_and_replies() {
        let (input, peer_input) = tokio::io::duplex(64);
        let (peer_output, output) = tokio::io::duplex(64);
        let (sender, receiver) = mpsc::channel(128);
        let pending: PendingCalls = Arc::default();
        let (reply, prior) = oneshot::channel();
        register(&pending, 99, "thread/read".into(), reply);
        let events = Mutex::new(Events::new());
        let stop = CancellationToken::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                pump(input, output, receiver, &pending, &events, &stop),
                backpressure_peer(
                    peer_input,
                    peer_output,
                    sender,
                    prior,
                    &pending,
                    &events,
                    &stop
                )
            );
        })
        .await
        .expect("stdin backpressure must not stop stdout or unrelated replies");
    }

    #[test]
    fn retains_safe_native_error_categories() {
        assert_eq!(
            native_error(
                &json!({"code":-32600,"message":"thread private-id already has an active writer"})
            )
            .code,
            "HARNESS_SESSION_BUSY"
        );
        assert_eq!(
            native_error(&json!({"code":-32602,"message":"private details"})).code,
            "HARNESS_INVALID_PARAMS"
        );
        assert_eq!(native_error(&json!({"code":-32601})).code, "ECAPABILITY");
        assert_eq!(
            native_error(&json!({"code":-32000,"message":"thread not materialized yet"})).code,
            "HARNESS_HISTORY_PENDING"
        );
        assert_eq!(
            native_error(&json!({"code":-32000,"message":"private details"})).code,
            "HARNESS_RPC_ERROR"
        );
    }
}
