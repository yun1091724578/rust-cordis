//! Multi-version canary router with session affinity and graceful drain.
//!
//! Designed for single-host multi-session concurrent AI Agents running on top
//! of `rust-cordis`:
//! - Coexisting T2 plugin versions register under distinct service keys
//!   (e.g. `solver.v1@1`, `solver.v2@1`) instead of overwriting a live label;
//! - Each session pins to one version on first acquire so multi-turn agent
//!   runs never drift mid-conversation;
//! - Traffic weights allow deterministic canary splits across sessions;
//! - `begin_drain(version)` + `wait_drained(version)` lets new sessions move
//!   immediately to the promoted variant while existing in-flight sessions
//!   finish cleanly without `host_lifecycle` interruption.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

#[derive(Clone, Debug)]
struct VersionSlot {
    service_key: Arc<str>,
    weight: u32,
    draining: bool,
    /// Once a draining version reaches `in_flight == 0`, it is sealed (`drained = true`)
    /// so previously pinned sessions cannot resurrect it before `unregister`.
    drained: bool,
    in_flight: usize,
}

#[derive(Default)]
struct RouterState {
    versions: BTreeMap<String, VersionSlot>,
    /// Session affinity: `session_id -> version`.
    pinned: HashMap<String, String>,
}

/// Thread-safe multi-version router shared across concurrent agent sessions.
#[derive(Clone, Default)]
pub struct VersionRouter {
    inner: Arc<RouterInner>,
}

#[derive(Default)]
struct RouterInner {
    state: Mutex<RouterState>,
    drained_notify: Notify,
}

/// RAII lease tracking one active in-flight call/turn on a routed version.
///
/// Dropping the lease (including on cancellation or panic) decrements the
/// version's `in_flight` counter and wakes any `wait_drained` waiters once
/// `in_flight` reaches zero.
pub struct VersionLease {
    inner: Arc<RouterInner>,
    version: String,
    service_key: Arc<str>,
    session_id: String,
    release_on_drop: bool,
}

impl VersionLease {
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn service_key(&self) -> &str {
        &self.service_key
    }

    /// Automatically remove the session's affinity pin when this lease drops
    /// (prevents `pinned` map growth for single-turn / ephemeral sessions).
    pub fn release_session_on_drop(mut self) -> Self {
        self.release_on_drop = true;
        self
    }
}

impl Drop for VersionLease {
    fn drop(&mut self) {
        let notify = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.release_on_drop
                && state
                    .pinned
                    .get(&self.session_id)
                    .is_some_and(|pinned| pinned == &self.version)
            {
                state.pinned.remove(&self.session_id);
            }
            if let Some(slot) = state.versions.get_mut(&self.version) {
                slot.in_flight = slot.in_flight.saturating_sub(1);
                if slot.in_flight == 0 && slot.draining {
                    slot.drained = true;
                }
                slot.in_flight == 0
            } else {
                true
            }
        };
        if notify {
            self.inner.drained_notify.notify_waiters();
        }
    }
}

impl VersionRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or update) a mounted plugin version with its service key and canary weight.
    pub fn register(
        &self,
        version: impl Into<String>,
        service_key: impl Into<Arc<str>>,
        weight: u32,
    ) {
        let woke_draining = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let version = version.into();
            let service_key = service_key.into();
            let mut cleared = false;
            state
                .versions
                .entry(version)
                .and_modify(|slot| {
                    slot.service_key = service_key.clone();
                    slot.weight = weight;
                    if slot.draining {
                        cleared = true;
                    }
                    slot.draining = false;
                    slot.drained = false;
                })
                .or_insert(VersionSlot {
                    service_key,
                    weight,
                    draining: false,
                    drained: false,
                    in_flight: 0,
                });
            cleared
        };
        if woke_draining {
            self.inner.drained_notify.notify_waiters();
        }
    }

    /// Update a version's canary weight (0 stops routing new sessions).
    pub fn set_weight(&self, version: &str, weight: u32) -> Result<(), String> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let slot = state
            .versions
            .get_mut(version)
            .ok_or_else(|| format!("unknown version {version:?}"))?;
        if slot.draining && weight > 0 {
            return Err(format!(
                "version {version:?} is draining; cannot restore positive weight"
            ));
        }
        slot.weight = weight;
        Ok(())
    }

    /// Promote `target` to receive 100% of new sessions (sets all other versions to weight 0).
    pub fn promote(&self, target: &str) -> Result<(), String> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.versions.contains_key(target) {
            return Err(format!("cannot promote unknown version {target:?}"));
        }
        if state.versions[target].draining {
            return Err(format!("cannot promote draining version {target:?}"));
        }
        for (name, slot) in &mut state.versions {
            slot.weight = if name == target { 100 } else { 0 };
        }
        Ok(())
    }

    /// Mark a version as draining: no new sessions will bind to it, while
    /// already-pinned sessions may finish their in-flight work.
    pub fn begin_drain(&self, version: &str) -> Result<(), String> {
        let notify = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let slot = state
                .versions
                .get_mut(version)
                .ok_or_else(|| format!("cannot drain unknown version {version:?}"))?;
            slot.weight = 0;
            slot.draining = true;
            if slot.in_flight == 0 {
                slot.drained = true;
            }
            slot.in_flight == 0
        };
        if notify {
            self.inner.drained_notify.notify_waiters();
        }
        Ok(())
    }

    /// Wait until a draining version has zero active in-flight leases.
    pub async fn wait_drained(&self, version: &str) -> Result<(), String> {
        loop {
            // Register the waiter BEFORE acquiring `state` to prevent lost wakeups when
            // the last lease drops or `register()` clears `draining` between unlock and `.await`.
            let notified = self.inner.drained_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let done = {
                let mut state = self
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let slot = state
                    .versions
                    .get_mut(version)
                    .ok_or_else(|| format!("unknown version {version:?}"))?;
                if !slot.draining {
                    return Err(format!(
                        "version {version:?} is not draining; call begin_drain first"
                    ));
                }
                if slot.in_flight == 0 {
                    slot.drained = true;
                    true
                } else {
                    false
                }
            };
            if done {
                return Ok(());
            }
            notified.await;
        }
    }

    /// Remove a drained version from the router table after its plugin is unloaded.
    pub fn unregister(&self, version: &str) -> Result<(), String> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(slot) = state.versions.get(version) else {
            return Err(format!("unknown version {version:?}"));
        };
        if slot.in_flight > 0 {
            return Err(format!(
                "version {version:?} still has {} in-flight leases",
                slot.in_flight
            ));
        }
        state.versions.remove(version);
        state.pinned.retain(|_, pinned| pinned != version);
        Ok(())
    }

    /// Release session affinity once a session ends.
    pub fn release_session(&self, session_id: &str) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.pinned.remove(session_id);
    }

    /// Acquire a lease for `session_id`: reuses the session's pinned version if
    /// still registered, otherwise picks deterministically by canary weight.
    pub fn acquire(&self, session_id: &str) -> Result<VersionLease, String> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let chosen = if let Some(pinned) = state.pinned.get(session_id).cloned() {
            if state
                .versions
                .get(&pinned)
                .is_some_and(|slot| !slot.drained)
            {
                pinned
            } else {
                let picked = pick_weighted(&state.versions, session_id)?;
                state.pinned.insert(session_id.to_string(), picked.clone());
                picked
            }
        } else {
            let picked = pick_weighted(&state.versions, session_id)?;
            state.pinned.insert(session_id.to_string(), picked.clone());
            picked
        };
        let slot = state
            .versions
            .get_mut(&chosen)
            .ok_or_else(|| format!("selected version {chosen:?} disappeared"))?;
        slot.in_flight += 1;
        Ok(VersionLease {
            inner: self.inner.clone(),
            version: chosen,
            service_key: slot.service_key.clone(),
            session_id: session_id.to_string(),
            release_on_drop: false,
        })
    }

    /// Acquire a lease explicitly targeting a specific version (useful for pinned canary probes).
    pub fn acquire_version(&self, session_id: &str, version: &str) -> Result<VersionLease, String> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let slot = state
            .versions
            .get_mut(version)
            .ok_or_else(|| format!("unknown version {version:?}"))?;
        if slot.draining {
            return Err(format!("version {version:?} is draining"));
        }
        slot.in_flight += 1;
        let service_key = slot.service_key.clone();
        state
            .pinned
            .insert(session_id.to_string(), version.to_string());
        Ok(VersionLease {
            inner: self.inner.clone(),
            version: version.to_string(),
            service_key,
            session_id: session_id.to_string(),
            release_on_drop: false,
        })
    }

    pub fn pinned_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pinned
            .len()
    }

    pub fn in_flight(&self, version: &str) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .versions
            .get(version)
            .map(|slot| slot.in_flight)
            .unwrap_or(0)
    }
}

fn pick_weighted(
    versions: &BTreeMap<String, VersionSlot>,
    session_id: &str,
) -> Result<String, String> {
    let total: u64 = versions
        .values()
        .filter(|slot| !slot.draining && slot.weight > 0)
        .map(|slot| slot.weight as u64)
        .sum();
    if total == 0 {
        return Err("no active non-draining versions with positive weight".to_string());
    }
    let mut bucket = cordis_core::fnv(session_id.as_bytes()) % total;
    for (name, slot) in versions {
        if slot.draining || slot.weight == 0 {
            continue;
        }
        let w = slot.weight as u64;
        if bucket < w {
            return Ok(name.clone());
        }
        bucket -= w;
    }
    unreachable!("bucket is strictly less than total weight")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_affinity_and_graceful_drain_do_not_break_inflight() {
        let router = VersionRouter::new();
        router.register("v1", "solver.v1@1", 100);
        router.register("v2", "solver.v2@1", 0);

        let lease1 = router.acquire("sess-old").unwrap();
        assert_eq!(lease1.version(), "v1");
        assert_eq!(router.in_flight("v1"), 1);

        router.promote("v2").unwrap();
        router.begin_drain("v1").unwrap();

        // New sessions immediately move to v2.
        let lease_new = router.acquire("sess-new").unwrap();
        assert_eq!(lease_new.version(), "v2");

        // Already-pinned session keeps affinity to v1 while draining.
        let lease_old_turn2 = router.acquire("sess-old").unwrap();
        assert_eq!(lease_old_turn2.version(), "v1");
        assert_eq!(router.in_flight("v1"), 2);

        // Drain waiter blocks until both v1 leases drop.
        let waiter = {
            let router = router.clone();
            tokio::spawn(async move { router.wait_drained("v1").await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "must not finish while in_flight > 0");
        drop(lease1);
        tokio::task::yield_now().await;
        assert!(
            !waiter.is_finished(),
            "must not finish while in_flight == 1"
        );
        drop(lease_old_turn2);
        waiter.await.unwrap().unwrap();

        router.unregister("v1").unwrap();
        assert_eq!(router.in_flight("v1"), 0);
        drop(lease_new);
    }

    #[test]
    fn canary_split_is_deterministic_per_session() {
        let router = VersionRouter::new();
        router.register("v1", "solver.v1@1", 50);
        router.register("v2", "solver.v2@1", 50);
        let mut v1 = 0usize;
        let mut v2 = 0usize;
        for i in 0..40usize {
            let sid = format!("sess-{i:02}");
            let first = router.acquire(&sid).unwrap();
            let second = router.acquire(&sid).unwrap();
            assert_eq!(first.version(), second.version(), "affinity must hold");
            if first.version() == "v1" {
                v1 += 1;
            } else {
                v2 += 1;
            }
        }
        assert!(
            v1 > 0 && v2 > 0,
            "50/50 split must hit both: v1={v1} v2={v2}"
        );
    }

    #[test]
    fn release_session_on_drop_cleans_up_pinned_entry() {
        let router = VersionRouter::new();
        router.register("v1", "solver.v1@1", 100);
        {
            let lease = router
                .acquire("ephemeral-1")
                .unwrap()
                .release_session_on_drop();
            assert_eq!(lease.version(), "v1");
            assert_eq!(router.pinned_count(), 1);
        }
        assert_eq!(router.pinned_count(), 0);
        assert_eq!(router.in_flight("v1"), 0);
    }

    #[tokio::test]
    async fn r3_p1_wait_drained_no_lost_wakeup_resurrection_or_stale_unpin() {
        // 1. After drain completes (in_flight reaches 0), an old pinned session MUST NOT
        //    resurrect the drained version before `unregister`.
        let router = VersionRouter::new();
        router.register("v1", "solver.v1@1", 100);
        router.register("v2", "solver.v2@1", 0);

        let lease1 = router.acquire("sess-old").unwrap();
        assert_eq!(lease1.version(), "v1");
        router.promote("v2").unwrap();
        router.begin_drain("v1").unwrap();
        drop(lease1);

        tokio::time::timeout(std::time::Duration::from_secs(2), router.wait_drained("v1"))
            .await
            .expect("wait_drained must finish")
            .unwrap();

        // Even BEFORE `unregister("v1")`, `sess-old` must migrate to v2 instead of resurrecting v1.
        let lease_after = router.acquire("sess-old").unwrap();
        assert_eq!(
            lease_after.version(),
            "v2",
            "drain 完成后旧 pinned 会话不得再把已排空的 v1 复活"
        );
        assert_eq!(router.in_flight("v1"), 0);
        router.unregister("v1").unwrap();
        drop(lease_after);

        // 2. Re-registering a draining version while `wait_drained` is sleeping must wake the waiter.
        let router2 = VersionRouter::new();
        router2.register("v1", "solver.v1@1", 100);
        let held = router2.acquire("s1").unwrap();
        router2.begin_drain("v1").unwrap();
        let r2_wait = router2.clone();
        let waiter = tokio::spawn(async move { r2_wait.wait_drained("v1").await });
        tokio::task::yield_now().await;
        // Re-register clears draining; waiter must wake and return Err instead of hanging.
        router2.register("v1", "solver.v1@1", 100);
        let res = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("register() clearing draining must wake wait_drained")
            .unwrap();
        assert!(res.is_err(), "cleared draining must surface Err: {res:?}");
        drop(held);

        // 3. Stale `release_session_on_drop` lease for v1 must NOT wipe out a newer v2 pin.
        let router3 = VersionRouter::new();
        router3.register("v1", "solver.v1@1", 100);
        router3.register("v2", "solver.v2@1", 0);
        let old_lease = router3
            .acquire_version("shared-sess", "v1")
            .unwrap()
            .release_session_on_drop();
        let new_lease = router3.acquire_version("shared-sess", "v2").unwrap();
        drop(old_lease);
        assert_eq!(
            router3.pinned_count(),
            1,
            "旧版本租约释放不得误删同 session 已迁移到新版本的 pin"
        );
        let next = router3.acquire("shared-sess").unwrap();
        assert_eq!(next.version(), "v2");
        drop(next);
        drop(new_lease);
    }
}
