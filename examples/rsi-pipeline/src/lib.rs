//! RSI substrate e2e: a real T2 plugin that forwards to `solver@1`.
//!
//! It exists to make the trace chain indirect: `evaluator -> pipeline -> solver`.
//! `run` returns the solver result synchronously when the host inlines the
//! outbound callback; otherwise it stays pending and `fulfill`s the inbound
//! dispatch. The distinction is what proves the e2e attribution path.

use cordis_abi::cordis_plugin;
use cordis_abi::sdk::{self, AsyncReply, PluginResult};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

cordis_plugin! {
    name: "rsi-pipeline",
    inject: ["solver@1"],
    provision: ["pipeline@1"],
    apply: apply,
}

fn apply(ctx: &sdk::Ctx, _config: Value) -> PluginResult {
    let ctx = *ctx;
    let run = make_forwarder(ctx, "run");
    let run_hold = make_forwarder(ctx, "run_hold");
    ctx.provide_iface_async("pipeline@1", 1, vec![("run", run), ("run_hold", run_hold)])
}

enum ForwardState {
    Waiting,
    InlineReady(Result<Value, String>),
    SwitchedToAsync,
}

/// Build a method that forwards to `solver@1`; uses a single-mutex 3-state
/// machine so inline completion and async `OP_CALLBACK` continuation can never
/// race and drop a settlement.
fn make_forwarder(ctx: sdk::Ctx, method: &'static str) -> sdk::AsyncIfaceFn {
    Arc::new(move |args: Value, _meta: Option<Value>, dispatch_id: u64| {
        let target = if method == "run_hold" {
            "hold"
        } else {
            "solve"
        };
        let state = Arc::new(Mutex::new(ForwardState::Waiting));
        let cb_state = state.clone();
        let callback_ctx = ctx;
        ctx.call_iface("solver@1", 1, target, args, move |result| {
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
                let value = match result {
                    Ok(value) => value,
                    Err(err) => json!({ "error": err }),
                };
                let _ = callback_ctx.fulfill(dispatch_id, value);
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
            Some(Ok(value)) => Ok(AsyncReply::Reply(value)),
            Some(Err(err)) => Err(err),
            None => Ok(AsyncReply::Pending),
        }
    })
}
