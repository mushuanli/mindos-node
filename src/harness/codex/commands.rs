use super::*;

pub(super) fn catalog() -> Value {
    json!({"commands":[
        {"name":"compact","description":"Compact this conversation's context","argumentHint":"","kind":"action"},
        {"name":"review","description":"Review working changes or the supplied instructions","argumentHint":"[instructions]","kind":"action"}
    ]})
}
impl Codex {
    pub(super) async fn command(&self, bridge: &Bridge, args: &Value) -> Result<Value, Error> {
        if args.get("selection").is_some() {
            return Err(Error::unsupported());
        }
        let id = string(args, "sessionId")?;
        self.require_owned(id).await?;
        let session = self.session(&self.summary(bridge, id).await?);
        if session["status"] == "active" || session["activeTurnId"].is_string() {
            return Err(Error::busy());
        }
        let input = super::super::commands::arguments(args)?;
        match string(args, "command")? {
            "compact" if input.trim().is_empty() => {
                bridge
                    .persistent_call("thread/compact/start", json!({"threadId":id}))
                    .await?;
                Ok(json!({}))
            }
            "review" => self.review(bridge, args, input).await,
            _ => Err(Error::unsupported()),
        }
    }
    async fn review(&self, bridge: &Bridge, args: &Value, input: &str) -> Result<Value, Error> {
        let id = string(args, "sessionId")?;
        let operation = string(args, "requestId")?;
        if let Some(runtime) = &self.runtime {
            runtime.begin(id, operation)?;
        }
        let target = if input.trim().is_empty() {
            json!({"type":"uncommittedChanges"})
        } else {
            json!({"type":"custom","instructions":input})
        };
        let result = bridge
            .persistent_call(
                "review/start",
                json!({"threadId":id,"delivery":"inline","target":target}),
            )
            .await;
        if let (Err(error), Some(runtime)) = (&result, &self.runtime) {
            runtime.submission_failed(id, operation, error);
        }
        let result = result?;
        let turn = string(&result["turn"], "id").map_err(|_| Error::internal())?;
        if let Some(runtime) = &self.runtime {
            runtime.bind_turn(id, operation, turn);
        }
        Ok(json!({"turnId":turn}))
    }
}
