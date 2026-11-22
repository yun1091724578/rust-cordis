use super::*;
use std::process::{Command, Stdio};
use std::sync::{Barrier, TryLockError};
use std::time::{Duration, Instant};

struct RemoveGate {
    target: u64,
    reached: Barrier,
    release: Barrier,
}

static REMOVE_GATE: Mutex<Option<Arc<RemoveGate>>> = Mutex::new(None);

pub(super) fn before_remove(target: u64) {
    let gate = {
        let mut slot = REMOVE_GATE.lock().unwrap();
        if slot.as_ref().is_some_and(|gate| gate.target == target) {
            slot.take()
        } else {
            None
        }
    };
    if let Some(gate) = gate {
        gate.reached.wait();
        gate.release.wait();
    }
}

// A poisoned table can turn a later ABI call into an abort. Keep it (and the
// registry/drop probes) isolated; the parent must observe a successful child.
fn in_child(name: &str) -> bool {
    const CHILD: &str = "CORDIS_O2_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return true;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("derived_dispose_tests::{name}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, name)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "O2 child {name} failed: {status}");
            return false;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let status = child.wait().unwrap();
            panic!("O2 child {name} watchdog expired: {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

unsafe extern "C" fn unused_call(_: u64, _: u32, _: Blob, _: Blob, _: *mut Out) -> i32 {
    STATUS_OK
}

fn root(ctx: Ctx) -> u64 {
    register_instance_with_keepalive(ctx, unused_call, None, 0).unwrap()
}

fn call(f: impl FnOnce(*mut Out) -> i32) -> (i32, String) {
    let mut buffer = [0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = f(&mut out);
    assert!(out.len <= buffer.len());
    (status, String::from_utf8_lossy(&buffer[..out.len]).into())
}

fn isolate(handle: u64) -> u64 {
    let (status, text) = call(|out| unsafe {
        host_ctx_isolate(handle, Blob::new(b"o2-service"), Blob::new(b""), out)
    });
    assert_eq!(status, STATUS_OK, "{text}");
    serde_json::from_str::<Value>(&text).unwrap()["handle"]
        .as_u64()
        .unwrap()
}

fn dispose(handle: u64, target: u64) -> (i32, String) {
    let target = json!({ "handle": target }).to_string();
    call(|out| unsafe { host_ctx_dispose(handle, Blob::new(target.as_bytes()), out) })
}

fn readable(handle: u64) {
    let (status, text) =
        call(|out| unsafe { host_ctx_get(handle, Blob::new(b"o2-absent-service"), out) });
    assert_eq!(status, STATUS_OK, "unrelated derived handle failed: {text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!({ "missing": true })
    );
}

fn race(unregister: bool) {
    let app = cordis_core::App::new();
    let owner = root(app.root.clone());
    let target = isolate(owner);
    let other = root(app.root.clone());
    let survivor = isolate(other);
    let gate = Arc::new(RemoveGate {
        target,
        reached: Barrier::new(2),
        release: Barrier::new(2),
    });
    *REMOVE_GATE.lock().unwrap() = Some(gate.clone());
    let a = std::thread::spawn(move || dispose(owner, target));
    gate.reached.wait();

    let locked = match derived_table().try_lock() {
        Ok(table) => {
            assert!(table.contains_key(&target));
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Poisoned(_)) => panic!("table poisoned before competing removal"),
    };
    // Old code is between locks: B removes before A resumes, deterministically
    // hitting remove.expect. Atomic code holds the lock: release A first rather
    // than waiting for B under that lock (which would deadlock the test itself).
    if locked {
        gate.release.wait();
    }
    let b = if unregister {
        unregister_instance(owner);
        None
    } else {
        Some(dispose(owner, target))
    };
    if !locked {
        gate.release.wait();
    }
    let a = a.join().expect("dispose thread panicked");
    if let Some(b) = b {
        assert_eq!(
            usize::from(a.0 == STATUS_OK) + usize::from(b.0 == STATUS_OK),
            1
        );
        let rejected = if a.0 == STATUS_OK { &b } else { &a };
        assert_eq!(rejected.0, STATUS_ERR);
        assert!(
            rejected.1.contains("已失效"),
            "unexpected failure: {rejected:?}"
        );
    } else {
        assert_eq!(
            a.0, STATUS_OK,
            "dispose should finish before root drain: {a:?}"
        );
    }
    assert!(
        !derived_table().is_poisoned(),
        "competing removal poisoned the table"
    );
    assert!(!derived_table().lock().unwrap().contains_key(&target));
    readable(survivor);
    assert_eq!(dispose(other, survivor).0, STATUS_OK);
    unregister_instance(owner);
    unregister_instance(other);
}

#[test]
fn o2_double_dispose_is_atomic() {
    if in_child("o2_double_dispose_is_atomic") {
        race(false);
    }
}

#[test]
fn o2_dispose_vs_root_unregister_is_atomic() {
    if in_child("o2_dispose_vs_root_unregister_is_atomic") {
        race(true);
    }
}

#[test]
fn o2_wrong_owner_preserves_entry() {
    if !in_child("o2_wrong_owner_preserves_entry") {
        return;
    }
    let app = cordis_core::App::new();
    let owner = root(app.root.clone());
    let other = root(app.root.clone());
    let target = isolate(owner);
    let denied = dispose(other, target);
    assert_eq!(denied.0, STATUS_ERR);
    assert!(denied.1.contains("不属于"), "{denied:?}");
    readable(target);
    assert_eq!(dispose(owner, target).0, STATUS_OK);
    assert_eq!(dispose(owner, target).0, STATUS_ERR);
    unregister_instance(owner);
    unregister_instance(other);
}

struct ChildRegistry(Arc<dyn Plugin>);
impl NativeRegistry for ChildRegistry {
    fn native(&self, name: &str) -> Option<Arc<dyn Plugin>> {
        (name == "o2-child").then(|| self.0.clone())
    }
}

#[test]
fn o2_live_fiber_without_runtime_preserves_entry() {
    if !in_child("o2_live_fiber_without_runtime_preserves_entry") {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let app = cordis_core::App::new();
    let owner = root(app.root.clone());
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let done_tx = Arc::new(Mutex::new(Some(done_tx)));
    set_native_registry(Arc::new(ChildRegistry(cordis_core::plugin_fn(
        "o2-child",
        cordis_core::Inject::default(),
        move |ctx, _| {
            let done_tx = done_tx.clone();
            Box::pin(async move {
                ctx.effect(move |_| {
                    cordis_core::Disposable::new(move || {
                        Box::pin(async move {
                            if let Some(done) = done_tx.lock().unwrap().take() {
                                let _ = done.send(());
                            }
                        })
                    })
                });
                Ok(())
            })
        },
    ))));
    let (target, fiber) = runtime.block_on(async {
        let (status, text) = call(|out| unsafe {
            host_ctx_use(owner, Blob::new(b"@native:o2-child"), Blob::new(b"{}"), out)
        });
        assert_eq!(status, STATUS_OK, "{text}");
        let target = serde_json::from_str::<Value>(&text).unwrap()["handle"]
            .as_u64()
            .unwrap();
        let fiber = derived_table().lock().unwrap()[&target]
            .fiber
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        fiber.settle().await;
        assert_eq!(fiber.state(), cordis_core::FiberState::Active);
        (target, fiber)
    });
    assert!(tokio::runtime::Handle::try_current().is_err());
    let denied = dispose(owner, target);
    assert_eq!(denied.0, STATUS_ERR);
    assert!(denied.1.contains("异步运行时"), "{denied:?}");
    readable(target);
    assert_eq!(fiber.state(), cordis_core::FiberState::Active);
    runtime.block_on(async {
        assert_eq!(dispose(owner, target).0, STATUS_OK);
        assert!(!derived_table().lock().unwrap().contains_key(&target));
        tokio::time::timeout(Duration::from_secs(5), done_rx)
            .await
            .unwrap()
            .unwrap();
        app.close().await;
    });
    unregister_instance(owner);
}

type DropLog = Arc<Mutex<Vec<(&'static str, bool, bool)>>>;
struct DropProbe(&'static str, DropLog);
impl Drop for DropProbe {
    fn drop(&mut self) {
        let instances_free = instances().try_write().is_ok();
        let derived_free = derived_table().try_lock().is_ok();
        self.1
            .lock()
            .unwrap()
            .push((self.0, instances_free, derived_free));
    }
}

fn probed(ctx: &Ctx, label: &'static str, log: &DropLog) -> Ctx {
    let probe = DropProbe(label, log.clone());
    ctx.filter(Arc::new(move |_| {
        let _ = &probe;
        true
    }))
}

fn probe_entry(target: u64, label: &'static str, log: &DropLog) {
    let mut table = derived_table().lock().unwrap();
    let entry = table.get_mut(&target).unwrap();
    entry.ctx = probed(&entry.ctx, label, log);
}

#[test]
fn o2_fiberless_dispose_without_runtime_drops_synchronously() {
    if !in_child("o2_fiberless_dispose_without_runtime_drops_synchronously") {
        return;
    }
    let app = cordis_core::App::new();
    let owner = root(app.root.clone());
    let isolated = isolate(owner);
    let (status, text) = call(|out| unsafe {
        host_ctx_intercept(owner, Blob::new(b"o2-service"), Blob::new(b"{}"), out)
    });
    assert_eq!(status, STATUS_OK, "{text}");
    let intercepted = serde_json::from_str::<Value>(&text).unwrap()["handle"]
        .as_u64()
        .unwrap();
    let log = DropLog::default();
    probe_entry(isolated, "isolate", &log);
    probe_entry(intercepted, "intercept", &log);
    assert!(tokio::runtime::Handle::try_current().is_err());
    for target in [isolated, intercepted] {
        assert!(derived_table().lock().unwrap()[&target].fiber.is_none());
        assert_eq!(dispose(owner, target).0, STATUS_OK);
        assert!(!derived_table().lock().unwrap().contains_key(&target));
    }
    assert_eq!(
        *log.lock().unwrap(),
        vec![("isolate", true, true), ("intercept", true, true)]
    );
    unregister_instance(owner);
}

#[test]
fn o2_root_unregister_drops_outside_both_tables() {
    if !in_child("o2_root_unregister_drops_outside_both_tables") {
        return;
    }
    let app = cordis_core::App::new();
    let log = DropLog::default();
    let owner = root(probed(&app.root, "instance", &log));
    unregister_instance(owner);

    let owner = root(app.root.clone());
    let target = isolate(owner);
    let other = root(app.root.clone());
    let survivor = isolate(other);
    probe_entry(target, "derived", &log);
    unregister_instance(owner);
    assert!(resolve_handle(owner).is_none());
    assert!(resolve_handle(target).is_none());
    assert_eq!(
        *log.lock().unwrap(),
        vec![("instance", true, true), ("derived", true, true)]
    );
    readable(survivor);
    assert_eq!(dispose(other, survivor).0, STATUS_OK);
    unregister_instance(other);
}
