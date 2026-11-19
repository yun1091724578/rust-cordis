use super::*;
use crate::{App, BoxFuture, Config};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker};

struct Collector(Arc<Mutex<Vec<TraceRecord>>>);

impl TraceSink for Collector {
    fn record(&self, record: TraceRecord) {
        self.0.lock().unwrap().push(record);
    }
}

fn collect(app: &App) -> Arc<Mutex<Vec<TraceRecord>>> {
    let records = Arc::new(Mutex::new(Vec::new()));
    assert!(app.set_trace_sink(Arc::new(Collector(records.clone()))));
    records
}

#[tokio::test]
async fn missing_service_is_recorded_with_call_id_zero() {
    let app = App::new();
    let records = collect(&app);
    let result = app.root.call_iface("nope", 1, "run", Config::Null).await;
    assert!(result.is_err());
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 1, "解析失败必须恰好一条记录");
    let record = &records[0];
    assert_eq!(record.call_id, 0);
    assert_eq!(record.parent_call_id, 0);
    assert_eq!(record.callee_uid, 0);
    assert_eq!(&*record.callee_plugin, "nope");
    assert_eq!(&*record.name, "nope.run");
    assert_eq!(record.outcome, Outcome::Err("resolve_failed".into()));
    assert_eq!(record.caller_uid, crate::fiber::UID_ROOT);
}

#[tokio::test]
async fn caller_uid_is_scoped_to_the_poll() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let inner = {
        let seen = seen.clone();
        async move {
            seen.lock().unwrap().push(current_caller_uid());
            Ok(Config::Null)
        }
    };
    let future = scope_caller(Box::pin(inner), 7);
    assert_eq!(current_caller_uid(), 0, "作用域外必须是 0");
    let value = future.await.unwrap();
    assert_eq!(value, Config::Null);
    assert_eq!(&*seen.lock().unwrap(), &[7]);
    assert_eq!(current_caller_uid(), 0, "poll 结束必须恢复上一个值");
}

#[test]
fn caller_uid_is_restored_after_inner_panic() {
    let inner: BoxFuture<Result<Config>> = Box::pin(async { panic!("poll 边界 panic") });
    let mut future = scope_caller(inner, 7);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let caught = catch_unwind(AssertUnwindSafe(|| Pin::new(&mut future).poll(&mut cx)));
    assert!(caught.is_err(), "内层 poll 必须真的 panic");
    assert_eq!(current_caller_uid(), 0, "unwind 也必须恢复 caller 作用域");
}

struct CheckService;

impl crate::ExternService for CheckService {
    fn service(&self) -> &str {
        "check@1"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["run".into()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> BoxFuture<Result<Config>> {
        Box::pin(async { Ok(Config::Null) })
    }
}

#[tokio::test]
async fn iface_check_failures_are_recorded_with_the_request_name() {
    let app = App::new();
    let records = collect(&app);
    app.root
        .provide_iface("check@1", Arc::new(CheckService))
        .expect("provide");

    app.root
        .call_iface("check@1", 2, "run", Config::Null)
        .await
        .expect_err("版本过高必须失败");
    app.root
        .call_iface("check@1", 1, "nope", Config::Null)
        .await
        .expect_err("未知方法必须失败");

    let records = records.lock().unwrap();
    assert_eq!(records.len(), 2, "两次校验失败各一条记录：{records:?}");
    for record in records.iter() {
        assert_eq!(record.call_id, 0);
        assert_eq!(record.parent_call_id, 0);
        assert_eq!(record.callee_uid, 0);
        assert_eq!(&*record.callee_plugin, "check@1");
        assert_eq!(record.outcome, Outcome::Err("iface_check_failed".into()));
    }
    assert_eq!(&*records[0].name, "check@1.run");
    assert_eq!(&*records[1].name, "check@1.nope");
}

struct PanicSink;

impl TraceSink for PanicSink {
    fn record(&self, _record: TraceRecord) {
        panic!("sink 自己 panic");
    }
}

#[test]
fn trace_sink_panic_is_contained() {
    let app = App::new();
    assert!(app.set_trace_sink(Arc::new(PanicSink)));
    app.root.store.record_trace(TraceRecord {
        ts_ms: 0,
        name: Box::from("x.run"),
        call_id: 0,
        parent_call_id: 0,
        caller_uid: crate::fiber::UID_ROOT,
        callee_uid: 0,
        callee_plugin: Box::from("x"),
        elapsed: std::time::Duration::from_millis(1),
        outcome: Outcome::Ok,
    });
    assert_eq!(current_caller_uid(), 0, "sink panic 不得污染调用作用域");
}

#[test]
fn installing_the_sink_twice_is_rejected() {
    let app = App::new();
    let _records = collect(&app);
    assert!(
        !app.set_trace_sink(Arc::new(Collector(Arc::new(Mutex::new(Vec::new()))))),
        "只允许安装一次"
    );
}
