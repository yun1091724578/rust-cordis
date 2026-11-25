//! Portable, zero-dependency exhaustive DFS interleaving model checks for hand-written
//! protocol abstractions of `cordis-core`'s concurrency primitives.
//!
//! IMPORTANT: This file tests hand-written state-machine abstractions (`CancelModel`,
//! `FiberLoadDisposeModel`, `StoreUnregModel`), NOT the production implementations in
//! `fiber.rs` / `store.rs` / `cancel.rs`. Changes to production code are not automatically
//! detected here; treat these tests as executable design specifications, not implementation
//! guarantees.
//!
//! Complements `tests/loom_lifecycle.rs` (which runs `loom::model` on Linux / macOS / Windows MSVC)
//! by running on all platforms including `windows-gnu`.

use cordis_core::FiberState;

/// Exhaustively explores all interleavings of `n` actors, where each actor executes
/// a sequence of atomic steps until completion (or blocked). Detects deadlocks and
/// asserts `check_final` on every terminal schedule.
fn explore_all_interleavings<S, F, C>(
    initial: S,
    actor_count: usize,
    step: &F,
    check_final: &C,
) -> usize
where
    S: Clone,
    F: Fn(&mut S, usize) -> StepResult,
    C: Fn(&S),
{
    fn dfs<S, F, C>(
        state: S,
        actor_count: usize,
        step: &F,
        check_final: &C,
        completed_schedules: &mut usize,
    ) where
        S: Clone,
        F: Fn(&mut S, usize) -> StepResult,
        C: Fn(&S),
    {
        let mut any_runnable = false;
        let mut all_done = true;

        for actor in 0..actor_count {
            let mut next = state.clone();
            match step(&mut next, actor) {
                StepResult::Stepped => {
                    any_runnable = true;
                    all_done = false;
                    dfs(next, actor_count, step, check_final, completed_schedules);
                }
                StepResult::Blocked => {
                    all_done = false;
                }
                StepResult::Done => {}
            }
        }

        if all_done {
            check_final(&state);
            *completed_schedules += 1;
        } else if !any_runnable {
            panic!("deadlock detected: some actors are blocked and none are runnable");
        }
    }

    let mut total = 0;
    dfs(initial, actor_count, step, check_final, &mut total);
    assert!(total > 0, "model must explore at least one schedule");
    total
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StepResult {
    Stepped,
    Blocked,
    Done,
}

// ------------------------------------------------------------------ 1. CancelToken

#[derive(Clone)]
struct CancelModel {
    cancelled: bool,
    waiter_registered: [bool; 2],
    waiter_woken: [bool; 2],
    pc: [u8; 3], // actor 0 = cancel(), actor 1..=2 = cancelled()
}

/// Exhaustively proves `CancelToken` with `enable()`-before-check has 0 lost wakeups
/// across all interleavings of 2 concurrent waiters + 1 canceller.
#[test]
fn dfs_cancel_token_multi_waiter_no_lost_wakeup() {
    let init = CancelModel {
        cancelled: false,
        waiter_registered: [false; 2],
        waiter_woken: [false; 2],
        pc: [0; 3],
    };
    let schedules = explore_all_interleavings(
        init,
        3,
        &|s, actor| {
            if actor == 0 {
                match s.pc[0] {
                    0 => {
                        s.cancelled = true;
                        s.pc[0] = 1;
                        StepResult::Stepped
                    }
                    1 => {
                        // notify_waiters(): wake all registered waiters.
                        for i in 0..2 {
                            if s.waiter_registered[i] {
                                s.waiter_woken[i] = true;
                            }
                        }
                        s.pc[0] = 2;
                        StepResult::Stepped
                    }
                    _ => StepResult::Done,
                }
            } else {
                let w = actor - 1;
                match s.pc[actor] {
                    0 => {
                        // notified.as_mut().enable() BEFORE checking is_cancelled()
                        s.waiter_registered[w] = true;
                        s.pc[actor] = 1;
                        StepResult::Stepped
                    }
                    1 => {
                        if s.cancelled {
                            s.pc[actor] = 3;
                        } else {
                            s.pc[actor] = 2;
                        }
                        StepResult::Stepped
                    }
                    2 => {
                        if s.waiter_woken[w] {
                            s.pc[actor] = 3;
                            StepResult::Stepped
                        } else {
                            StepResult::Blocked
                        }
                    }
                    _ => StepResult::Done,
                }
            }
        },
        &|s| {
            assert!(s.cancelled);
            assert_eq!(s.pc, [2, 3, 3]);
        },
    );
    assert!(
        schedules >= 10,
        "expected thorough schedule exploration, got {schedules}"
    );
}

// ------------------------------------------------------------------ 2. Fiber settle/load vs dispose

#[derive(Clone)]
struct FiberLoadDisposeModel {
    disposed: bool,
    state: FiberState,
    saw_disposed_state: bool,
    illegal_resurrection: bool,
    effect_gate_closed: bool,
    effects_registered: usize,
    effects_disposed: usize,
    driver_held_by: Option<usize>,
    pc: [u8; 2], // actor 0 = settle_inner/load, actor 1 = dispose
}

impl FiberLoadDisposeModel {
    fn set_state(&mut self, next: FiberState) {
        if self.state == next {
            return;
        }
        if self.disposed && !matches!(next, FiberState::Disposed | FiberState::Unloading) {
            return;
        }
        self.state = next;
        if next == FiberState::Disposed {
            self.saw_disposed_state = true;
        } else if self.saw_disposed_state
            && !matches!(next, FiberState::Disposed | FiberState::Unloading)
        {
            self.illegal_resurrection = true;
        }
    }
}

/// Exhaustively explores all interleavings of `settle_inner` (`load`) vs `dispose`.
#[test]
fn dfs_fiber_settle_load_vs_dispose_absorbing_invariant() {
    let init = FiberLoadDisposeModel {
        disposed: false,
        state: FiberState::Pending,
        saw_disposed_state: false,
        illegal_resurrection: false,
        effect_gate_closed: false,
        effects_registered: 0,
        effects_disposed: 0,
        driver_held_by: None,
        pc: [0; 2],
    };
    let schedules = explore_all_interleavings(
        init,
        2,
        &|s, actor| {
            if actor == 0 {
                // settle_inner -> load -> (if disposed, unload)
                match s.pc[0] {
                    0 => {
                        if s.driver_held_by.is_some() {
                            return StepResult::Blocked;
                        }
                        s.driver_held_by = Some(0);
                        s.pc[0] = 1;
                        StepResult::Stepped
                    }
                    1 => {
                        // open_effects() checks !is_disposed()
                        if s.disposed {
                            s.pc[0] = 5; // jump to unload
                        } else {
                            s.effect_gate_closed = false;
                            s.set_state(FiberState::Loading);
                            s.pc[0] = 2;
                        }
                        StepResult::Stepped
                    }
                    2 => {
                        // apply registers effect if gate open
                        if !s.effect_gate_closed {
                            s.effects_registered += 1;
                        }
                        s.pc[0] = 3;
                        StepResult::Stepped
                    }
                    3 => {
                        // apply completes: re-check is_disposed() and set_state(Active)
                        if s.disposed {
                            s.pc[0] = 5; // jump to unload
                        } else {
                            s.set_state(FiberState::Active);
                            s.pc[0] = 4;
                        }
                        StepResult::Stepped
                    }
                    4 => {
                        // settle loop re-checks disposed before releasing driver
                        if s.disposed {
                            s.pc[0] = 5;
                        } else {
                            s.driver_held_by = None;
                            s.pc[0] = 7;
                        }
                        StepResult::Stepped
                    }
                    5 => {
                        // unload: Unloading -> close_effects -> drain
                        s.set_state(FiberState::Unloading);
                        s.effect_gate_closed = true;
                        s.effects_disposed += s.effects_registered;
                        s.effects_registered = 0;
                        s.pc[0] = 6;
                        StepResult::Stepped
                    }
                    6 => {
                        s.set_state(FiberState::Disposed);
                        s.driver_held_by = None;
                        s.pc[0] = 7;
                        StepResult::Stepped
                    }
                    _ => StepResult::Done,
                }
            } else {
                // dispose: uid=DISPOSED -> mark_effects_closed -> set_state(Disposed) -> acquire driver & unload
                match s.pc[1] {
                    0 => {
                        s.disposed = true;
                        s.pc[1] = 1;
                        StepResult::Stepped
                    }
                    1 => {
                        s.effect_gate_closed = true;
                        s.set_state(FiberState::Disposed);
                        s.pc[1] = 2;
                        StepResult::Stepped
                    }
                    2 => {
                        if s.driver_held_by.is_some() {
                            return StepResult::Blocked;
                        }
                        s.driver_held_by = Some(1);
                        s.pc[1] = 3;
                        StepResult::Stepped
                    }
                    3 => {
                        s.set_state(FiberState::Unloading);
                        s.effect_gate_closed = true;
                        s.effects_disposed += s.effects_registered;
                        s.effects_registered = 0;
                        s.set_state(FiberState::Disposed);
                        s.driver_held_by = None;
                        s.pc[1] = 4;
                        StepResult::Stepped
                    }
                    _ => StepResult::Done,
                }
            }
        },
        &|s| {
            assert!(s.disposed);
            assert_eq!(s.state, FiberState::Disposed);
            assert!(
                !s.illegal_resurrection,
                "illegal resurrection from Disposed"
            );
            assert!(s.effect_gate_closed);
            assert_eq!(s.effects_registered, 0, "leaked effect after dispose");
            assert!(s.driver_held_by.is_none());
        },
    );
    assert!(
        schedules >= 10,
        "expected thorough schedule exploration, got {schedules}"
    );
}

// ------------------------------------------------------------------ 3. Store unregister vs register

#[derive(Clone)]
struct StoreUnregModel {
    runtimes_lock: Option<usize>,
    fibers_lock: Option<usize>,
    runtime_in_table: bool,
    runtime_fibers: [bool; 2], // fiber 0 (existing), fiber 1 (mounting)
    pc: [u8; 2],               // actor 0 = unregister(0), actor 1 = runtime_for + register(1)
}

/// Exhaustively proves `Store::unregister_fiber` vs `runtime_for` + `register_fiber`
/// under lock order `runtimes -> runtime.fibers` never orphans fiber 1 and never deadlocks.
#[test]
fn dfs_store_unregister_vs_register_fiber_no_orphan() {
    let init = StoreUnregModel {
        runtimes_lock: None,
        fibers_lock: None,
        runtime_in_table: true,
        runtime_fibers: [true, false],
        pc: [0; 2],
    };
    let schedules = explore_all_interleavings(
        init,
        2,
        &|s, actor| {
            if actor == 0 {
                // unregister_fiber(0): lock runtimes -> lock fibers -> remove 0 -> if empty remove runtime -> unlock
                match s.pc[0] {
                    0 => {
                        if s.runtimes_lock.is_some() {
                            return StepResult::Blocked;
                        }
                        s.runtimes_lock = Some(0);
                        s.pc[0] = 1;
                        StepResult::Stepped
                    }
                    1 => {
                        if s.fibers_lock.is_some() {
                            return StepResult::Blocked;
                        }
                        s.fibers_lock = Some(0);
                        s.runtime_fibers[0] = false;
                        let empty = !s.runtime_fibers[0] && !s.runtime_fibers[1];
                        s.fibers_lock = None;
                        if empty {
                            s.runtime_in_table = false;
                        }
                        s.runtimes_lock = None;
                        s.pc[0] = 2;
                        StepResult::Stepped
                    }
                    _ => StepResult::Done,
                }
            } else {
                // runtime_for() (lock/unlock runtimes) -> register_fiber(1) (lock runtimes -> ensure in table -> lock fibers -> insert 1)
                match s.pc[1] {
                    0 => {
                        if s.runtimes_lock.is_some() {
                            return StepResult::Blocked;
                        }
                        s.runtimes_lock = Some(1);
                        s.runtime_in_table = true;
                        s.runtimes_lock = None;
                        s.pc[1] = 1;
                        StepResult::Stepped
                    }
                    1 => {
                        if s.runtimes_lock.is_some() {
                            return StepResult::Blocked;
                        }
                        s.runtimes_lock = Some(1);
                        s.runtime_in_table = true;
                        s.pc[1] = 2;
                        StepResult::Stepped
                    }
                    2 => {
                        if s.fibers_lock.is_some() {
                            return StepResult::Blocked;
                        }
                        s.fibers_lock = Some(1);
                        s.runtime_fibers[1] = true;
                        s.fibers_lock = None;
                        s.runtimes_lock = None;
                        s.pc[1] = 3;
                        StepResult::Stepped
                    }
                    _ => StepResult::Done,
                }
            }
        },
        &|s| {
            assert!(s.runtime_in_table, "runtime evicted while fiber 1 is live!");
            assert!(!s.runtime_fibers[0]);
            assert!(s.runtime_fibers[1], "fiber 1 was orphaned!");
        },
    );
    // 3 exact interleavings across the 2 critical sections:
    // [unreg, rt_for, reg], [rt_for, unreg, reg], [rt_for, reg, unreg].
    assert_eq!(schedules, 3);
}
