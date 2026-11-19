//! RSI substrate e2e: a real T2 plugin that forwards to `solver@1`.
//!
//! It exists to make the trace chain indirect: `evaluator -> pipeline -> solver`.
//! `run` returns the solver result synchronously when the host inlines the
//! outbound callback; otherwise it stays pending and `fulfill`s the inbound
//! dispatch. The distinction is what proves the e2e attribution path.

use cordis_abi::cordis_plugin;
use cordis_abi::sdk::{self, AsyncReply, PluginResult};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Build a method that forwards to `solver@1`; when the host does not inline
/// the outbound callback, the method stays pending and settles later.
fn make_forwarder(ctx: sdk::Ctx, method: &'static str) -> sdk::AsyncIfaceFn {
    Arc::new(move |args: Value, _meta: Option<Value>, dispatch_id: u64| {
        let target = if method == "run_hold" {
            "hold"
        } else {
            "solve"
        };
        let slot = Arc::new(Mutex::new(None::<Result<Value, String>>));
        let inline = Arc::new(AtomicBool::new(false));
        let callback_slot = slot.clone();
        let callback_inline = inline.clone();
        let callback_ctx = ctx;
        ctx.call_iface("solver@1", 1, target, args, move |result| {
            if callback_inline.load(Ordering::SeqCst) {
                // Async continuation: the inbound dispatch is still pending.
                let value = match result {
                    Ok(value) => value,
                    Err(err) => json!({ "error": err }),
                };
                let _ = callback_ctx.fulfill(dispatch_id, value);
            } else {
                *callback_slot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
            }
        })?;
        inline.store(true, Ordering::SeqCst);
        let result = slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        match result {
            Some(Ok(value)) => Ok(AsyncReply::Reply(value)),
            Some(Err(err)) => Err(err),
            None => Ok(AsyncReply::Pending),
        }
    })
}
