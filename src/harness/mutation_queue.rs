//! Serialize one session while allowing independent native submissions to overlap.
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{
    Mutex as AsyncMutex, OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock,
};

#[derive(Default)]
pub(super) struct MutationQueues {
    sessions: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    topology: Arc<RwLock<()>>,
}
enum TopologyGuard {
    Read(#[allow(dead_code)] OwnedRwLockReadGuard<()>),
    Write(#[allow(dead_code)] OwnedRwLockWriteGuard<()>),
}
pub(super) struct MutationGuard {
    _topology: TopologyGuard,
    _session: OwnedMutexGuard<()>,
}
impl MutationQueues {
    fn session(&self, args: &Value) -> Arc<AsyncMutex<()>> {
        let key = args["sessionId"]
            .as_str()
            .map(|s| format!("session:{s}"))
            .unwrap_or_else(|| format!("request:{}", args["requestId"]));
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, queue| queue.strong_count() > 0);
        if let Some(queue) = sessions.get(&key).and_then(Weak::upgrade) {
            return queue;
        }
        let queue = Arc::new(AsyncMutex::new(()));
        sessions.insert(key, Arc::downgrade(&queue));
        queue
    }
    pub async fn acquire(&self, name: &str, args: &Value) -> MutationGuard {
        // Destructive native subtree changes exclude submissions to every descendant.
        let topology = if matches!(name, "harness_delete" | "harness_archive") {
            TopologyGuard::Write(self.topology.clone().write_owned().await)
        } else {
            TopologyGuard::Read(self.topology.clone().read_owned().await)
        };
        let session = self.session(args).lock_owned().await;
        MutationGuard {
            _topology: topology,
            _session: session,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use serde_json::json;
    #[tokio::test]
    async fn independent_sessions_bypass_a_pending_start_but_deletion_excludes_submissions() {
        let queues = MutationQueues::default();
        let a = json!({"sessionId":"a"});
        let b = json!({"sessionId":"b"});
        let pending = queues.acquire("harness_turn", &a).await;
        assert!(queues.acquire("harness_turn", &b).now_or_never().is_some());
        assert!(queues.acquire("harness_turn", &a).now_or_never().is_none());
        assert!(queues
            .acquire("harness_delete", &b)
            .now_or_never()
            .is_none());
        assert!(queues
            .acquire("harness_archive", &b)
            .now_or_never()
            .is_none());
        drop(pending);
        let deleting = queues.acquire("harness_delete", &a).await;
        assert!(queues.acquire("harness_turn", &b).now_or_never().is_none());
        drop(deleting);
        let archiving = queues.acquire("harness_archive", &a).await;
        assert!(queues.acquire("harness_turn", &b).now_or_never().is_none());
        drop(archiving);
        assert!(queues.acquire("harness_turn", &b).now_or_never().is_some());
    }
}
