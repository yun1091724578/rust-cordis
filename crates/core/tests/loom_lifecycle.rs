#![cfg(not(all(windows, target_env = "gnu")))]
//! `loom` concurrency model checks for hand-written protocol abstractions of
//! `cordis-core`'s concurrency primitives:
//!
//! IMPORTANT: This file tests hand-written abstractions (`LoomCancelToken`, `FiberModel`,
//! `StoreModel`) and does NOT compile `crates/core/src/{fiber,store,cancel}.rs` under `loom`.
//! Changes to production code are not automatically detected here; treat these tests as
//! executable protocol specifications, not implementation guarantees.
//!
//! 1. `LoomCancelToken`: multi-waiter cancellation (no lost wakeup / deadlock).
//! 2. `FiberModel`: `settle_inner` / `load` vs `dispose` (absorbing `Disposed` state,
//!    effect gate closure, zero leaked effects).
//! 3. `FiberModel`: `unload` vs concurrent `restart` + `dispose` convergence.
//! 4. `StoreModel`: `unregister_fiber` vs `runtime_for` + `register_fiber` (no orphaned fibers,
//!    strict `runtimes -> runtime.fibers` lock order).

use cordis_core::FiberState;
use loom::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;

const UID_DISPOSED: u64 = u64::MAX;

/// Bound preemptions to 2 by default (unless `LOOM_MAX_PREEMPTIONS` is set) so
/// debug CI runs stay fast and deterministic without state-space explosion.
fn run_model<F>(f: F)
where
    F: Fn() + Sync + Send + 'static,
{
    let mut builder = loom::model::Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(2);
    }
    builder.check(f);
}

/// Loom model of `CancelToken` (matching `crates/core/src/cancel.rs`).
struct LoomCancelToken {
    cancelled: AtomicBool,
    gate: Mutex<()>,
    cvar: Condvar,
}

impl LoomCancelToken {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            gate: Mutex::new(()),
            cvar: Condvar::new(),
        }
    }

    fn cancel(&self) {
        let _g = self.gate.lock().unwrap();
        self.cancelled.store(true, Ordering::SeqCst);
        self.cvar.notify_all();
    }

    fn wait_cancelled(&self) {
        let mut g = self.gate.lock().unwrap();
        while !self.cancelled.load(Ordering::SeqCst) {
            g = self.cvar.wait(g).unwrap();
        }
    }
}

/// 1. Two concurrent waiters racing with `cancel()` must never lose wakeup or deadlock.
#[test]
fn loom_cancel_token_multi_waiter_no_lost_wakeup() {
    run_model(|| {
        let token = Arc::new(LoomCancelToken::new());
        let woke = Arc::new(AtomicUsize::new(0));

        let t1 = token.clone();
        let w1 = woke.clone();
        let h1 = thread::spawn(move || {
            t1.wait_cancelled();
            w1.fetch_add(1, Ordering::SeqCst);
        });

        let t2 = token.clone();
        let w2 = woke.clone();
        let h2 = thread::spawn(move || {
            t2.wait_cancelled();
            w2.fetch_add(1, Ordering::SeqCst);
        });

        token.cancel();
        h1.join().unwrap();
        h2.join().unwrap();
        assert_eq!(woke.load(Ordering::SeqCst), 2);
    });
}

/// Loom model of `Fiber`'s core lifecycle fields and state-machine transitions
/// (`uid`, `state` CAS invariant, `effect_gate`, `desired`, `running`, `driver`, `disposables`).
struct EffectGateModel {
    closed: bool,
    in_flight: usize,
}

struct FiberModel {
    uid: AtomicU64,
    state: AtomicU8,
    effect_gate: Mutex<EffectGateModel>,
    effect_cvar: Condvar,
    desired: Mutex<Option<u64>>,
    running: Mutex<Option<u64>>,
    driver: Mutex<()>,
    disposables: Mutex<usize>,
    disposed_effects: AtomicUsize,
    illegal_resurrection: AtomicBool,
}

impl FiberModel {
    fn new() -> Self {
        Self {
            uid: AtomicU64::new(1),
            state: AtomicU8::new(FiberState::Pending as u8),
            effect_gate: Mutex::new(EffectGateModel {
                closed: false,
                in_flight: 0,
            }),
            effect_cvar: Condvar::new(),
            desired: Mutex::new(Some(1)),
            running: Mutex::new(None),
            driver: Mutex::new(()),
            disposables: Mutex::new(0),
            disposed_effects: AtomicUsize::new(0),
            illegal_resurrection: AtomicBool::new(false),
        }
    }

    fn is_disposed(&self) -> bool {
        self.uid.load(Ordering::SeqCst) == UID_DISPOSED
    }

    fn state(&self) -> FiberState {
        match self.state.load(Ordering::SeqCst) {
            0 => FiberState::Pending,
            1 => FiberState::Loading,
            2 => FiberState::Active,
            3 => FiberState::Unloading,
            4 => FiberState::Failed,
            _ => FiberState::Disposed,
        }
    }

    /// Mirrors `Fiber::set_state` CAS absorbing-Disposed invariant.
    fn set_state(&self, next: FiberState) -> bool {
        let mut cur = self.state.load(Ordering::SeqCst);
        loop {
            if cur == next as u8 {
                return false;
            }
            if self.is_disposed() && !matches!(next, FiberState::Disposed | FiberState::Unloading) {
                return false;
            }
            match self
                .state
                .compare_exchange(cur, next as u8, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => {
                    // Check the exact pre-CAS state `cur` atomically replaced by this transition
                    // (a separate post-CAS atomic load could observe a subsequent `dispose()`
                    // that legally happened *after* this CAS).
                    if cur == FiberState::Disposed as u8
                        && !matches!(next, FiberState::Disposed | FiberState::Unloading)
                    {
                        self.illegal_resurrection.store(true, Ordering::SeqCst);
                    }
                    return true;
                }
                Err(actual) => cur = actual,
            }
        }
    }

    fn open_effects(&self) -> bool {
        let mut gate = self.effect_gate.lock().unwrap();
        if self.is_disposed() {
            return false;
        }
        gate.closed = false;
        true
    }

    fn register_effect(&self) -> bool {
        {
            let mut gate = self.effect_gate.lock().unwrap();
            if gate.closed {
                return false;
            }
            gate.in_flight += 1;
        }
        *self.disposables.lock().unwrap() += 1;
        {
            let mut gate = self.effect_gate.lock().unwrap();
            gate.in_flight -= 1;
            if gate.in_flight == 0 {
                self.effect_cvar.notify_all();
            }
        }
        true
    }

    fn close_effects(&self) {
        let mut gate = self.effect_gate.lock().unwrap();
        gate.closed = true;
        while gate.in_flight > 0 {
            gate = self.effect_cvar.wait(gate).unwrap();
        }
    }

    fn drain_effects(&self) {
        let count = {
            let mut d = self.disposables.lock().unwrap();
            let n = *d;
            *d = 0;
            n
        };
        self.disposed_effects.fetch_add(count, Ordering::SeqCst);
    }

    fn settle_inner(&self) {
        let _guard = self.driver.lock().unwrap();
        loop {
            if self.is_disposed() {
                self.unload();
                return;
            }
            let target = *self.desired.lock().unwrap();
            let current = *self.running.lock().unwrap();
            if target == current {
                return;
            }
            if current.is_some() {
                self.unload();
                continue;
            }
            if let Some(epoch) = target {
                self.load(epoch);
            }
        }
    }

    fn load(&self, epoch: u64) {
        if !self.open_effects() {
            return;
        }
        self.set_state(FiberState::Loading);
        if self.is_disposed() {
            return;
        }
        // Plugin apply registers an effect.
        let _ = self.register_effect();
        if self.is_disposed() {
            return;
        }
        *self.running.lock().unwrap() = Some(epoch);
        self.set_state(FiberState::Active);
    }

    fn unload(&self) {
        self.set_state(FiberState::Unloading);
        self.close_effects();
        self.drain_effects();
        *self.running.lock().unwrap() = None;
        if self.is_disposed() {
            self.set_state(FiberState::Disposed);
        } else {
            self.open_effects();
            self.set_state(FiberState::Pending);
        }
    }

    fn dispose(&self) {
        if self.uid.swap(UID_DISPOSED, Ordering::SeqCst) != UID_DISPOSED {
            self.effect_gate.lock().unwrap().closed = true;
            *self.desired.lock().unwrap() = None;
            self.set_state(FiberState::Disposed);
        }
        self.settle_inner();
    }

    fn restart(&self, next_epoch: u64) {
        if self.is_disposed() {
            return;
        }
        *self.desired.lock().unwrap() = Some(next_epoch);
        self.settle_inner();
    }
}

/// 2. Exhaustively check `settle_inner` / `load` racing against `dispose`.
#[test]
fn loom_fiber_settle_load_vs_dispose_absorbing_invariant() {
    run_model(|| {
        let fiber = Arc::new(FiberModel::new());

        let f_load = fiber.clone();
        let h_load = thread::spawn(move || {
            f_load.settle_inner();
        });

        let f_dispose = fiber.clone();
        let h_dispose = thread::spawn(move || {
            f_dispose.dispose();
        });

        h_load.join().unwrap();
        h_dispose.join().unwrap();

        assert!(fiber.is_disposed());
        assert_eq!(fiber.state(), FiberState::Disposed);
        assert!(
            !fiber.illegal_resurrection.load(Ordering::SeqCst),
            "disposed fiber illegally transitioned back to a live state"
        );
        assert_eq!(
            *fiber.disposables.lock().unwrap(),
            0,
            "all registered effects must be drained on dispose"
        );
    });
}

/// 3. Exhaustively check `unload` / `restart` racing against `dispose`.
#[test]
fn loom_fiber_unload_vs_restart_and_dispose() {
    run_model(|| {
        let fiber = Arc::new(FiberModel::new());
        // Start from Active state with 1 registered effect.
        fiber.settle_inner();
        assert_eq!(fiber.state(), FiberState::Active);

        let f_restart = fiber.clone();
        let h_restart = thread::spawn(move || {
            f_restart.restart(2);
        });

        let f_dispose = fiber.clone();
        let h_dispose = thread::spawn(move || {
            f_dispose.dispose();
        });

        h_restart.join().unwrap();
        h_dispose.join().unwrap();

        assert!(fiber.is_disposed());
        assert_eq!(fiber.state(), FiberState::Disposed);
        assert!(!fiber.illegal_resurrection.load(Ordering::SeqCst));
        assert_eq!(*fiber.disposables.lock().unwrap(), 0);
        assert!(fiber.disposed_effects.load(Ordering::SeqCst) >= 1);
    });
}

/// 4. Exhaustively check `Store::unregister_fiber` vs `runtime_for` + `register_fiber`
///    under unified lock order `runtimes -> runtime.fibers`.
#[test]
fn loom_store_unregister_vs_register_fiber_no_orphan() {
    struct RuntimeModel {
        fibers: Mutex<Vec<u64>>,
    }
    struct StoreModel {
        runtimes: Mutex<Vec<Arc<RuntimeModel>>>,
    }
    impl StoreModel {
        fn runtime_for(&self) -> Arc<RuntimeModel> {
            let mut runtimes = self.runtimes.lock().unwrap();
            if let Some(existing) = runtimes.first() {
                return existing.clone();
            }
            let rt = Arc::new(RuntimeModel {
                fibers: Mutex::new(Vec::new()),
            });
            runtimes.push(rt.clone());
            rt
        }
        fn register_fiber(&self, rt: &Arc<RuntimeModel>, id: u64) {
            let mut runtimes = self.runtimes.lock().unwrap();
            if !runtimes.iter().any(|r| Arc::ptr_eq(r, rt)) {
                runtimes.push(rt.clone());
            }
            rt.fibers.lock().unwrap().push(id);
        }
        fn unregister_fiber(&self, rt: &Arc<RuntimeModel>, id: u64) {
            let mut runtimes = self.runtimes.lock().unwrap();
            let empty = {
                let mut fibers = rt.fibers.lock().unwrap();
                fibers.retain(|&f| f != id);
                fibers.is_empty()
            };
            if empty {
                runtimes.retain(|r| !Arc::ptr_eq(r, rt));
            }
        }
    }

    run_model(|| {
        let store = Arc::new(StoreModel {
            runtimes: Mutex::new(Vec::new()),
        });
        let rt1 = store.runtime_for();
        store.register_fiber(&rt1, 1);

        let s_unreg = store.clone();
        let rt_unreg = rt1.clone();
        let h_unreg = thread::spawn(move || {
            s_unreg.unregister_fiber(&rt_unreg, 1);
        });

        let s_mount = store.clone();
        let h_mount = thread::spawn(move || {
            let rt2 = s_mount.runtime_for();
            s_mount.register_fiber(&rt2, 2);
        });

        h_unreg.join().unwrap();
        h_mount.join().unwrap();

        let runtimes = store.runtimes.lock().unwrap();
        let found = runtimes
            .iter()
            .any(|r| r.fibers.lock().unwrap().contains(&2));
        assert!(
            found,
            "newly mounted fiber #2 was orphaned from Store::runtimes!"
        );
    });
}
