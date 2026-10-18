//! SDK commands and explicit session controls; terminal navigation belongs to the host.
use super::*;
const MODES: &[&str] = &["manual", "plan", "acceptEdits", "dontAsk"];
const NAVIGATION: &[&str] = &["clear", "reset", "new", "resume", "fork", "exit", "quit"];

pub(super) fn catalog(raw: &Value) -> Value {
    let raw = raw
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| {
            let mut row = if let Some(name) = entry.as_str() {
                json!({"name":name})
            } else {
                entry.clone()
            };
            if row["name"] == "compact" {
                row["kind"] = json!("action");
            }
            row
        })
        .collect::<Vec<_>>();
    let mut rows = super::super::commands::with_aliases(&json!(raw))
        .as_array()
        .unwrap()
        .clone();
    rows.retain(|row| {
        !NAVIGATION.contains(&row["name"].as_str().unwrap_or("")) && row["name"] != "permissions"
    });
    rows.truncate(255);
    rows.push(json!({"name":"permissions","description":"View or change this session's tool permission mode",
        "argumentHint":"[manual|plan|acceptEdits|dontAsk]","kind":"action",
        "argumentChoices":MODES.iter().map(|mode|json!({"value":mode,"label":mode})).collect::<Vec<_>>()}));
    json!(rows)
}
pub(super) fn refresh(prior: &Value, raw: &Value) -> Value {
    let mut rows = prior.as_array().unwrap().clone();
    for row in super::super::commands::with_aliases(raw)
        .as_array()
        .unwrap()
    {
        if !rows.iter().any(|old| old["name"] == row["name"]) {
            rows.push(row.clone());
        }
    }
    catalog(&json!(rows))
}
impl Claude {
    pub(super) async fn commands(&self, args: &Value) -> Result<Value, Error> {
        if let Some(id) = args["sessionId"].as_str() {
            self.session(id).await?;
            if let Some(process) = self.sessions.get(id).await {
                return Ok(json!({"commands":process.commands.lock().unwrap().clone()}));
            }
        }
        Ok(json!({"commands":self.discover(args, true).await?["commands"]}))
    }
    pub(super) async fn command(&self, args: &Value) -> Result<Value, Error> {
        let process = self.owned(string(args, "sessionId")?).await?;
        let name = string(args, "command")?;
        if !process
            .commands
            .lock()
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == name)
        {
            return Err(Error::unsupported());
        }
        let input = super::super::commands::arguments(args)?;
        if name == "permissions" {
            return self.permissions(&process, args, input.trim()).await;
        }
        let mut turn = args.clone();
        turn["prompt"] = json!(format!("/{name} {input}").trim_end());
        self.turn(&turn).await
    }
    async fn permissions(
        &self,
        process: &process::Process,
        args: &Value,
        mode: &str,
    ) -> Result<Value, Error> {
        if args.get("selection").is_some() {
            return Err(Error::unsupported());
        }
        if process.session.lock().unwrap()["status"] == "active" {
            return Err(Error::busy());
        }
        if !mode.is_empty() {
            if !MODES.contains(&mode) {
                return Err(Error::invalid());
            }
            process
                .control(json!({"subtype":"set_permission_mode","mode":mode}))
                .await?;
            process.session.lock().unwrap()["permissionMode"] = json!(mode);
        }
        let current = process.session.lock().unwrap()["permissionMode"]
            .as_str()
            .unwrap_or("manual")
            .to_owned();
        Ok(
            json!({"commandId":args["requestId"],"output":format!("Permission mode: {current}\n/permissions [{}]\nSession only; project authorization still applies.", MODES.join(" | "))}),
        )
    }
}
