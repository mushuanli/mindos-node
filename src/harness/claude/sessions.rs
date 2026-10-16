//! Only a session's own initialization may delay access to that session.
use super::process::Process;
use crate::core::error::Error;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::Mutex as AsyncMutex;

type Slot = Arc<AsyncMutex<Option<Arc<Process>>>>;
#[derive(Default)]
pub(super) struct Sessions(Mutex<BTreeMap<String, Slot>>);

impl Sessions {
    pub fn slot(&self, id: &str) -> Result<Slot, Error> {
        let mut sessions = self.0.lock().unwrap();
        sessions.retain(|_, slot| {
            Arc::strong_count(slot) > 1
                || slot
                    .try_lock()
                    .map_or(true, |process| process.as_ref().is_some_and(|p| p.alive()))
        });
        if !sessions.contains_key(id) && sessions.len() >= 16 {
            return Err(Error::too_many("CLAUDE_SESSION_LIMIT"));
        }
        Ok(sessions.entry(id.into()).or_default().clone())
    }
    pub async fn get(&self, id: &str) -> Option<Arc<Process>> {
        let slot = self.0.lock().unwrap().get(id).cloned()?;
        let process = slot.lock().await;
        process.as_ref().filter(|p| p.alive()).cloned()
    }
    pub fn live(&self) -> Vec<Arc<Process>> {
        let slots = self.0.lock().unwrap().values().cloned().collect::<Vec<_>>();
        slots
            .iter()
            .filter_map(|slot| {
                slot.try_lock()
                    .ok()
                    .and_then(|process| process.as_ref().filter(|p| p.alive()).cloned())
            })
            .collect()
    }
    pub async fn drain(&self) {
        let slots = std::mem::take(&mut *self.0.lock().unwrap());
        let mut processes = Vec::new();
        for slot in slots.into_values() {
            if let Some(process) = slot.lock().await.as_ref() {
                process.close();
                processes.push(process.clone());
            }
        }
        futures_util::future::join_all(processes.iter().map(|p| p.drain())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn an_initializing_session_does_not_lock_other_slots_or_live_views() {
        let sessions = Sessions::default();
        let slow = sessions.slot("slow").unwrap();
        let _pending = slow.lock().await;
        let other = sessions.slot("other").unwrap();
        assert!(other.try_lock().is_ok());
        assert!(sessions.live().is_empty());
        assert!(sessions.get("missing").await.is_none());
        assert!(Arc::ptr_eq(&slow, &sessions.slot("slow").unwrap()));
    }
    #[test]
    fn capacity_counts_initializing_slots_and_reclaims_unused_reservations() {
        let sessions = Sessions::default();
        let slots = (0..16)
            .map(|id| sessions.slot(&id.to_string()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            sessions.slot("overflow").err().unwrap().code,
            "CLAUDE_SESSION_LIMIT"
        );
        drop(slots);
        assert!(sessions.slot("next").is_ok());
    }
}
