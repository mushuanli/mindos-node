use super::*;

impl Dsh {
    pub(super) async fn commands(
        &self,
        client: &transport::Client,
        args: &Value,
    ) -> Result<Value, Error> {
        let Some(id) = args["sessionId"].as_str() else {
            return Ok(json!({"commands":[]}));
        };
        self.summary(client, id).await?;
        let rows = client.call("commands/list", json!({"agentId":id})).await?;
        let Some(rows) = rows.as_array().filter(|rows| rows.len() <= 256) else {
            return Err(Error::invalid());
        };
        let mut rows = rows.clone();
        for row in &mut rows {
            row["argumentHint"] = row["input"]["hint"].clone();
        }
        let mut commands = super::super::commands::normalize(&json!(rows));
        for row in commands.as_array_mut().unwrap() {
            row["kind"] = json!("action");
        }
        Ok(json!({"commands":commands}))
    }
    pub(super) async fn command(
        &self,
        client: &transport::Client,
        args: &Value,
    ) -> Result<Value, Error> {
        let id = string(args, "sessionId")?;
        if args.get("selection").is_some() {
            return Err(Error::unsupported());
        }
        let live = self.owned(id)?;
        if !live.cursor.lock().unwrap().turn.is_empty() {
            return Err(Error::busy());
        }
        let name = string(args, "command")?;
        let commands = self.commands(client, args).await?;
        if !commands["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == name)
        {
            return Err(Error::unsupported());
        }
        let input = super::super::commands::arguments(args)?;
        let operation = string(args, "requestId")?;
        if let Some(runtime) = &self.runtime {
            runtime.begin(id, operation)?;
        }
        let result = client.call("commands/execute", json!({"agentId":id,"line":format!("/{name} {input}").trim_end(),"submittedAttachments":[]})).await;
        match (&result, &self.runtime) {
            (Ok(_), Some(runtime)) => runtime.reject(id, operation),
            (Err(error), Some(runtime)) => runtime.submission_failed(id, operation, error),
            _ => (),
        }
        let result = result?;
        if !result["commandId"].is_string()
            || !matches!(result["result"]["kind"].as_str(), Some("success" | "error"))
        {
            return Err(Error::unavailable());
        }
        Ok(
            json!({"commandId":result["commandId"],"output":result["result"]["text"].as_str().unwrap_or(name),"failed":result["result"]["kind"]=="error"}),
        )
    }
}
