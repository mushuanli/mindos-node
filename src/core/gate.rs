//! Mutual exclusion between the file APIs and the process API.
//!
//! File work takes a shared guard; starting a command takes the exclusive
//! guard and keeps it until the command has been reaped. Every failure to
//! acquire a guard is reported as `EBUSY`, never by blocking a request.
//!
//! If a command cannot be reaped, its process ownership is unknown, so the
//! gate is *poisoned* instead of releasing the guard: every later file or
//! process request fails closed with `EIO` until the operator restarts. This
//! keeps the safety of never reopening the exports without leaking a lock.

use crate::core::error::Error;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

/// Cloneable handle to the process-wide file/process gate.
#[derive(Clone, Default)]
pub struct FileGate {
    inner: Arc<RwLock<()>>,
    poisoned: Arc<AtomicBool>,
    activity: Arc<Mutex<Activity>>,
}

#[derive(Default, Debug)]
struct Activity {
    count: usize,
    guard: Option<OwnedRwLockWriteGuard<()>>,
}

/// Native executions share one exclusive ownership group, never file API access.
#[derive(Debug)]
pub struct ActivityPermit(Arc<Mutex<Activity>>);
impl Drop for ActivityPermit {
    fn drop(&mut self) {
        let mut activity = self.0.lock().unwrap();
        activity.count -= 1;
        if activity.count == 0 {
            activity.guard = None;
        }
    }
}

impl FileGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Shared access for reads, listings and revisions.
    pub fn shared(&self) -> Result<OwnedRwLockReadGuard<()>, Error> {
        self.check()?;
        self.inner
            .clone()
            .try_read_owned()
            .map_err(|_| Error::busy())
    }

    /// Exclusive access while a command owns the exports.
    pub fn exclusive(&self) -> Result<OwnedRwLockWriteGuard<()>, Error> {
        self.check()?;
        self.inner
            .clone()
            .try_write_owned()
            .map_err(|_| Error::busy())
    }

    /// Admit another supervised execution while retaining exclusion from file/process APIs.
    pub fn activity(&self) -> Result<ActivityPermit, Error> {
        let mut activity = self.activity.lock().map_err(|_| Error::internal())?;
        self.check()?;
        if activity.count == 0 {
            activity.guard = Some(self.exclusive()?);
        }
        activity.count += 1;
        Ok(ActivityPermit(self.activity.clone()))
    }

    /// Permanently close the gate after an unreaped command.
    pub fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    /// Wait for every in-flight file operation; used only during shutdown.
    pub async fn drain(&self) -> OwnedRwLockWriteGuard<()> {
        self.inner.clone().write_owned().await
    }

    fn check(&self) -> Result<(), Error> {
        if self.is_poisoned() {
            return Err(Error::unavailable());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn execution_permits_exclude_file_apis_until_the_last_owner_leaves() {
        let gate = FileGate::new();
        let read = gate.shared().unwrap();
        assert_eq!(gate.activity().unwrap_err().code, "EBUSY");
        drop(read);
        let a = gate.activity().unwrap();
        let b = gate.activity().unwrap();
        assert!(gate.shared().is_err());
        assert!(gate.exclusive().is_err());
        drop(a);
        assert!(gate.shared().is_err());
        drop(b);
        assert!(gate.shared().is_ok());
        let write = gate.exclusive().unwrap();
        assert_eq!(gate.activity().unwrap_err().code, "EBUSY");
        drop(write);
        assert!(gate.activity().is_ok());
    }
    #[test]
    fn poisoning_rejects_new_owners_even_when_a_group_is_already_running() {
        let gate = FileGate::new();
        let permit = gate.activity().unwrap();
        gate.poison();
        assert_eq!(gate.activity().unwrap_err().code, "EIO");
        drop(permit);
        assert_eq!(gate.shared().unwrap_err().code, "EIO");
    }
}
