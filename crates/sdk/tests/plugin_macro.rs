//! M6 红测试：`#[cordis::plugin]` 与 `#[cordis::config]`（G6/G8/G12 的声明面）。
//!
//! `#[cordis::plugin]` 把插件的**声明**（name / inject / provision / volatile / config）
//! 收进语法树，并生成 `impl Plugin`——`apply` 由作者写成普通 `async fn`。
//!
//! 两条必须写死的契约：
//! - **`Clone`**：`Plugin::apply` 返回 `'static` future，不能借用 `&self` ⇒ 宏在调用前克隆
//!   一份（共享状态请放 `Arc`，与既有示例/夹具的写法一致）；
//! - **声明照字面写**：`inject(tools)` 是裸名 `"tools"`，需要命名空间时写 `inject("tools@1")`
//!   （宏不替你猜 major——那是 `#[cordis::service]` 的 `KEY` 的职责）。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use cordis::core::{App, Config, Ctx, Dyn, ExternValue, FiberState, Inject, Plugin};
use cordis::{config, plugin};
use serde_json::json;

#[config]
#[derive(Clone, Debug, PartialEq)]
pub struct LoopConfig {
    pub concurrency: u32,
    pub prompt: String,
}

#[plugin(
    name = "agent-loop",
    version = "1.2.3",
    inject("greeter@1", llm),
    provision("agentLoop@1"),
    volatile("cache.dir"),
    schema = r#"{"type":"object","required":["prompt"]}"#,
    config(LoopConfig)
)]
#[derive(Clone)]
pub struct AgentLoop {
    /// 被克隆的那一份仍然共享同一个计数器：证明 `apply` 确实跑到了（且状态放 `Arc`）。
    pub applied: Arc<AtomicU32>,
}

impl AgentLoop {
    async fn apply(&self, ctx: Ctx, config: Config) -> cordis::core::Result<()> {
        let cfg = LoopConfig::parse(config)?;
        self.applied.fetch_add(1, Ordering::SeqCst);
        ctx.provide(
            "agentLoop@1",
            Dyn::new(ExternValue(json!({ "prompt": cfg.prompt }))),
        )?;
        Ok(())
    }
}

fn agent_loop() -> AgentLoop {
    AgentLoop {
        applied: Arc::new(AtomicU32::new(0)),
    }
}

#[test]
fn plugin_macro_emits_inject_provision_and_volatile() {
    let plugin: Arc<dyn Plugin> = Arc::new(agent_loop());
    assert_eq!(plugin.name(), "agent-loop");
    let inject: Vec<String> = plugin
        .inject()
        .0
        .iter()
        .map(|(name, _)| name.to_string())
        .collect();
    assert_eq!(inject, vec!["greeter@1".to_string(), "llm".to_string()]);
    assert_eq!(plugin.provision(), vec!["agentLoop@1".to_string()]);
    assert_eq!(plugin.volatile_paths(), vec!["cache.dir".to_string()]);
    assert_eq!(plugin.version(), Some("1.2.3"));
    assert_eq!(
        plugin.config_schema().as_deref(),
        Some(r#"{"type":"object","required":["prompt"]}"#)
    );
}

#[tokio::test]
async fn plugin_macro_apply_hook_runs_and_config_is_validated() {
    let app = App::new();
    // 依赖先就位：裸名与 `@major` 键都走同一张服务表。
    let deps = cordis::core::plugin_fn("deps", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("llm", Dyn::new(ExternValue(json!({}))))?;
            ctx.provide(
                "greeter@1",
                Dyn::new(ExternValue(json!({ "value": "stub" }))),
            )?;
            Ok(())
        })
    });
    app.plugin(deps, Config::Null).await.unwrap();

    // 1) 配置不合 schema ⇒ **挂载期**就失败（`Plugin::validate`），不是 apply 里的运行期惊喜。
    let loop_plugin = Arc::new(agent_loop());
    let bad = app
        .plugin(
            loop_plugin.clone(),
            json!({ "concurrency": "many", "prompt": "x" }),
        )
        .await;
    let message = match bad {
        Ok(fiber) => format!("state={:?}", fiber.state()),
        Err(err) => err.to_string(),
    };
    assert!(
        message.contains("invalid config") || message.contains("Failed"),
        "非法配置必须在挂载期被拒：{message}"
    );
    assert_eq!(
        loop_plugin.applied.load(Ordering::SeqCst),
        0,
        "校验失败不得跑 apply"
    );

    // 2) 合规配置 ⇒ apply 跑到、服务装上。
    let fiber = app
        .plugin(
            loop_plugin.clone(),
            json!({ "concurrency": 2, "prompt": "hi" }),
        )
        .await
        .unwrap();
    assert_eq!(fiber.state(), FiberState::Active);
    assert_eq!(loop_plugin.applied.load(Ordering::SeqCst), 1);
    assert!(app.root.require("agentLoop@1").is_ok());

    // 3) `#[cordis::config]` 自己的往返与可读错误。
    assert_eq!(
        LoopConfig::parse(json!({ "concurrency": 1, "prompt": "p" })).unwrap(),
        LoopConfig {
            concurrency: 1,
            prompt: "p".to_string()
        }
    );
    let err = LoopConfig::parse(json!({ "concurrency": "x", "prompt": "p" })).unwrap_err();
    assert!(!err.issues.is_empty(), "解析失败要给出可读的 issue：{err}");
    assert!(err.to_string().contains("concurrency"), "{err}");
}

#[cordis::events]
pub enum KeywordEvents {
    #[event("r3/type")]
    Type(String),
}

#[cordis::service(name = "r3kw", version = 1)]
pub trait RawIdentService {
    async fn r#type(&self) -> Result<String, String>;
}

struct RawIdentImpl;

#[cordis::service_impl]
impl RawIdentService for RawIdentImpl {
    async fn r#type(&self) -> Result<String, String> {
        Ok("ok-type".to_string())
    }
}

fn default_flag() -> String {
    "defaulted".to_string()
}

#[config]
#[derive(Clone, Debug, PartialEq)]
pub struct DefaultedConfig {
    #[serde(default = "default_flag")]
    pub flag: String,
}

#[plugin(
    name = "r3-defaulted-plugin",
    volatile("flag"),
    config(DefaultedConfig)
)]
#[derive(Clone)]
pub struct DefaultedPlugin {
    pub saw_volatile: Arc<std::sync::Mutex<Option<String>>>,
}

impl DefaultedPlugin {
    async fn apply(&self, ctx: Ctx, _config: Config) -> cordis::core::Result<()> {
        *self.saw_volatile.lock().unwrap() = ctx
            .volatile("flag")
            .and_then(|v| v.get().as_str().map(str::to_string));
        Ok(())
    }
}

#[tokio::test]
async fn r3_p3_macros_keyword_event_raw_ident_method_and_config_defaults() {
    let app = App::new();

    // 1. Keyword variant `Type` in `#[cordis::events]` compiles and dispatches.
    let seen = Arc::new(std::sync::Mutex::new(None::<String>));
    let _h = KeywordEvents::on_type(&app.root, {
        let seen = seen.clone();
        move |val| {
            let seen = seen.clone();
            Box::pin(async move {
                *seen.lock().unwrap() = Some(val);
                Ok(())
            })
        }
    });
    KeywordEvents::r#type(&app.root, "hello".to_string()).await;
    assert_eq!(*seen.lock().unwrap(), Some("hello".to_string()));

    // 2. Raw-identifier method `r#type` exposes wire method name `"type"` (without `"r#"`).
    let _svc = RawIdentServiceService::provide(&app.root, Arc::new(RawIdentImpl)).unwrap();
    let res = app
        .root
        .call_iface("r3kw@1", 1, "type", json!([]))
        .await
        .unwrap();
    assert_eq!(res, json!("ok-type"));

    // 3. `#[cordis::plugin(volatile("flag"), config(DefaultedConfig))]` preserves serde defaults
    //    in `fiber.resolved()` and creates the `VolatileRef`.
    let saw_volatile = Arc::new(std::sync::Mutex::new(None));
    let fiber = app
        .plugin(
            Arc::new(DefaultedPlugin {
                saw_volatile: saw_volatile.clone(),
            }),
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(fiber.resolved(), json!({ "flag": "defaulted" }));
    assert_eq!(
        *saw_volatile.lock().unwrap(),
        Some("defaulted".to_string()),
        "serde 默认字段必须落入 fiber.resolved() 并成功构造 VolatileRef"
    );
    app.close().await;
}
