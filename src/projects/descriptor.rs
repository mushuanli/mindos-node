//! Per-caller capability projections; persisted project policy is never a grant by itself.
use super::{model::Project, service};
use crate::{app::State, core::error::Error};
use serde_json::{json, Value};

pub(super) fn project(state: &State, identity: usize, project: &Project) -> Result<Value, Error> {
    service::authorize(state, identity, project)?;
    let project = service::effective(state, identity, project);
    let client = state.auth.client(identity);
    let run = state.execution.ready()
        && service::mounts(&project)
            .iter()
            .all(|m| client.may_execute(&m.alias));
    let mut value = serde_json::to_value(&project).map_err(|_| Error::internal())?;
    value["authorization"] = client.authorization();
    value["capabilities"] = json!({"fileRead":true,"fileWrite":project.access == "rw" && client.may_write(&project.alias),
        "run":run,"sessionRead":client.has_harness(),"sessionControl":run && client.has_harness()});
    Ok(value)
}
