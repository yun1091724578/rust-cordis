//! 缺陷 5 / G11 面的**针对性回归**：卸载一个「依赖装载器行服务的原生 fiber」之后，
//! 那一行的插件对象（`Arc<dyn Plugin>`）必须能被回收。
//!
//! # 这条判据为什么就是"暂存副本能不能删"
//! 二进制层的观测是"`.rN` 副本不删"——`Arc<LoadedPlugin>` 活着 ⇒ `StagedFile` 不 drop。
//! 而 `Arc<dyn Plugin>` 的长期持有面只有两条：`Store.runtimes[..].plugin` 与
//! `Fiber.runtime → Runtime.plugin`；**两条都以"fiber 活着"为前提**。
//! 于是问题总是化成：插件卸载之后，谁还握着那条 fiber？
//!
//! 已知的两条链（本次一并钉住）：
//! 1. **`Context → Fiber` 曾经是强引用**（M4 的 G18 之前）：依赖者被卸载后，只要还有人
//!    留着它的 ctx（依赖者提交视图里的 `Impl`、宿主的 `Instance.ctx`、派生 intercept ctx），
//!    提供者的 fiber 就被一起吊住。G18 改成 `Weak<Fiber>`（根 fiber 由 `Store.root_fiber`
//!    强持）⇒ 链断。本文件的 (b)/(h)/(e) 三个变体就是它。
//! 2. **行 fiber 是"父 fiber 的一个 effect"**：父若是永不 dispose 的 root fiber，
//!    行 fiber 就永远回收不了（附录 E 缺陷 6）。所以本文件**必须**把装载器挂成一个
//!    插件 fiber（demo 的 `loader()` / spike 的 `harness-loader` 就是这个形态），
//!    收尾时拆它——否则测试量到的是缺陷 6，而不是缺陷 5。
//!
//! # 反空转
//! `the_probe_detects_a_deliberately_retained_plugin` 里假源**故意留一份**插件对象：
//! 判据必须因此为假。少了这条，"全绿"可能只是"探针没接上"。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cordis_core::{
    plugin_fn, App, BoxFuture, Config, Ctx, Dyn, Error, ExternService, Fiber, FiberState, Inject,
    Plugin, PluginKey,
};
use cordis_host::loader::{self, InterpolationMode, LoadedFile, LoaderConfig, PluginSource};
use cordis_host::patch::Entry;
use serde_json::{json, Value};

// ------------------------------------------------------------------ 探针

/// 插件对象的回收探针：`Drop` 打标。**本文件唯一的判据。**
struct SpyPlugin {
    inner: Arc<dyn Plugin>,
    dropped: Arc<AtomicBool>,
}

impl Plugin for SpyPlugin {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn key(&self) -> PluginKey {
        self.inner.key()
    }

    fn inject(&self) -> Inject {
        self.inner.inject()
    }

    fn provision(&self) -> Vec<String> {
        self.inner.provision()
    }

    fn volatile_paths(&self) -> Vec<String> {
        self.inner.volatile_paths()
    }

    fn apply(&self, ctx: Ctx, config: Config) -> BoxFuture<cordis_core::Result<()>> {
        self.inner.apply(ctx, config)
    }
}

impl Drop for SpyPlugin {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// 假源：每个 `load` 造一个"提供 `config.service` 的插件"的探针包装。
///
/// 服务的**值形态**与 ABI 提供者同形：`Dyn::new(...).with_adapter(adapter)`，
/// 而那个 adapter **握着提供者的 ctx**（真宿主里是 `PluginIface` 握着 `Instance.ctx`）。
/// 这正是"被留下的 ctx 吊住提供者 fiber"那条链的入口——测试必须覆盖它。
#[derive(Default)]
struct SpySource {
    dropped: Mutex<Vec<Arc<AtomicBool>>>,
    /// 行名是 `keeps` 时，把插件对象**故意留一份在这里**（反空转用）。
    kept: Mutex<Vec<Arc<dyn Plugin>>>,
}

impl SpySource {
    fn last(&self) -> Arc<AtomicBool> {
        self.dropped
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("至少装载过一次")
    }
}

/// 只认 `svc` 的一个方法的适配器（入参是裸 JSON，与 `greeting` 那种写法一致）。
struct CtxAdapter {
    ctx: Ctx,
}

impl ExternService for CtxAdapter {
    fn service(&self) -> &str {
        "svc"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["get".to_string()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        method: String,
        _args: Config,
    ) -> BoxFuture<Result<Config, Error>> {
        let ctx = self.ctx.clone();
        Box::pin(async move {
            match method.as_str() {
                "get" => {
                    let value = ctx.require_as::<u32>("svc").map(|value| *value)?;
                    Ok(json!(value))
                }
                other => Err(format!("没有方法 {other}").into()),
            }
        })
    }
}

impl PluginSource for SpySource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        let config = entry.config.clone().unwrap_or(Config::Null);
        let service = config
            .get("service")
            .and_then(Value::as_str)
            .unwrap_or("svc")
            .to_string();
        let value = config.get("value").and_then(Value::as_u64).unwrap_or(0) as u32;
        let inner: Arc<dyn Plugin> = plugin_fn(
            "provides",
            Inject::default(),
            move |ctx: Ctx, _cfg: Config| {
                let service = service.clone();
                Box::pin(async move {
                    let adapter = Arc::new(CtxAdapter { ctx: ctx.clone() });
                    ctx.provide(&service, Dyn::new(value).with_adapter(adapter))?;
                    Ok(())
                })
            },
        );
        let dropped = Arc::new(AtomicBool::new(false));
        self.dropped.lock().unwrap().push(dropped.clone());
        let plugin: Arc<dyn Plugin> = Arc::new(SpyPlugin { inner, dropped });
        if entry.name == "keeps" {
            self.kept.lock().unwrap().push(plugin.clone());
        }
        let path = PathBuf::from(format!("<fake:{}>", entry.name));
        Ok(LoadedFile {
            plugin,
            source: path.clone(),
            staged: path,
            fingerprint: None,
        })
    }
}

// ------------------------------------------------------------------ 夹具

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cordis-test-{}-{}-{tag}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).expect("建临时目录");
        Self(path)
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).expect("写配置文件");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config_for(path: &Path) -> LoaderConfig {
    LoaderConfig {
        path: path.to_string_lossy().into_owned(),
        patches: Vec::new(),
        plugin_dir: None,
        strict: true,
        strict_cycles: false,
        hmr: None,
        js_evaluator: None,
        interpolation: InterpolationMode::Builtin,
        dispatch_policy: None,
        host_limits: None,
    }
}

struct Fixture {
    app: App,
    /// 装载器**宿主** fiber：行 fiber 是它的孩子 ⇒ 收尾拆它才是 demo/spike 的真实序列。
    host: Arc<Fiber>,
    state: Arc<loader::LoaderState>,
    spy: Arc<AtomicBool>,
}

/// 装一行 `provides` 的行（行 fiber 挂在**可拆的**装载器宿主下面）。
async fn boot(tag: &str) -> Fixture {
    boot_row(tag, "provides").await
}

/// 同上，但行名是 `keeps`（假源会把插件对象故意留一份 ⇒ 判据应当为假）。
async fn boot_keeping(tag: &str) -> Fixture {
    boot_row(tag, "keeps").await
}

async fn boot_row(tag: &str, name: &str) -> Fixture {
    let dir = TempDir::new(tag);
    let source = Arc::new(SpySource::default());
    let path = dir.write(
        "cordis.yml",
        &format!("- id: provider\n  name: {name}\n  config:\n    service: svc\n    value: 7\n"),
    );
    let app = App::new();
    // 装载器宿主：**它**是那些行的父 fiber（照 demo 的 `loader()` 形态）。
    let slot: Arc<Mutex<Option<Arc<loader::LoaderState>>>> = Arc::new(Mutex::new(None));
    let config = config_for(&path);
    let host_plugin = {
        let slot = slot.clone();
        let source = source.clone();
        plugin_fn(
            "loader-host",
            Inject::default(),
            move |ctx: Ctx, _cfg: Config| {
                let slot = slot.clone();
                let source = source.clone();
                let config = config.clone();
                Box::pin(async move {
                    let state = loader::load_with_source(ctx.clone(), config, source).await?;
                    *slot.lock().unwrap() = Some(state);
                    Ok(())
                })
            },
        )
    };
    let host = app.root.plugin(host_plugin, Config::Null);
    host.settle().await;
    assert_eq!(host.state(), FiberState::Active, "装载器宿主必须激活");
    let state = slot
        .lock()
        .unwrap()
        .clone()
        .expect("装载器必须已经把状态放出来");
    assert_eq!(
        app.store.inventory().len(),
        2,
        "宿主 + 行各一条 runtime：{:?}",
        app.store.inventory()
    );
    Fixture {
        app,
        host,
        state,
        spy: source.last(),
    }
}

/// 收尾序列（与 demo/spike 逐字同形）：
/// ① 行还在服务 ⇒ 插件对象**不得**回收（否则这条测试会变成空测试）；
/// ② 卸载这一行（走装载器）⇒ runtime 必须被清掉；
/// ③ 拆装载器宿主 ⇒ 现在插件对象**必须**被回收。
async fn assert_released_after_teardown(fixture: &Fixture) {
    let Fixture {
        app,
        host,
        state,
        spy,
    } = fixture;
    assert!(
        !spy.load(Ordering::SeqCst),
        "行还在服务：插件对象不该被回收（否则这条测试会变成空测试）"
    );
    state
        .set_disabled("provider", Some(true))
        .await
        .expect("卸载这一行");
    assert!(
        app.store
            .inventory()
            .iter()
            .all(|(name, _, _)| name != "provides"),
        "行卸载后 runtime 必须被清掉：{:?}",
        app.store.inventory()
    );
    host.dispose().await;
    assert!(
        spy.load(Ordering::SeqCst),
        "装载器拆完之后 `Arc<dyn Plugin>` 必须被回收——它就是二进制层\
         \"暂存副本能否删除\"的等价观测（附录 E 缺陷 5）"
    );
}

// ------------------------------------------------------------------ 测试

/// 缺陷 5 变体 `(h)`：**依赖 + 调用 + 卸载依赖者**。
#[tokio::test]
async fn unloading_a_row_after_its_calling_dependent_was_disposed_releases_the_plugin() {
    let fixture = boot("release-h").await;

    let consumer = plugin_fn(
        "consumer",
        Inject::of(&["svc"]),
        |ctx: Ctx, _cfg: Config| {
            Box::pin(async move {
                let answer = ctx.call_iface("svc", 1, "get", json!([])).await?;
                assert_eq!(answer, json!(7));
                Ok(())
            })
        },
    );
    let consumer_fiber: Arc<Fiber> = fixture.app.root.plugin(consumer, Config::Null);
    consumer_fiber.settle().await;
    assert_eq!(
        consumer_fiber.state(),
        FiberState::Active,
        "消费者应当激活（它真的调到了服务）"
    );

    // 卸载**依赖者**：二进制层里这一步就是"原生调用者被拆掉"。
    consumer_fiber.dispose().await;
    assert_released_after_teardown(&fixture).await;
}

/// 缺陷 5 变体 `(b)`：**依赖 + 派生 intercept ctx + 卸载依赖者**（不调用）。
#[tokio::test]
async fn unloading_a_row_after_an_intercept_derived_dependent_was_disposed_releases_the_plugin() {
    let fixture = boot("release-b").await;

    let consumer = plugin_fn(
        "consumer",
        Inject::of(&["svc"]),
        |ctx: Ctx, _cfg: Config| {
            Box::pin(async move {
                // 派生一个叠了 use-site 元数据的子 context（M3 的 `ι`），并读一次依赖。
                let site = ctx.intercept("svc", json!({ "use-site": true }));
                let value = site.require_as::<u32>("svc").map(|value| *value)?;
                assert_eq!(value, 7);
                Ok(())
            })
        },
    );
    let consumer_fiber: Arc<Fiber> = fixture.app.root.plugin(consumer, Config::Null);
    consumer_fiber.settle().await;
    assert_eq!(consumer_fiber.state(), FiberState::Active);

    consumer_fiber.dispose().await;
    assert_released_after_teardown(&fixture).await;
}

/// 缺陷 5 变体 `(e)`：**依赖 + 调用 + 依赖者留在线上**（矩阵里这一条也必须干净）。
#[tokio::test]
async fn unloading_a_row_with_a_live_dependent_also_releases_the_plugin() {
    let fixture = boot("release-e").await;

    let consumer = plugin_fn(
        "consumer",
        Inject::of(&["svc"]),
        |ctx: Ctx, _cfg: Config| {
            Box::pin(async move {
                let _ = ctx.call_iface("svc", 1, "get", json!([])).await?;
                Ok(())
            })
        },
    );
    let consumer_fiber: Arc<Fiber> = fixture.app.root.plugin(consumer, Config::Null);
    consumer_fiber.settle().await;
    assert_eq!(consumer_fiber.state(), FiberState::Active);

    fixture
        .state
        .set_disabled("provider", Some(true))
        .await
        .expect("卸载这一行");
    consumer_fiber.settle().await;
    assert_eq!(
        consumer_fiber.state(),
        FiberState::Pending,
        "提供者走了 ⇒ 留在线上的依赖者退回未激活（cordis 的核心行为）"
    );
    fixture.host.dispose().await;
    assert!(
        fixture.spy.load(Ordering::SeqCst),
        "没有谁再握着那一行的 fiber ⇒ 插件对象必须被回收"
    );
}

/// 反空转：**故意留一份插件对象**时，判据必须为假。
#[tokio::test]
async fn the_probe_detects_a_deliberately_retained_plugin() {
    let fixture = boot_keeping("release-spy").await;
    fixture
        .state
        .set_disabled("provider", Some(true))
        .await
        .expect("卸载这一行");
    fixture.host.dispose().await;
    assert!(
        !fixture.spy.load(Ordering::SeqCst),
        "假源自己留着一份 ⇒ 判据必须为假，否则前三条测试是空转"
    );
}
