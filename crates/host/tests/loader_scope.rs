//! M4-1：行级 `isolate` / `intercept`（装载层补全）。
//!
//! 上游参照：`vendor/loader/src/config/isolate.ts`
//! - `isolate: {name: true}` → **行私有域**（LocalRealm，按行身份稳定，`:49-57`）；
//! - `isolate: {name: "<label>"}` → **具名共享域**（GlobalRealm，跨行共享，`:59-68`）；
//! - `intercept` → 行级配置链（`loader/patch-context` step 3，`:122-126`）；
//! - 组继承 = 子条目的映射**从父条目的映射派生**（step 1，`:97-101`）。
//!
//! 本仓的两处有意偏离（附录 A / §8 M4-1）：
//! 1. **具名域不回收**（上游 `partial-dispose` 会 GC，`:155-172`）——域是"服务地址"的
//!    一部分，回收它会让两代插件意外共享或失去隔离；数量进 `loader.report` 观测（P2-14）；
//! 2. isolate/intercept 变更走 **replace**（卸旧挂新），不做上游的槽位搬迁。
//!
//! 这些测试全部**不需要动态库**：`PluginSource` seam（M4-1 落地）用假插件顶替加载。

// clippy 1.98 会把已经是 `const { … }` 的初始化器继续报成“可改为 const”。
#![allow(clippy::missing_const_for_thread_local)]

use cordis_core::{App, Config, Ctx, Dyn, Inject, Plugin};
use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
use cordis_host::patch::Entry;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// 假插件源：按行名分派假插件；服务值取自该行的 `config.value`（默认 0）。
struct FakeSource;

impl PluginSource for FakeSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        let plugin: Arc<dyn Plugin> = match entry.name.as_str() {
            "provides" => {
                let value = entry
                    .config
                    .as_ref()
                    .and_then(|config| config.get("value"))
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0) as u32;
                cordis_core::plugin_fn("provides", Inject::default(), move |ctx: Ctx, _cfg| {
                    Box::pin(async move {
                        ctx.provide("svc", Dyn::new(value))?;
                        Ok(())
                    })
                })
            }
            "consumes" => {
                cordis_core::plugin_fn("consumes", Inject::default(), |ctx: Ctx, _cfg| {
                    Box::pin(async move {
                        let value = ctx
                            .require_as::<u32>("svc")
                            .map(|value| *value)
                            .unwrap_or(u32::MAX);
                        SEEN.with(|seen| seen.borrow_mut().push(value));
                        Ok(())
                    })
                })
            }
            other => return Err(format!("假插件源不认识 \"{other}\"")),
        };
        let path = PathBuf::from(format!("<fake:{0}>", entry.name));
        Ok(LoadedFile {
            plugin,
            source: path.clone(),
            staged: path,
            fingerprint: None,
        })
    }
}

thread_local! {
    /// `consumes` 插件读到的服务值（`u32::MAX` = 没读到）。
    static SEEN: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn seen() -> Vec<u32> {
    SEEN.with(|seen| seen.borrow().clone())
}

/// 每个用例一个临时目录（跨平台安全；中文路径也走 UTF-16 的 LoadLibraryW，
/// 这里虽然不加载真库，仍保持同一约定）。
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
        interpolation: cordis_host::loader::InterpolationMode::Builtin,
    }
}

/// 装一份配置（strict），返回 `(app, 每行的失败消息)`。
async fn load(app: &App, dir: &TempDir, name: &str, yaml: &str) -> Result<(), String> {
    let path = dir.write(name, yaml);
    load_with_source(app.root.clone(), config_for(&path), Arc::new(FakeSource))
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
}

fn report(app: &App) -> serde_json::Value {
    report_from(&app.root)
}

fn report_from(ctx: &Ctx) -> serde_json::Value {
    let value = ctx.require("loader.report").expect("loader.report");
    let exposed = value
        .downcast::<cordis_core::ExternValue>()
        .expect("ExternValue");
    exposed.0.clone()
}

/// M4-1（红→绿）：**组行**上的 `isolate: {svc: true}` 要传给子条目——
/// 组内的提供者与消费者落在同一个私有域里，因此能互相看到。
///
/// 现状（红）：`isolate` 字段被整条忽略（P2-1），提供者与消费者都在默认域，
/// 这个用例恰好会"通过"——所以真正的对照是下面第 2、3 条；
/// 本条锁定的是**组继承**这一半（`isolate.ts:97-101` 的派生链）。
#[tokio::test]
async fn provider_and_consumer_inside_group_isolate_see_each_other() {
    SEEN.with(|seen| seen.borrow_mut().clear());
    let app = App::new();
    let dir = TempDir::new("group-isolate");

    let result = load(
        &app,
        &dir,
        "cordis.yml",
        concat!(
            "- id: grp\n",
            "  name: grp\n",
            "  group: true\n",
            "  isolate:\n",
            "    svc: true\n",
            "  config:\n",
            "    - id: provider\n",
            "      name: provides\n",
            "      config:\n",
            "        value: 7\n",
            "    - id: consumer\n",
            "      name: consumes\n",
            "      inject: [\"svc\"]\n",
        ),
    )
    .await;
    assert!(result.is_ok(), "两个子条目都应装载成功：{result:?}");
    assert_eq!(seen(), vec![7], "组内的消费者必须读到组内提供者的服务");

    let report = report(&app);
    assert_eq!(report["mounted"], serde_json::json!(2));
}

/// M4-1（红→绿）：**行私有域**让两行互不干扰——同名服务在各自的私有域里各有一套，
/// 默认域读者看不到任何一套。
#[tokio::test]
async fn private_isolate_keeps_rows_independent() {
    let app = App::new();
    let dir = TempDir::new("private-isolate");

    let result = load(
        &app,
        &dir,
        "cordis.yml",
        concat!(
            "- id: a\n",
            "  name: provides\n",
            "  isolate:\n",
            "    svc: true\n",
            "  config:\n",
            "    value: 1\n",
            "- id: b\n",
            "  name: provides\n",
            "  isolate:\n",
            "    svc: true\n",
            "  config:\n",
            "    value: 2\n",
        ),
    )
    .await;
    // 现状（红）：两行的 `svc` 都在默认域 ⇒ 第二行 provide 冲突 ⇒ strict 装载失败。
    assert!(result.is_ok(), "两行各有私有域，不得互相撞名：{result:?}");
    assert_eq!(report(&app)["mounted"], serde_json::json!(2));
    // 默认域里没有 `svc`（私有域不外泄）。
    assert!(
        app.root.get("svc").is_none(),
        "私有域的服务不得出现在默认域"
    );
}

/// M4-1（红→绿）：**具名共享域**让两个不同的行共享同一套服务。
#[tokio::test]
async fn shared_label_lets_another_row_consume() {
    SEEN.with(|seen| seen.borrow_mut().clear());
    let app = App::new();
    let dir = TempDir::new("shared-label");

    let result = load(
        &app,
        &dir,
        "cordis.yml",
        concat!(
            "- id: provider\n",
            "  name: provides\n",
            "  isolate:\n",
            "    svc: shared\n",
            "  config:\n",
            "    value: 9\n",
            "- id: consumer\n",
            "  name: consumes\n",
            "  isolate:\n",
            "    svc: shared\n",
            "  inject: [\"svc\"]\n",
        ),
    )
    .await;
    assert!(result.is_ok(), "共享域里的消费者应当读到提供者：{result:?}");
    assert_eq!(seen(), vec![9], "共享标签 ⇒ 同一个域");
    assert!(app.root.get("svc").is_none(), "具名域同样不外泄到默认域");
}

/// M4-1（红→绿）：**同一具名域里抢同一个服务名必须 fail loud，并点出提供者身份**。
///
/// 这也是唯一需要"两行同名服务互撞"的场景：共享域是显式声明的约定，
/// 撞名是配置错误，必须让用户看得见是谁占着名字。
#[tokio::test]
async fn label_collision_fails_loud_with_provider_identity() {
    let app = App::new();
    let dir = TempDir::new("collision");

    let result = load(
        &app,
        &dir,
        "cordis.yml",
        concat!(
            "- name: provides\n",
            "  isolate:\n",
            "    svc: shared\n",
            "  config:\n",
            "    value: 1\n",
            "- name: provides\n",
            "  isolate:\n",
            "    svc: shared\n",
            "  config:\n",
            "    value: 2\n",
        ),
    )
    .await;

    let error = result.expect_err("同一具名域里的第二个提供者必须失败");
    assert!(error.contains("svc"), "错误必须点名服务名：{error}");
    assert!(
        error.contains("已被") && error.contains("注册"),
        "错误必须点出提供者身份：{error}"
    );
    // 稳定 label 让两条同名行都能被指认（P2-15：不再互相覆盖）。
    assert!(
        error.contains("provides#1"),
        "失败行必须用稳定 label 指认：{error}"
    );
}

/// M4-1（红→绿）：具名域 id 稳定——同一 `(服务名, 标签)` 永远映射到**同一个** realm
/// （具名域存在 `Store` 里、不回收），所以**第二次装载**的消费者仍能读到第一次装载的
/// 提供者。
///
/// 说明：真·跨 reconcile 的稳定性属 M4-3（对账状态机）；本仓在 M4-1 先把
/// "具名域驻留在 Store、跨装载复用"这一半钉住。
#[tokio::test]
async fn named_realm_id_is_stable_across_loads() {
    SEEN.with(|seen| seen.borrow_mut().clear());
    let app = App::new();
    let dir = TempDir::new("stable-realm");

    // 第一次装载：提供者 + 消费者进同一个具名域。
    load(
        &app,
        &dir,
        "one.yml",
        concat!(
            "- id: provider\n",
            "  name: provides\n",
            "  isolate:\n",
            "    svc: shared\n",
            "  config:\n",
            "    value: 5\n",
            "- id: consumer\n",
            "  name: consumes\n",
            "  isolate:\n",
            "    svc: shared\n",
            "  inject: [\"svc\"]\n",
        ),
    )
    .await
    .expect("第一次装载");
    assert_eq!(seen(), vec![5]);
    // M4-6：报告的域观测面从 `named_realms` 改成 `realms:{default,named}`（`named` 是同一个计数）。
    assert_eq!(report(&app)["realms"]["named"], serde_json::json!(1));

    // Store 级契约：同一 (name, label) 永远拿到同一个 id，且不重复分配。
    let realm = app.store.named_realm("svc", "shared");
    assert_eq!(
        app.store.named_realm("svc", "shared"),
        realm,
        "同一 (name,label) ⇒ 同一 id"
    );
    assert_eq!(app.store.named_realm_count(), 1, "复用而不是新分配");

    // 一个**后到的行**：手动挂进同一个具名域，必须看见第一次装载的提供者。
    // （第二个装载器实例会在 `loader.report` 上撞名——那本身就是内核的正确行为：
    //   它是 Store 级的默认域单例。这里改为直接验证"域被复用"这一条契约。）
    let late_ctx = app.root.isolate("svc", Some(realm));
    let plugin = cordis_core::plugin_fn("late-consumer", Inject::of(&["svc"]), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            let value = ctx
                .require_as::<u32>("svc")
                .map(|value| *value)
                .unwrap_or(u32::MAX);
            SEEN.with(|seen| seen.borrow_mut().push(value));
            Ok(())
        })
    });
    let late = late_ctx.plugin(plugin, Config::Null);
    late.settle().await;
    assert_eq!(
        late.state(),
        cordis_core::FiberState::Active,
        "同一具名域 ⇒ 依赖可见"
    );
    assert_eq!(seen(), vec![5, 5], "后到的行复用同一个域，而不是落进新域");
}
