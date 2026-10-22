//! Harness-neutral control port. Native protocols stay in their own adapters.
use crate::core::error::Error;
use futures_util::future::BoxFuture;
use serde_json::Value;

pub trait HarnessDriver: Send + Sync {
    fn subscribe(&self, _args: Value) -> crate::plugin::common::EventFuture<'_> {
        Box::pin(async { Err(Error::unsupported()) })
    }
    fn descriptor(&self) -> Value;
    fn read<'a>(&'a self, name: &'a str, args: Value) -> BoxFuture<'a, Result<Value, Error>>;
    fn execute<'a>(&'a self, name: &'a str, args: &'a Value)
        -> BoxFuture<'a, Result<Value, Error>>;
    fn stop(&self);
    /// Conservative default: plugins must explicitly prove no execution or interaction is live.
    fn idle_since(&self) -> Option<std::time::Instant> {
        None
    }
    fn drain(&self) -> BoxFuture<'_, ()>;
}
