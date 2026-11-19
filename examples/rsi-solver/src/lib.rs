//! RSI substrate e2e: one real T2 cdylib with build-time variants.
//!
//! Features are intentionally non-exclusive so `cargo check --all-features`
//! remains valid. `panic` wins over every other variant; then `crash`, `v3`,
//! `v2`, and finally `v1`.

use cordis_abi::cordis_plugin;
use cordis_abi::sdk::{self, AsyncIfaceFn, AsyncReply, PluginResult};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

cordis_plugin! {
    name: "rsi-solver",
    provision: ["solver@1"],
    apply: apply,
}

const fn variant() -> &'static str {
    if cfg!(feature = "panic") {
        "panic"
    } else if cfg!(feature = "crash") {
        "crash"
    } else if cfg!(feature = "v3") {
        "v3"
    } else if cfg!(feature = "v2") {
        "v2"
    } else {
        "v1"
    }
}

fn apply(ctx: &sdk::Ctx, _config: Value) -> PluginResult {
    if cfg!(feature = "panic") {
        panic!("rsi-solver: apply panic ({})", variant());
    }
    let ctx = *ctx;
    let find = Arc::new(Mutex::new(Vec::<u64>::new()));
    let solve = solve_method();
    let hold = hold_method(ctx, find.clone());
    let mut methods: Vec<(&'static str, AsyncIfaceFn)> = Vec::new();
    if cfg!(feature = "v3") {
        methods.push(("answer", solve));
    } else {
        methods.push(("solve", solve));
    }
    methods.push(("hold", hold));
    ctx.provide_iface_async("solver@1", 1, methods)?;

    // `run_hold` releases pending solver calls through this event. It is a
    // separate event (not an iface method) so the e2e driver can control
    // timing from the root context without another service.
    let release_ctx = ctx;
    ctx.on("rsi/release-solver", move |_args: Vec<Value>| {
        let pending: Vec<u64> = {
            let mut guard = find.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.drain(..).collect()
        };
        for dispatch_id in pending {
            let _ = release_ctx.fulfill(dispatch_id, json!({ "released": true }));
        }
        Ok(Value::Null)
    })?;
    Ok(())
}

fn solve_method() -> AsyncIfaceFn {
    Arc::new(
        move |args: Value, _meta: Option<Value>, _dispatch_id: u64| {
            let case = args.get(0).cloned().unwrap_or(Value::Null);
            if cfg!(feature = "crash") && case.as_str() == Some("crash") {
                panic!("rsi-solver: solve panic (crash variant)");
            }
            if cfg!(feature = "v2") {
                let id = case.as_u64().unwrap_or(0);
                if id == 2 || id == 4 {
                    return Err(format!("solver rejected case {id}"));
                }
            }
            Ok(AsyncReply::Reply(json!({
                "solver": variant(),
                "case": case,
            })))
        },
    )
}

fn hold_method(ctx: sdk::Ctx, pending: Arc<Mutex<Vec<u64>>>) -> AsyncIfaceFn {
    Arc::new(
        move |_args: Value, _meta: Option<Value>, dispatch_id: u64| {
            pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(dispatch_id);
            let _ = ctx.emit("rsi/solver-held", Vec::new());
            Ok(AsyncReply::Pending)
        },
    )
}
