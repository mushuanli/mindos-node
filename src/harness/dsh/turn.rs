//! Prompt admission and lifecycle controls; native turns remain asynchronous.
use super::*;
impl Dsh {
    async fn turn(&self, client: &Arc<transport::Client>, args: &Value) -> Result<Value, Error> {
        let id = string(args, "sessionId")?;
        let live = self.owned(id)?;
        let prompt = string(args, "prompt")?;
        if prompt.is_empty() || prompt.len() > 128 * 1024 {
            return Err(Error::invalid());
        }
        if args["attachments"]
            .as_array()
            .is_some_and(|files| !files.is_empty())
        {
            return Err(Error::unsupported());
        }
        let operation = string(args, "requestId")?;
        self.begin(id, operation, &live)?;
        let result = client
            .call(
                "session/prompt",
                json!({"request":{"sessionId":id,"requestId":operation,
            "mode":"queue","content":[{"type":"text","text":prompt}]}}),
            )
            .await;
        self.admission(id, operation, &live, result)
    }
    fn begin(&self, id: &str, operation: &str, live: &Live) -> Result<(), Error> {
        let mut cursor = live.cursor.lock().unwrap();
        if !cursor.turn.is_empty() || live.session.lock().unwrap()["status"] == "active" {
            return Err(Error::busy());
        }
        if let Some(runtime) = &self.runtime {
            runtime.begin(id, operation)?;
            runtime.bind_turn(id, operation, operation);
        }
        cursor.turn = operation.into();
        cursor.native_turn = None;
        cursor.interrupted = false;
        live.session.lock().unwrap()["activeTurnId"] = json!(operation);
        Ok(())
    }
    fn admission(
        &self,
        id: &str,
        operation: &str,
        live: &Live,
        result: Result<Value, Error>,
    ) -> Result<Value, Error> {
        match result {
            Ok(value) if value["accepted"] == true => Ok(json!({"turnId":operation})),
            Ok(_) => Err(Error::unavailable()),
            Err(error) => {
                if let Some(runtime) = &self.runtime {
                    runtime.submission_failed(id, operation, &error);
                }
                if !matches!(error.code, "EIO" | "ETIMEDOUT") {
                    live.cursor.lock().unwrap().turn.clear();
                    live.session.lock().unwrap()["activeTurnId"] = Value::Null;
                }
                Err(error)
            }
        }
    }

    pub(super) async fn execute(&self, name: &str, args: &Value) -> Result<Value, Error> {
        let client = self.client().await?;
        match name {
            "harness_create" => self.create(&client, args).await,
            "harness_resume" => {
                self.attach(
                    &client,
                    self.summary(&client, string(args, "sessionId")?).await?,
                    None,
                )
                .await
            }
            "harness_turn" => self.turn(&client, args).await,
            "harness_interrupt" => self.interrupt(&client, args).await,
            "harness_respond" => {
                interactions::respond(&client, &self.sessions, &self.events, args).await
            }
            "harness_rename" | "harness_archive" => self.manage(&client, name, args).await,
            _ => Err(Error::unsupported()),
        }
    }
    async fn interrupt(&self, client: &transport::Client, args: &Value) -> Result<Value, Error> {
        let id = string(args, "sessionId")?;
        let live = self.owned(id)?;
        if live.cursor.lock().unwrap().turn != string(args, "turnId")? {
            return Err(Error::conflict("HARNESS_TURN_CHANGED"));
        }
        live.cursor.lock().unwrap().interrupted = true;
        let result = client
            .call("session/cancel", json!({"request":{"sessionId":id}}))
            .await?;
        if result["accepted"] != true {
            return Err(Error::busy());
        }
        Ok(json!({}))
    }
}
