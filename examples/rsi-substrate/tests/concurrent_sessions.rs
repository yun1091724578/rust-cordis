//! Single-App multi-session concurrent Rust RSI e2e:
//! - Compile-time shared Rust DTO contract (`SolveRequest` / `SolveResponse`)
//! - Non-blocking T2 `STATUS_PENDING` off-thread execution template (`async_solver_fn`)
//! - Multi-version coexistence (`solver.v1@1` + `solver.v2@1`) via `VersionRouter`
//! - Multi-hop concurrent T2→T2 (`gateway@1 -> solver.vX@1`) across 4 Tokio workers
//! - `AsyncJsonlSink` + `correlation_id` per-session leaf-error attribution
//! - Graceful drain & zero-interruption upgrade (old v1 sessions finish cleanly
//!   while new sessions route to v2, then v1 unloads cleanly)

use cordis_abi::sdk::{self, AsyncIfaceFn, PluginResult};
use cordis_core::{App, Config, Ctx, FiberState, Inject, Plugin};
use cordis_host::{
    apply_via_with_error_format, host_api, loaded_instances, set_host_limits_for, CallFn,
    HostLimits,
};
use rsi_substrate::contract::{
    async_solver_fn, decode_outcome, forward_call_iface, SolveOutcome, SolveRequest, SolveResponse,
    SolverImpl, SOLVE_CONTRACT_VERSION,
};
use rsi_substrate::router::VersionRouter;
use rsi_substrate::trace::{leaf_errors_by_session, window, AsyncJsonlSink};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

struct VariantSolver {
    variant: &'static str,
}

impl SolverImpl for VariantSolver {
    fn solve(&self, req: SolveRequest) -> Result<SolveResponse, String> {
        if req.delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(req.delay_ms));
        }
        Ok(SolveResponse {
            variant: self.variant.to_string(),
            task_id: req.task_id,
            output: format!("{}:{}:{}", self.variant, req.session_id, req.prompt),
        })
    }
}

fn solver_v1_apply(ctx: &sdk::Ctx, _config: Value) -> PluginResult {
    let solve = async_solver_fn(*ctx, Arc::new(VariantSolver { variant: "v1" }));
    ctx.provide_iface_async(
        "solver.v1@1",
        SOLVE_CONTRACT_VERSION,
        vec![("solve", solve)],
    )
}

fn solver_v2_apply(ctx: &sdk::Ctx, _config: Value) -> PluginResult {
    let base = async_solver_fn(*ctx, Arc::new(VariantSolver { variant: "v2" }));
    let solve: AsyncIfaceFn =
        Arc::new(move |args: Value, meta: Option<Value>, dispatch_id: u64| {
            let raw = args.get(0).cloned().unwrap_or_else(|| args.clone());
            if raw.get("prompt").and_then(Value::as_str) == Some("trigger-v2-err") {
                return Err("v2 rejected trigger-v2-err".to_string());
            }
            base(args, meta, dispatch_id)
        });
    ctx.provide_iface_async(
        "solver.v2@1",
        SOLVE_CONTRACT_VERSION,
        vec![("solve", solve)],
    )
}

fn gateway_apply(ctx: &sdk::Ctx, _config: Value) -> PluginResult {
    let ctx = *ctx;
    let dispatch: AsyncIfaceFn =
        Arc::new(move |args: Value, _meta: Option<Value>, dispatch_id: u64| {
            let envelope = args.get(0).cloned().unwrap_or(args);
            let target = envelope["target_service"]
                .as_str()
                .ok_or_else(|| "gateway missing target_service".to_string())?;
            let request = envelope["request"].clone();
            forward_call_iface(
                ctx,
                dispatch_id,
                target,
                SOLVE_CONTRACT_VERSION,
                "solve",
                json!([request]),
                |err| {
                    serde_json::to_value(SolveOutcome::Err {
                        code: "downstream_err".to_string(),
                        message: err,
                    })
                    .unwrap_or(Value::Null)
                },
            )
        });
    ctx.provide_iface_async(
        "gateway@1",
        SOLVE_CONTRACT_VERSION,
        vec![("dispatch", dispatch)],
    )
}

fn dispatch_apply(ctx: &sdk::Ctx, config: Value) -> PluginResult {
    match config.get("role").and_then(Value::as_str) {
        Some("v1") => solver_v1_apply(ctx, config),
        Some("v2") => solver_v2_apply(ctx, config),
        Some("gateway") => gateway_apply(ctx, config),
        other => Err(format!("unknown role {other:?}")),
    }
}

fn abi_plugin(name: &'static str, entry: CallFn, error_format: u32) -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name,
        key: cordis_core::unique_key(name),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| {
            apply_via_with_error_format(entry, ctx, config, name, error_format)
        }),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_session_concurrent_canary_attribution_and_graceful_drain() {
    let tmp = std::env::temp_dir().join(format!(
        "cordis-rsi-concurrent-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let trace_path = tmp.join("trace.jsonl");
    let sink = Arc::new(AsyncJsonlSink::open(&trace_path, 4096).unwrap());

    unsafe { sdk::attach(host_api()) };
    let desc = sdk::build_plugin("rsi-concurrent-sdk", &[], dispatch_apply);
    let (entry, error_format) = unsafe {
        let d = &*desc;
        (d.call, cordis_abi::PluginDesc::error_format_at(desc))
    };

    let app = App::new();
    // Tune HostLimits explicitly for single-host multi-session concurrency.
    let limits = HostLimits::new(1024, 4096, 1024, 128).expect("valid HostLimits");
    set_host_limits_for(&app.root, limits).expect("apply HostLimits");
    assert!(app.set_trace_sink(sink.clone()));

    let v1_fiber = app.root.plugin(
        abi_plugin("solver-v1", entry, error_format),
        json!({ "role": "v1" }),
    );
    v1_fiber.settle().await;
    let v2_fiber = app.root.plugin(
        abi_plugin("solver-v2", entry, error_format),
        json!({ "role": "v2" }),
    );
    v2_fiber.settle().await;
    // Gateway uses optional injects for coexisting solver versions so draining
    // and unloading v1 never cascades into unloading gateway or breaking v2.
    let gw_fiber = app.store.plugin_with_optional(
        &app.root,
        abi_plugin("gateway", entry, error_format),
        json!({ "role": "gateway" }),
        Inject::default(),
        &["solver.v1@1".to_string(), "solver.v2@1".to_string()],
    );
    gw_fiber.settle().await;
    for fiber in [&v1_fiber, &v2_fiber, &gw_fiber] {
        assert_eq!(fiber.state(), FiberState::Active);
    }

    let router = VersionRouter::new();
    router.register("v1", "solver.v1@1", 50);
    router.register("v2", "solver.v2@1", 50);

    // ---- Phase 1: 20 concurrent sessions, 2 turns each, 50/50 canary --------
    sink.marker("phase1-canary");
    let mut handles = Vec::new();
    for i in 0..20usize {
        let session_id = format!("sess-{i:02}");
        let sess_ctx = app.root.with_correlation(session_id.clone());
        let router = router.clone();
        let delay_ms = 5 + ((i * 3) % 15) as u64;
        handles.push(tokio::spawn(async move {
            // Turn 1: normal request (both v1 and v2 succeed off-thread).
            let lease1 = router.acquire(&session_id).expect("acquire turn 1");
            let v_turn1 = lease1.version().to_string();
            let req1 = SolveRequest {
                session_id: session_id.clone(),
                task_id: 1,
                prompt: "hello".to_string(),
                delay_ms,
            };
            let raw1 = sess_ctx
                .call_iface(
                    "gateway@1",
                    SOLVE_CONTRACT_VERSION,
                    "dispatch",
                    json!({ "target_service": lease1.service_key(), "request": req1 }),
                )
                .await
                .expect("turn 1 gateway call must succeed");
            let resp1 = decode_outcome(raw1)
                .expect("valid DTO")
                .into_result()
                .expect("turn 1 must be Ok");
            assert_eq!(resp1.variant, v_turn1);
            drop(lease1);

            // Turn 2: affinity must keep the exact same version; trigger-v2-err fails on v2 only.
            let lease2 = router.acquire(&session_id).expect("acquire turn 2");
            assert_eq!(
                lease2.version(),
                v_turn1,
                "session affinity must hold across turns"
            );
            let req2 = SolveRequest {
                session_id: session_id.clone(),
                task_id: 2,
                prompt: "trigger-v2-err".to_string(),
                delay_ms,
            };
            let turn2_res = sess_ctx
                .call_iface(
                    "gateway@1",
                    SOLVE_CONTRACT_VERSION,
                    "dispatch",
                    json!({ "target_service": lease2.service_key(), "request": req2 }),
                )
                .await;
            drop(lease2);
            router.release_session(&session_id);
            (session_id, v_turn1, turn2_res)
        }));
    }

    let mut v1_sessions = HashSet::new();
    let mut v2_sessions = HashSet::new();
    for handle in handles {
        let (sid, version, turn2_res) = handle.await.expect("session task must not panic");
        if version == "v1" {
            let raw = turn2_res.expect("v1 turn 2 must succeed");
            let resp = decode_outcome(raw)
                .expect("valid DTO")
                .into_result()
                .expect("v1 turn 2 Ok");
            assert_eq!(resp.variant, "v1");
            v1_sessions.insert(sid);
        } else {
            assert_eq!(version, "v2");
            assert!(
                turn2_res.is_err(),
                "v2 turn 2 must fail with STATUS_ERR: {turn2_res:?}"
            );
            v2_sessions.insert(sid);
        }
    }
    sink.marker("phase1-end");

    assert!(
        !v1_sessions.is_empty() && !v2_sessions.is_empty(),
        "50/50 split must exercise both variants: v1={} v2={}",
        v1_sessions.len(),
        v2_sessions.len()
    );

    let all_lines = sink.lines();
    assert_eq!(sink.dropped_count(), 0, "bounded async sink must drop 0");
    let p1_rows = window(&all_lines, "phase1-canary");
    // 20 sessions × 2 turns × 2 hops (gateway + solver) = 80 trace rows.
    assert_eq!(
        p1_rows.len(),
        80,
        "20 sessions * 2 turns * 2 hops must produce 80 trace records, got {}",
        p1_rows.len()
    );

    // Every single row must carry a valid session correlation_id and valid parent link.
    let mut by_session: HashMap<String, Vec<Value>> = HashMap::new();
    for row in &p1_rows {
        let cid = row["correlation_id"]
            .as_str()
            .unwrap_or_else(|| panic!("missing correlation_id in row: {row}"));
        by_session
            .entry(cid.to_string())
            .or_default()
            .push(row.clone());
    }
    assert_eq!(by_session.len(), 20);
    for (sid, rows) in &by_session {
        assert_eq!(rows.len(), 4, "session {sid} must have 4 hops: {rows:?}");
        let gw_ids: HashSet<u64> = rows
            .iter()
            .filter(|r| r["callee_plugin"] == "gateway")
            .filter_map(|r| r["call_id"].as_u64())
            .collect();
        for solver_row in rows
            .iter()
            .filter(|r| r["callee_plugin"] == "solver-v1" || r["callee_plugin"] == "solver-v2")
        {
            let parent = solver_row["parent_call_id"].as_u64().unwrap_or(0);
            assert!(
                gw_ids.contains(&parent),
                "solver hop in {sid} must link to its session's gateway hop: {solver_row}"
            );
        }
    }

    // Leaf error attribution per session must pin 100% of faults to solver-v2 only.
    let leaves = leaf_errors_by_session(&p1_rows);
    assert_eq!(
        leaves.len(),
        v2_sessions.len(),
        "only v2 sessions may have leaf errors: {leaves:?}"
    );
    for sid in &v2_sessions {
        let counts = leaves
            .get(sid)
            .unwrap_or_else(|| panic!("missing leaf error for v2 session {sid}"));
        assert_eq!(counts.get("solver-v2"), Some(&1), "{counts:?}");
        assert_eq!(
            counts.len(),
            1,
            "gateway must not be blamed as leaf: {counts:?}"
        );
    }
    for sid in &v1_sessions {
        assert!(
            !leaves.contains_key(sid),
            "v1 session {sid} must have zero errors"
        );
    }

    // ---- Phase 2: Graceful drain while 6 v1 sessions are mid-flight ---------
    sink.marker("phase2-drain");
    let mut old_tasks = Vec::new();
    let mut old_leases = Vec::new();
    for i in 0..6usize {
        let sid = format!("drain-old-{i}");
        let lease = router
            .acquire_version(&sid, "v1")
            .expect("pin old session to v1");
        old_leases.push((sid, lease));
    }
    assert_eq!(router.in_flight("v1"), 6);

    for (sid, lease) in old_leases {
        let sess_ctx = app.root.with_correlation(sid.clone());
        old_tasks.push(tokio::spawn(async move {
            let req = SolveRequest {
                session_id: sid.clone(),
                task_id: 99,
                prompt: "slow-work".to_string(),
                delay_ms: 80,
            };
            let raw = sess_ctx
                .call_iface(
                    "gateway@1",
                    SOLVE_CONTRACT_VERSION,
                    "dispatch",
                    json!({ "target_service": lease.service_key(), "request": req }),
                )
                .await
                .expect("in-flight v1 session must NOT be interrupted during drain");
            let resp = decode_outcome(raw).unwrap().into_result().unwrap();
            assert_eq!(resp.variant, "v1");
            drop(lease);
            sid
        }));
    }

    // Promote v2 and start draining v1 immediately while old sessions are in-flight.
    router.promote("v2").unwrap();
    router.begin_drain("v1").unwrap();

    // Launch 6 brand-new sessions concurrently: ALL must route to v2.
    let mut new_tasks = Vec::new();
    for i in 0..6usize {
        let sid = format!("drain-new-{i}");
        let sess_ctx = app.root.with_correlation(sid.clone());
        let router = router.clone();
        new_tasks.push(tokio::spawn(async move {
            let lease = router.acquire(&sid).expect("new session acquires v2");
            assert_eq!(lease.version(), "v2", "new sessions must go to promoted v2");
            let req = SolveRequest {
                session_id: sid.clone(),
                task_id: 100,
                prompt: "new-traffic".to_string(),
                delay_ms: 10,
            };
            let raw = sess_ctx
                .call_iface(
                    "gateway@1",
                    SOLVE_CONTRACT_VERSION,
                    "dispatch",
                    json!({ "target_service": lease.service_key(), "request": req }),
                )
                .await
                .expect("new v2 session must succeed");
            let resp = decode_outcome(raw).unwrap().into_result().unwrap();
            assert_eq!(resp.variant, "v2");
            drop(lease);
            router.release_session(&sid);
        }));
    }

    for task in new_tasks {
        task.await.unwrap();
    }
    for task in old_tasks {
        task.await.unwrap();
    }

    // Now v1 has drained to zero; safe to unload v1 fiber without interrupting anyone.
    tokio::time::timeout(Duration::from_secs(5), router.wait_drained("v1"))
        .await
        .expect("v1 drain waiter must complete")
        .unwrap();
    v1_fiber.dispose().await;
    router.unregister("v1").unwrap();

    // Verify v2 continues serving cleanly after v1 is unloaded.
    let post_ctx = app.root.with_correlation("post-unload-01");
    let lease = router.acquire("post-unload-01").unwrap();
    assert_eq!(lease.version(), "v2");
    let req = SolveRequest {
        session_id: "post-unload-01".into(),
        task_id: 101,
        prompt: "still-healthy".into(),
        delay_ms: 5,
    };
    let raw = post_ctx
        .call_iface(
            "gateway@1",
            SOLVE_CONTRACT_VERSION,
            "dispatch",
            json!({ "target_service": lease.service_key(), "request": req }),
        )
        .await
        .expect("v2 must keep serving after v1 unload");
    let resp = decode_outcome(raw).unwrap().into_result().unwrap();
    assert_eq!(resp.variant, "v2");
    drop(lease);

    app.close().await;
    assert_eq!(
        loaded_instances(),
        0,
        "all ABI plugin instances must return to 0 after App::close"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
