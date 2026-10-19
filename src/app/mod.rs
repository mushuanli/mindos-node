//! Wired runtime state.
//!
//! [`State`] is the composition root: it turns a validated [`Config`] into the
//! handful of components the HTTP layer needs. Each component owns exactly one
//! concern and is addressed directly, so a handler never reaches into a field
//! it does not need:
//!
//! | component | concern |
//! |---|---|
//! | [`Auth`] | who may read or write which alias |
//! | [`Exports`] | the directory capabilities |
//! | [`FileGate`] | mutual exclusion with command execution |
//! | [`Workers`] | bounded blocking-filesystem concurrency |
//! | [`Operations`] | idempotent write receipts |
//! | [`Execution`] | process epoch, admission and the retained lock |

use crate::{
    auth::Auth,
    config::{open_exports, Config},
    core::{gate::FileGate, workers::Workers},
    fs::Exports,
    operations::Operations,
    process::Execution,
};
use std::sync::Arc;

/// Blocking filesystem worker budget.
pub const WORKER_LIMIT: usize = 16;
/// Random material used for cursors and the process epoch.
const KEY_BYTES: usize = 32;

pub struct State {
    pub auth: Auth,
    pub exports: Exports,
    pub cursor_key: [u8; KEY_BYTES],
    pub files: FileGate,
    pub workers: Workers,
    pub operations: Operations,
    pub execution: Execution,
    pub sync: Option<Arc<crate::sync::SyncService>>,
    pub harness: crate::harness::Harnesses,
    pub projects: Option<Arc<crate::projects::ProjectService>>,
    pub allowed_origins: Vec<String>,
}

impl State {
    /// Validate a parsed configuration and build the runtime.
    pub fn from_config(config: &Config) -> Result<Arc<Self>, String> {
        Self::from_config_with_plugins(config, crate::harness::HarnessPlugins::default())
    }
    pub fn from_config_with_plugins(
        config: &Config,
        plugins: crate::harness::HarnessPlugins,
    ) -> Result<Arc<Self>, String> {
        crate::config::identity::validate_identity(config)?;
        let sync = open_sync(config)?;
        let projects = config
            .projects
            .as_ref()
            .map(|p| crate::projects::ProjectService::open(p, config).map(Arc::new))
            .transpose()?;
        let exports = configured_exports(config)?;
        let auth = configured_auth(config, &exports)?;
        let mut state = Self::new(auth, exports)?;
        Arc::get_mut(&mut state).unwrap().sync = sync;
        Arc::get_mut(&mut state).unwrap().projects = projects;
        Arc::get_mut(&mut state).unwrap().harness = crate::harness::Harnesses::with_plugins(
            crate::harness::config::validate_with_plugins(config, &plugins)?,
            hex(&random_bytes()?),
            plugins,
        )?;
        Arc::get_mut(&mut state).unwrap().allowed_origins = config.allowed_origins.clone();
        if state.harness.enabled() {
            crate::harness::Harnesses::maintain(Arc::downgrade(&state));
        }
        Ok(state)
    }

    /// Assemble the runtime with fresh random material.
    ///
    /// The pagination cursor key and the process epoch are independent secrets:
    /// the epoch is published through `/v1/capabilities`, the key is not.
    pub fn new(auth: Auth, exports: Exports) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            auth,
            exports,
            cursor_key: random_bytes()?,
            files: FileGate::new(),
            workers: Workers::new(WORKER_LIMIT),
            operations: Operations::default(),
            execution: Execution::new(hex(&random_bytes()?)),
            sync: None,
            projects: None,
            harness: crate::harness::Harnesses::new(vec![], hex(&random_bytes()?))?,
            allowed_origins: vec![],
        }))
    }

    /// Test constructor with deterministic random material.
    #[doc(hidden)]
    pub fn with_secrets(
        auth: Auth,
        exports: Exports,
        cursor_key: [u8; KEY_BYTES],
        process_epoch: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            auth,
            exports,
            cursor_key,
            files: FileGate::new(),
            workers: Workers::new(WORKER_LIMIT),
            operations: Operations::default(),
            execution: Execution::new(process_epoch.clone()),
            sync: None,
            projects: None,
            harness: crate::harness::Harnesses::new(vec![], process_epoch.clone())
                .expect("Empty harness catalog is valid"),
            allowed_origins: vec![],
        })
    }
}

fn open_sync(config: &Config) -> Result<Option<Arc<crate::sync::SyncService>>, String> {
    config
        .sync
        .as_ref()
        .filter(|s| s.enabled)
        .map(
            |c| -> Result<Arc<crate::sync::SyncService>, crate::sync::Error> {
                crate::sync::SyncService::validate_isolation(c, config)?;
                let service = crate::sync::SyncService::open(c)?;
                // A missing root is created by open; re-check the resolved path
                // against the exports now that it exists.
                crate::sync::SyncService::validate_isolation(c, config)?;
                Ok(service)
            },
        )
        .transpose()
        .map_err(|e| e.to_string())
}
fn configured_exports(config: &Config) -> Result<Exports, String> {
    crate::config::exports::validate_presence(config)?;
    if config.exports.is_empty() {
        Ok(Exports::new(std::collections::BTreeMap::new()))
    } else {
        open_exports(&config.exports)
    }
}
fn configured_auth(config: &Config, exports: &Exports) -> Result<Auth, String> {
    let server_id = match &config.server_id {
        Some(id) => Some(id.clone()),
        None if config.execution || !config.harnesses.is_empty() => {
            Some(format!("pi-agent-{}", hex(&random_bytes()?)))
        }
        None => None,
    };
    crate::config::authorization::compile(config, exports, server_id)
}

fn random_bytes() -> Result<[u8; KEY_BYTES], String> {
    let mut bytes = [0u8; KEY_BYTES];
    getrandom::getrandom(&mut bytes).map_err(|_| "Random source unavailable".to_owned())?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
