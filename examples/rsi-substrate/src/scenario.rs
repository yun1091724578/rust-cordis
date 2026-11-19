//! One-App e2e scenario: deploy, canary, attribute, gate, roll back.

use crate::admission;
use crate::evaluator;
use crate::fixtures::Fixtures;
use crate::trace::{leaf_errors, window, JsonlSink};
use cordis_core::{App, Config, ExternValue, FiberState, UID_ROOT};
use cordis_host::loader::{self, DiskPluginSource, LoaderConfig, LoaderState, PolicyPluginSource};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Report {
    pub v1: Value,
    pub v2: Value,
    pub v1_again: Value,
    pub trace: Vec<Value>,
}

const RELOAD_WAIT: Duration = Duration::from_secs(60);

pub async fn run() -> Result<Report, String> {
    let fixtures = Fixtures::build()?;
    fixtures.install_initial()?;
    admit(&fixtures, "v1")?;
    let config_path = fixtures.write_config("solver.dll")?;
    let score = fixtures.dir.path().join("score.json");
    let trace_path = fixtures.dir.path().join("trace.jsonl");
    let sink = Arc::new(JsonlSink::open(&trace_path).map_err(|err| err.to_string())?);

    let policy = Arc::new(fixtures.policy.clone());
    cordis_host::source::set_active_policy(Some(policy.clone()));
    let _reset = ResetPolicy;

    let app = App::new();
    assert!(
        app.set_trace_sink(sink.clone()),
        "trace sink must be installed once"
    );
    let reloads = Arc::new(Mutex::new(Vec::<Value>::new()));
    let held = Arc::new(AtomicBool::new(false));
    let _reload_listener = app.root.on("loader/reload", {
        let reloads = reloads.clone();
        move |call| {
            let value = call
                .args
                .first()
                .and_then(|arg| arg.downcast::<ExternValue>())
                .map(|value| value.0.clone())
                .unwrap_or_else(|| json!({ "raw_event": true }));
            reloads.lock().unwrap().push(value);
            Box::pin(async { None })
        }
    });
    let _held_listener = app.root.on("rsi/solver-held", {
        let held = held.clone();
        move |_call| {
            held.store(true, Ordering::SeqCst);
            Box::pin(async { None })
        }
    });

    let cfg: LoaderConfig = serde_json::from_value(json!({
        "path": config_path.to_string_lossy(),
        "pluginDir": fixtures.dir.path().to_string_lossy(),
        "strict": true,
        "strictCycles": true,
        "strictSidecars": true,
        "hmr": { "intervalMs": 500 },
    }))
    .map_err(|err| format!("LoaderConfig: {err}"))?;
    let state = loader::load_with_source(
        app.root.clone(),
        cfg,
        Arc::new(PolicyPluginSource::strict(
            Arc::new(DiskPluginSource),
            policy.clone(),
        )),
    )
    .await
    .map_err(|err| err.to_string())?;

    wait_for(
        || {
            state.capabilities().len() == 2
                && state
                    .fiber_of("pipeline")
                    .is_some_and(|fiber| fiber.state() == FiberState::Active)
        },
        "initial mount",
        Duration::from_secs(30),
    )
    .await?;
    let status_path = fixtures.dir.path().join("status.json");
    write_status(&app, &state, &status_path, &reloads);
    let evaluator_fiber: Arc<cordis_core::Fiber> =
        app.root.plugin(evaluator::plugin_fn(score), Config::Null);
    evaluator_fiber.settle().await;

    sink.marker("v1");
    let v1 = run_suite(&app).await?;
    sink.marker("v2-deploy");
    assert_eq!(v1["passed"], 5, "v1 must be 5/5: {v1}");
    let solver_v1_sha = cap(&state, "solver")["sha256"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(solver_v1_sha, fixtures.solver["v1"].sha256);

    let v1_rows = window(&sink.lines(), "v1");
    assert_eq!(v1_rows.len(), 10, "v1 must produce 5+5 hops: {v1_rows:?}");
    assert_eq!(
        v1_rows
            .iter()
            .filter(|r| r["name"] == "pipeline@1.run")
            .count(),
        5
    );
    assert_eq!(
        v1_rows
            .iter()
            .filter(|r| r["name"] == "solver@1.solve")
            .count(),
        5
    );
    assert!(v1_rows.iter().all(|r| r["status"] == "ok"), "{v1_rows:?}");
    for solver in v1_rows.iter().filter(|r| r["name"] == "solver@1.solve") {
        let parent = solver["parent_call_id"].as_u64().unwrap_or(0);
        assert!(
            v1_rows
                .iter()
                .any(|r| r["name"] == "pipeline@1.run" && r["call_id"] == parent),
            "solver hop must be parented by a pipeline hop: {solver:?}"
        );
        assert_eq!(solver["callee_plugin"], "rsi-solver");
    }

    // ---- v2: hot swap and leaf attribution ---------------------------------
    let baseline_methods = candidate_methods(&fixtures, "solver-v1.dll").await?;
    let v2_methods = candidate_methods(&fixtures, "solver-v2.dll").await?;
    assert!(
        contract_gate(&baseline_methods, &v2_methods).is_ok(),
        "v2 keeps the contract: baseline={baseline_methods:?} candidate={v2_methods:?}"
    );
    let solver_uid_v1 = cap(&state, "solver")["uid"].as_u64().unwrap();
    let before_pipeline_uid = cap(&state, "pipeline")["uid"].as_u64().unwrap();
    admit(&fixtures, "v2")?;
    fixtures.publish("v2")?;
    wait_for(
        || {
            let row = cap(&state, "solver");
            row["sha256"].as_str() == Some(fixtures.solver["v2"].sha256.as_str())
                && row["uid"].as_u64() != Some(solver_uid_v1)
                && state
                    .fiber_of("pipeline")
                    .is_some_and(|fiber| fiber.state() == FiberState::Active)
        },
        "v2 reload",
        RELOAD_WAIT,
    )
    .await?;
    assert_eq!(
        cap(&state, "pipeline")["uid"].as_u64(),
        Some(before_pipeline_uid),
        "pipeline keeps its fiber uid on solver reload"
    );
    wait_for(
        || evaluator_fiber.state() == FiberState::Active,
        "evaluator reactivation after v2",
        RELOAD_WAIT,
    )
    .await?;

    assert_ne!(cap(&state, "solver")["uid"].as_u64(), Some(solver_uid_v1));
    write_status(&app, &state, &status_path, &reloads);

    sink.marker("v2-run");
    let v2 = run_suite(&app).await?;
    sink.marker("rollback");
    assert_eq!(v2["passed"], 3, "v2 must be 3/5: {v2}");
    for id in [2, 4] {
        assert!(!case_ok(&v2, id), "v2 case {id} must fail: {v2}");
    }
    let v2_rows = window(&sink.lines(), "v2-run");
    assert_eq!(v2_rows.len(), 10, "{v2_rows:?}");
    let leaf = leaf_errors(&v2_rows);
    assert_eq!(leaf.len(), 2, "two leaf errors: {leaf:?}");
    let mut by_callee: HashMap<String, usize> = HashMap::new();
    for row in leaf {
        assert_eq!(row["status"], "err");
        assert_ne!(row["call_id"], 0);
        *by_callee
            .entry(
                row["callee_plugin"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
            .or_default() += 1;
    }
    assert_eq!(by_callee.get("rsi-solver"), Some(&2), "{by_callee:?}");
    assert_eq!(
        by_callee.len(),
        1,
        "pipeline must not be a leaf: {by_callee:?}"
    );

    // ---- YAML rollback ------------------------------------------------------
    fixtures.write_config("solver-baseline.dll")?;
    let solver_uid_v2 = cap(&state, "solver")["uid"].as_u64().unwrap();
    wait_for(
        || {
            let row = cap(&state, "solver");
            row["sha256"].as_str() == Some(fixtures.solver["v1"].sha256.as_str())
                && row["uid"].as_u64() != Some(solver_uid_v2)
        },
        "rollback to baseline",
        RELOAD_WAIT,
    )
    .await?;
    wait_for(
        || evaluator_fiber.state() == FiberState::Active,
        "evaluator reactivation after rollback",
        RELOAD_WAIT,
    )
    .await?;
    sink.marker("v1-again");
    let v1_again = run_suite(&app).await?;
    assert_eq!(
        v1_again["passed"], 5,
        "rollback must restore 5/5: {v1_again}"
    );
    for id in [2, 4] {
        assert!(
            case_ok(&v1_again, id),
            "rollback case {id} must be Ok again: {v1_again}"
        );
    }
    write_status(&app, &state, &status_path, &reloads);

    // ---- v3 contract gate ---------------------------------------------------
    admit(&fixtures, "v3")?;
    let v3_methods = candidate_methods(&fixtures, "solver-v3.dll").await?;
    let gate = contract_gate(&baseline_methods, &v3_methods);
    assert!(
        gate.is_err(),
        "the contract gate must reject v3: {gate:?} baseline={baseline_methods:?} candidate={v3_methods:?}"
    );
    assert!(!v3_methods.iter().any(|m| m == "solve"), "{v3_methods:?}");
    assert!(v3_methods.iter().any(|m| m == "answer"), "{v3_methods:?}");
    let canary_sha_before = cap(&state, "solver")["sha256"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        canary_sha_before, fixtures.solver["v1"].sha256,
        "contract gate must not publish the candidate"
    );

    // ---- apply panic rolls back --------------------------------------------
    let live = fixtures.baseline();
    let uid_before_panic = cap(&state, "solver")["uid"].as_u64().unwrap();
    let panic_reloads_before = reloads.lock().unwrap().len();
    admit(&fixtures, "panic")?;
    fixtures.publish_to(&live, "panic")?;
    wait_for(
        || reloads.lock().unwrap().len() > panic_reloads_before,
        "panic reload event",
        RELOAD_WAIT,
    )
    .await?;
    let panic_event = reloads
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(panic_event["label"], "solver");
    assert!(
        panic_event["error"]
            .as_str()
            .is_some_and(|err| err.contains("panic")),
        "{panic_event}"
    );
    wait_for(
        || cap(&state, "solver")["sha256"].as_str() == Some(fixtures.solver["v1"].sha256.as_str()),
        "panic rollback hash",
        RELOAD_WAIT,
    )
    .await?;
    assert_ne!(
        cap(&state, "solver")["uid"].as_u64(),
        Some(uid_before_panic),
        "rollback must rebuild the solver fiber"
    );
    wait_for(
        || evaluator_fiber.state() == FiberState::Active,
        "evaluator reactivation after panic",
        RELOAD_WAIT,
    )
    .await?;
    assert_eq!(run_suite(&app).await?["passed"], 5);
    write_status(&app, &state, &status_path, &reloads);

    // ---- callback panic remains one host-written Err ------------------------
    admit(&fixtures, "crash")?;
    fixtures.publish_to(&live, "crash")?;
    wait_for(
        || {
            cap(&state, "solver")["sha256"].as_str()
                == Some(fixtures.solver["crash"].sha256.as_str())
        },
        "crash reload",
        RELOAD_WAIT,
    )
    .await?;
    wait_for(
        || evaluator_fiber.state() == FiberState::Active,
        "evaluator reactivation after crash",
        RELOAD_WAIT,
    )
    .await?;
    sink.marker("crash");
    let crash = app
        .root
        .call_iface("solver@1", 1, "solve", json!(["crash"]))
        .await;
    sink.marker("after-crash");
    assert!(crash.is_err(), "crash callback must fail: {crash:?}");
    let crash_rows = window(&sink.lines(), "crash");
    let crashed: Vec<_> = crash_rows
        .iter()
        .filter(|row| row["name"] == "solver@1.solve")
        .collect();
    assert_eq!(crashed.len(), 1, "{crash_rows:?}");
    assert_eq!(crashed[0]["status"], "err");
    assert_eq!(crashed[0]["code"], "panic");
    assert_eq!(crashed[0]["callee_plugin"], "rsi-solver");
    assert_ne!(crashed[0]["call_id"], 0);
    write_status(&app, &state, &status_path, &reloads);

    // ---- in-flight: caller survives the provider unload ---------------------
    fixtures.publish_to(&live, "v1")?;
    wait_for(
        || cap(&state, "solver")["sha256"].as_str() == Some(fixtures.solver["v1"].sha256.as_str()),
        "crash -> v1",
        RELOAD_WAIT,
    )
    .await?;
    held.store(false, Ordering::SeqCst);
    let old_uid = cap(&state, "solver")["uid"].as_u64().unwrap();
    let pending = {
        let root = app.root.clone();
        tokio::spawn(async move { root.call_iface("solver@1", 1, "hold", Config::Null).await })
    };
    wait_for(
        || held.load(Ordering::SeqCst),
        "solver hold pending",
        Duration::from_secs(30),
    )
    .await?;
    sink.marker("inflight");
    fixtures.publish_to(&live, "v2")?;
    let held_result = tokio::time::timeout(RELOAD_WAIT, pending)
        .await
        .map_err(|_| "in-flight hold timed out".to_string())?
        .map_err(|err| err.to_string())?;
    wait_for(
        || cap(&state, "solver")["uid"].as_u64() != Some(old_uid),
        "in-flight provider unload",
        RELOAD_WAIT,
    )
    .await?;
    assert!(
        held_result.is_err(),
        "in-flight hold must settle as an error: {held_result:?}"
    );
    let inflight = window(&sink.lines(), "inflight");
    let hold_rows: Vec<_> = inflight
        .iter()
        .filter(|row| row["name"] == "solver@1.hold")
        .collect();
    assert_eq!(hold_rows.len(), 1, "{inflight:?}");
    assert_eq!(hold_rows[0]["status"], "err");
    assert_eq!(hold_rows[0]["code"], "host_lifecycle");
    assert_eq!(hold_rows[0]["callee_uid"], old_uid);
    assert_eq!(hold_rows[0]["caller_uid"], UID_ROOT);

    // ---- in-flight: caller is a dependent of the provider -------------------
    wait_for(
        || {
            state
                .fiber_of("pipeline")
                .is_some_and(|fiber| fiber.state() == FiberState::Active)
        },
        "pipeline active before dependent in-flight",
        RELOAD_WAIT,
    )
    .await?;
    held.store(false, Ordering::SeqCst);
    sink.marker("inflight-a");
    let pending_pipeline = {
        let root = app.root.clone();
        tokio::spawn(async move {
            root.call_iface("pipeline@1", 1, "run_hold", Config::Null)
                .await
        })
    };
    wait_for(
        || held.load(Ordering::SeqCst),
        "pipeline hold pending",
        Duration::from_secs(30),
    )
    .await?;
    fixtures.publish_to(&live, "v1")?;
    let pipeline_result = tokio::time::timeout(RELOAD_WAIT, pending_pipeline)
        .await
        .map_err(|_| "dependent in-flight call timed out".to_string())?
        .map_err(|err| err.to_string())?;
    assert!(
        pipeline_result.is_err(),
        "dependent in-flight call must fail: {pipeline_result:?}"
    );
    wait_for(
        || cap(&state, "solver")["sha256"].as_str() == Some(fixtures.solver["v1"].sha256.as_str()),
        "dependent in-flight provider swap",
        RELOAD_WAIT,
    )
    .await?;
    let dependent_rows = window(&sink.lines(), "inflight-a");
    assert!(
        !dependent_rows
            .iter()
            .any(|row| row["name"] == "solver@1.hold"),
        "a dropped dependent future must not produce a solver hop: {dependent_rows:?}"
    );
    let pipeline_hold: Vec<_> = dependent_rows
        .iter()
        .filter(|row| row["name"] == "pipeline@1.run_hold")
        .collect();
    assert_eq!(pipeline_hold.len(), 1, "{dependent_rows:?}");
    assert_eq!(pipeline_hold[0]["status"], "err");
    assert_eq!(pipeline_hold[0]["code"], "host_lifecycle");
    write_status(&app, &state, &status_path, &reloads);

    // ---- same App + cleanup -------------------------------------------------
    assert!(matches!(
        evaluator_fiber.state(),
        FiberState::Active | FiberState::Pending
    ));
    assert!(
        evaluator_fiber.error().is_none(),
        "the evaluator fiber must survive the reloads without a failure: {:?}",
        evaluator_fiber.error()
    );
    let trace = sink.lines();
    drop(state);
    app.close().await;
    Ok(Report {
        v1,
        v2,
        v1_again,
        trace,
    })
}

async fn run_suite(app: &App) -> Result<Value, String> {
    app.root
        .call_iface("evaluator@1", 1, "run_suite", Config::Null)
        .await
        .map_err(|err| err.to_string())
}

fn contract_gate(baseline: &[String], candidate: &[String]) -> Result<(), String> {
    let mut baseline = baseline.to_vec();
    let mut candidate = candidate.to_vec();
    baseline.sort();
    baseline.dedup();
    candidate.sort();
    candidate.dedup();
    if baseline == candidate {
        Ok(())
    } else {
        Err(format!(
            "service method set changed: baseline={baseline:?} candidate={candidate:?}"
        ))
    }
}

async fn candidate_methods(fixtures: &Fixtures, name: &str) -> Result<Vec<String>, String> {
    let path = fixtures.dir.path().join("contract.yaml");
    std::fs::write(&path, format!("- id: solver\n  name: {name}\n"))
        .map_err(|err| err.to_string())?;
    let report = cordis_cli::status_with_source(
        &path,
        cordis_cli::RunOptions {
            plugin_dir: Some(fixtures.dir.path().to_path_buf()),
            ..Default::default()
        },
        Arc::new(DiskPluginSource),
    )
    .await
    .map_err(|err| err.to_string())?;
    report
        .capabilities
        .iter()
        .find(|row| row.label == "solver")
        .and_then(|row| row.services.first())
        .map(|service| service.methods.clone())
        .ok_or_else(|| format!("candidate {name} has no solver service: {report:?}"))
}

fn cap(state: &LoaderState, label: &str) -> Value {
    state
        .capabilities()
        .into_iter()
        .find(|row| row["label"] == label)
        .unwrap_or(Value::Null)
}

fn case_ok(summary: &Value, id: u64) -> bool {
    summary["cases"]
        .as_array()
        .and_then(|cases| cases.iter().find(|case| case["id"] == id))
        .and_then(|case| case["ok"].as_bool())
        .unwrap_or(false)
}

fn write_status(
    app: &App,
    state: &LoaderState,
    path: &std::path::Path,
    reloads: &Arc<Mutex<Vec<Value>>>,
) {
    let mut report = state.report();
    report["ts_ms"] = json!(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0));
    report["reloads"] = Value::Array(reloads.lock().unwrap().clone());
    let mut rows = state.capabilities();
    for row in &mut rows {
        let services = row["label"]
            .as_str()
            .and_then(|label| state.fiber_of(label))
            .map(|fiber| {
                app.store
                    .services_of(&fiber)
                    .into_iter()
                    .map(|service| {
                        json!({
                            "name": service.name,
                            "version": service.version,
                            "methods": service.methods,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        row["services"] = Value::Array(services);
    }
    report["capabilities"] = Value::Array(rows);
    let _ = std::fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&report).unwrap()),
    );
}

fn admit(fixtures: &Fixtures, variant: &str) -> Result<(), String> {
    admission::verify(
        &fixtures.solver[variant].path,
        &fixtures.eval_records[variant],
        &fixtures.solver[variant].spec,
    )
}

async fn wait_for<F>(mut predicate: F, what: &str, budget: Duration) -> Result<(), String>
where
    F: FnMut() -> bool,
{
    let started = std::time::Instant::now();
    while started.elapsed() < budget {
        if predicate() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(format!("timed out waiting for {what} after {budget:?}"))
}

struct ResetPolicy;

impl Drop for ResetPolicy {
    fn drop(&mut self) {
        cordis_host::source::set_active_policy(None);
    }
}
