//! Binary entry point: load configuration, wire the runtime, serve, and shut
//! down in the order that keeps the exports fenced.
//!
//! Shutdown has two stages because a graceful stop has two waits:
//!
//! 1. stop admitting commands and let in-flight file work finish, bounded by
//!    [`DRAIN_TIMEOUT`];
//! 2. let open connections end on their own, bounded by [`CONNECTION_GRACE`].
//!
//! The second bound exists because a stalled download would otherwise keep the
//! process alive forever: the client never reads, so the connection never
//! closes. The order matters — the file gate is drained before the process is
//! allowed to exit.

use mindos_node::core::events::emit;
use mindos_node::{app::State, config::launch, process, router};
use serde_json::json;
use std::{sync::Arc, time::Duration};

mod startup;

/// Upper bound on how long shutdown waits for in-flight file work.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound on how long open connections may take to finish afterwards.
const CONNECTION_GRACE: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            emit(
                mindos_node::core::events::Level::Error,
                if std::env::args()
                    .nth(1)
                    .is_some_and(|arg| arg == "access" || arg == "sync")
                {
                    "cli.command_failed"
                } else {
                    "server.start_failed"
                },
                json!({"version": env!("CARGO_PKG_VERSION"), "error": error.to_string()}),
            );
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .first()
        .is_some_and(|arg| arg == "--version" || arg == "-V")
    {
        println!("mindos-node {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("{}", mindos_node::cli::HELP);
        return Ok(());
    }
    eprintln!("mindos-node {}", env!("CARGO_PKG_VERSION"));
    if args.first().is_some_and(|arg| arg == "access") {
        return mindos_node::cli::access(&args[1..]).map_err(Into::into);
    }
    if args.first().is_some_and(|s| s == "sync") {
        return mindos_node::sync::admin(&args[1..]).map_err(Into::into);
    }
    if args.len() > 1 || args.first().is_some_and(|arg| arg.starts_with('-')) {
        return Err("Invalid server arguments; use mindos-node --help".into());
    }
    let path = launch::resolve(args.first().map(String::as_str))?;
    let config = launch::load(&path)?;
    mindos_node::core::events::set_level(config.log_level);
    let log_file = mindos_node::core::events::init_file(&config.logging, &path)?;
    eprintln!("  Log file: {}", log_file.display());
    emit(
        mindos_node::core::events::Level::Info,
        "server.starting",
        json!({"version": env!("CARGO_PKG_VERSION"), "config": path, "logFile": log_file}),
    );
    let state = State::from_config(&config)?;
    // Advertise execution only after the sandbox has been proven to work.
    if config.execution {
        emit(
            mindos_node::core::events::Level::Debug,
            "sandbox.probing",
            json!({}),
        );
        process::enable(&state).await?;
        emit(
            mindos_node::core::events::Level::Info,
            "sandbox.ready",
            json!({}),
        );
    }
    if let Some(sync) = &state.sync {
        sync.start_maintenance();
    }
    let app = router(state.clone(), &config.allowed_origins)?;
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    emit(
        mindos_node::core::events::Level::Info,
        "server.ready",
        json!({"address": listener.local_addr()?.to_string(), "serverId": state.auth.server_id(),
        "version": env!("CARGO_PKG_VERSION"), "logFile": log_file,
        "principals":state.auth.clients().iter().map(|c| c.authorization()).collect::<Vec<_>>(),
        "execution": config.execution, "exportCount": config.exports.len(), "exports": startup::export_access(&state)}),
    );
    if mindos_node::core::events::enabled(mindos_node::core::events::Level::Info) {
        eprintln!(
            "mindos-node listening on {} (config {})",
            listener.local_addr()?,
            path.display()
        );
        startup::print(&config, listener.local_addr()?, &state);
        if config.principals.is_empty() && state.auth.client(0).username().is_none() {
            eprintln!("  API Key: {}", state.auth.client(0).secret());
        }
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(state))
        .await?;
    Ok(())
}

/// Stop admission, drain file work, then arm a hard exit deadline.
async fn shutdown(state: Arc<State>) {
    let signal = shutdown_signal().await.unwrap_or("signal-error");
    emit(
        mindos_node::core::events::Level::Info,
        "server.stopping",
        json!({"signal":signal}),
    );
    if let Some(sync) = &state.sync {
        sync.stop();
    }
    state.execution.stop_accepting();
    state.harness.close().await;
    state.operations.cancel_all();
    state.execution.cancel_all();
    drain_sync(&state).await;
    if tokio::time::timeout(DRAIN_TIMEOUT, state.files.drain())
        .await
        .is_err()
    {
        emit(
            mindos_node::core::events::Level::Warn,
            "server.drain_timeout",
            json!({"timeoutMs": DRAIN_TIMEOUT.as_millis()}),
        );
    }
    // Nothing left to protect; do not wait forever for a stalled client.
    tokio::spawn(async move {
        tokio::time::sleep(CONNECTION_GRACE).await;
        emit(
            mindos_node::core::events::Level::Info,
            "server.connection_grace_elapsed",
            json!({}),
        );
        std::process::exit(0);
    });
}

async fn shutdown_signal() -> std::io::Result<&'static str> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result=tokio::signal::ctrl_c()=>{result?;Ok("SIGINT")},
        _=terminate.recv()=>Ok("SIGTERM"),
    }
}

async fn drain_sync(state: &State) {
    let Some(storage) = state.sync.clone() else {
        return;
    };
    let work = tokio::task::spawn_blocking(move || storage.drain_timeout(DRAIN_TIMEOUT));
    match tokio::time::timeout(DRAIN_TIMEOUT, work).await {
        Ok(Ok(Ok(()))) => {}
        result => emit(
            mindos_node::core::events::Level::Warn,
            "sync.drain_failed",
            json!({"result":format!("{result:?}"),"recoveryRequired":true}),
        ),
    }
}
