//! Executable projects are directory grants, independent of sync datasets.
use crate::process::Mount;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub root: PathBuf,
    #[serde(default)]
    pub network: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub alias: String,
    pub path: String,
    pub access: String,
    pub revision: u64,
    pub mounts: Vec<Mount>,
    #[serde(deserialize_with = "crate::auth::deserialize_owner")]
    pub owner: String,
    pub root_identity: String,
    pub mount_identities: Vec<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Register {
    pub name: String,
    pub alias: String,
    pub path: String,
    pub access: String,
    #[serde(default)]
    pub mounts: Vec<Mount>,
    #[serde(default)]
    pub create_directory: bool,
}
