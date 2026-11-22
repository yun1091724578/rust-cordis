//! Compile-time Rust DTO contract + bounded non-blocking T2 async plugin template
//! + race-free T2-to-T2 interface forwarding helper.
//!
//! Why this exists for Rust RSI plugins:
//! 1. `ExternService::methods()` only locks method names across the C ABI;
//!    sharing a reviewed Rust DTO crate between the host and all evolved
//!    variants locks request/response shapes at compile time via `rustc`.
//! 2. T2 synchronous callbacks run on Tokio worker threads and are NOT
//!    preemptible (ADR 0005 §1.1). `async_solver_fn` offloads work onto a
//!    **bounded worker pool** with a bounded queue (`try_send`) so high
//!    concurrency cannot spawn unbounded OS threads or starve Tokio workers.
//! 3. `forward_call_iface` encapsulates the single-mutex 3-state machine so
//!    inline completion and async `OP_CALLBACK` continuations can never race.

use cordis_abi::sdk::{self, AsyncIfaceFn, AsyncReply};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

pub const SOLVE_CONTRACT_VERSION: u32 = 1;
pub const DEFAULT_SOLVER_WORKERS: usize = 4;
pub const DEFAULT_SOLVER_QUEUE: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolveRequest {
    pub session_id: String,
    pub task_id: u64,
    pub prompt: String,
    #[serde(default)]
    pub delay_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolveResponse {
    pub variant: String,
    pub task_id: u64,
    pub output: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SolveOutcome {
    Ok(SolveResponse),
    Err { code: String, message: String },
}

impl SolveOutcome {
    pub fn into_result(self) -> Result<SolveResponse, (String, String)> {
        match self {
            Self::Ok(resp) => Ok(resp),
            Self::Err { code, message } => Err((code, message)),
        }
    }
}

/// Pure Rust trait that RSI-generated plugin variants implement.
///
/// Generated code never touches `extern "C"`, raw buffers, or host handles.
/// Implementations must be stateless or strictly sharded by `req.session_id`
/// (no cross-session mutable `static`, ADR 0005 §1.1 #5).
pub trait SolverImpl: Send + Sync + 'static {
    fn solve(&self, req: SolveRequest) -> Result<SolveResponse, String>;
}

type Job = Box<dyn FnOnce() + Send + 'static>;

/// Fixed-size worker pool with a bounded queue so concurrent multi-session
/// calls never cause unbounded OS thread creation.
pub struct SolverPool {
    tx: Option<SyncSender<Job>>,
    workers: Vec<JoinHandle<()>>,
}

impl SolverPool {
    pub fn new(workers: usize, queue_capacity: usize) -> Self {
        let workers = workers.max(1);
        let (tx, rx) = mpsc::sync_channel::<Job>(queue_capacity.max(1));
        let shared_rx = Arc::new(Mutex::new(rx));
        let mut handles = Vec::with_capacity(workers);
        for index in 0..workers {
            let rx = shared_rx.clone();
            if let Ok(handle) = std::thread::Builder::new()
                .name(format!("cordis-solver-{index}"))
                .spawn(move || loop {
                    let next = {
                        let guard = rx.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        guard.recv()
                    };
                    match next {
                        Ok(job) => job(),
                        Err(_) => break,
                    }
                })
            {
                handles.push(handle);
            }
        }
        Self {
            tx: Some(tx),
            workers: handles,
        }
    }

    pub fn try_submit<F>(&self, job: F) -> Result<(), &'static str>
    where
        F: FnOnce() + Send + 'static,
    {
        let Some(tx) = self.tx.as_ref() else {
            return Err("host_lifecycle: solver worker pool closed");
        };
        match tx.try_send(Box::new(job)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("host_capacity: solver worker queue full"),
            Err(TrySendError::Disconnected(_)) => Err("host_lifecycle: solver worker pool closed"),
        }
    }
}

impl Drop for SolverPool {
    fn drop(&mut self) {
        self.tx.take();
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Wrap a pure [`SolverImpl`] into a non-blocking T2 [`AsyncIfaceFn`] backed
/// by a bounded worker pool (`DEFAULT_SOLVER_WORKERS` × `DEFAULT_SOLVER_QUEUE`).
pub fn async_solver_fn<S: SolverImpl>(ctx: sdk::Ctx, solver: Arc<S>) -> AsyncIfaceFn {
    async_solver_fn_with_pool(
        ctx,
        solver,
        Arc::new(SolverPool::new(
            DEFAULT_SOLVER_WORKERS,
            DEFAULT_SOLVER_QUEUE,
        )),
    )
}

/// Same as [`async_solver_fn`] with an explicit [`SolverPool`].
///
/// - Fast DTO validation and capacity admission happen synchronously; if the
///   bounded queue is full, the call fails immediately with `host_capacity`.
/// - Actual execution runs on the fixed worker pool inside `catch_unwind` and
///   settles via `ctx.fulfill`, preventing slow variants from starving Tokio.
pub fn async_solver_fn_with_pool<S: SolverImpl>(
    ctx: sdk::Ctx,
    solver: Arc<S>,
    pool: Arc<SolverPool>,
) -> AsyncIfaceFn {
    Arc::new(move |args: Value, _meta: Option<Value>, dispatch_id: u64| {
        let raw = args.get(0).cloned().unwrap_or(args);
        let req: SolveRequest = serde_json::from_value(raw)
            .map_err(|err| format!("invalid SolveRequest DTO: {err}"))?;
        let solver = solver.clone();
        pool.try_submit(move || {
            let outcome = match catch_unwind(AssertUnwindSafe(|| solver.solve(req))) {
                Ok(Ok(resp)) => SolveOutcome::Ok(resp),
                Ok(Err(message)) => SolveOutcome::Err {
                    code: "solver_error".to_string(),
                    message,
                },
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "panic inside solver worker".to_string());
                    SolveOutcome::Err {
                        code: "solver_panic".to_string(),
                        message,
                    }
                }
            };
            let value = serde_json::to_value(outcome).unwrap_or_else(|err| {
                json!({
                    "status": "err",
                    "code": "encode_error",
                    "message": err.to_string(),
                })
            });
            let _ = ctx.fulfill(dispatch_id, value);
        })
        .map_err(str::to_string)?;
        Ok(AsyncReply::Pending)
    })
}

enum ForwardState {
    Waiting,
    InlineReady(Result<Value, String>),
    SwitchedToAsync,
}

/// Race-free helper for forwarding an inbound `provide_iface_async` call to a
/// downstream T2 service via `ctx.call_iface`.
///
/// Uses a single-mutex 3-state transition so inline completion and cross-thread
/// async continuation delivery can never lose or double-fire a settlement.
pub fn forward_call_iface<E>(
    ctx: sdk::Ctx,
    dispatch_id: u64,
    target_service: &str,
    version: u32,
    method: &str,
    args: Value,
    on_async_err: E,
) -> Result<AsyncReply, String>
where
    E: Fn(String) -> Value + Send + 'static,
{
    let state = Arc::new(Mutex::new(ForwardState::Waiting));
    let cb_state = state.clone();
    let cb_ctx = ctx;
    ctx.call_iface(target_service, version, method, args, move |result| {
        let async_result = {
            let mut guard = cb_state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match *guard {
                ForwardState::Waiting => {
                    *guard = ForwardState::InlineReady(result);
                    None
                }
                ForwardState::SwitchedToAsync => Some(result),
                ForwardState::InlineReady(_) => None,
            }
        };
        if let Some(result) = async_result {
            let settled = match result {
                Ok(val) => val,
                Err(err) => on_async_err(err),
            };
            let _ = cb_ctx.fulfill(dispatch_id, settled);
        }
    })?;

    let immediate = {
        let mut guard = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match std::mem::replace(&mut *guard, ForwardState::SwitchedToAsync) {
            ForwardState::InlineReady(res) => Some(res),
            ForwardState::Waiting | ForwardState::SwitchedToAsync => None,
        }
    };
    match immediate {
        Some(Ok(val)) => Ok(AsyncReply::Reply(val)),
        Some(Err(err)) => Err(err),
        None => Ok(AsyncReply::Pending),
    }
}

/// Decode a host-side `call_iface` return value into [`SolveOutcome`].
pub fn decode_outcome(value: Value) -> Result<SolveOutcome, String> {
    serde_json::from_value(value).map_err(|err| format!("invalid SolveOutcome DTO: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn dto_round_trip_preserves_shape() {
        let req = SolveRequest {
            session_id: "sess-1".into(),
            task_id: 7,
            prompt: "hello".into(),
            delay_ms: 15,
        };
        let encoded = serde_json::to_value(&req).unwrap();
        let decoded: SolveRequest = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, req);

        let ok = SolveOutcome::Ok(SolveResponse {
            variant: "v1".into(),
            task_id: 7,
            output: "ok".into(),
        });
        let ok_val = serde_json::to_value(&ok).unwrap();
        assert_eq!(decode_outcome(ok_val).unwrap(), ok);

        let err = SolveOutcome::Err {
            code: "solver_error".into(),
            message: "bad".into(),
        };
        let err_val = serde_json::to_value(&err).unwrap();
        assert_eq!(decode_outcome(err_val).unwrap(), err);
    }

    #[test]
    fn solver_pool_rejects_when_bounded_queue_is_full() {
        let pool = SolverPool::new(1, 1);
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));

        // 1st job occupies the single worker thread.
        let s1 = started.clone();
        let r1 = release.clone();
        pool.try_submit(move || {
            s1.wait();
            r1.wait();
        })
        .unwrap();
        started.wait();

        // 2nd job fills the single bounded queue slot.
        pool.try_submit(|| {}).unwrap();

        // 3rd job must fail immediately with host_capacity instead of spawning a thread.
        let err = pool.try_submit(|| {}).expect_err("full queue must reject");
        assert!(err.contains("host_capacity"), "unexpected error: {err}");

        release.wait();
    }
}
