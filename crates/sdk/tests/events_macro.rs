//! M6 红测试：`#[cordis::events]`（G12）——typed 事件面。
//!
//! 两条硬约束：
//! - **事件名逐字**（§0.3 取代清单⑦）：`"tools/before-invoke"` 就是这一个字符串，宏**绝不**
//!   加前缀/命名空间——ABI 侧拦截名是硬编码的，改了就静默失配；
//! - 载荷走 **JSON 信封**（`Dyn::new(ExternValue(json))`，与宿主 `json_to_args` 同一形态）：
//!   typed 监听器（T1）与动态库监听器（T2）看到的是**同一份载荷**，`next` 也能跨 ABI 接力。

use std::sync::Mutex;

use cordis::core::{App, Config, Ctx, Dyn, ExternValue, Inject, Ret};
use cordis::events;
use cordis::typed::events::decode_ret;
use serde_json::json;

#[derive(Clone, Debug, PartialEq, ::cordis::serde::Serialize, ::cordis::serde::Deserialize)]
#[serde(crate = "::cordis::serde")]
pub struct StepReport {
    pub round: u32,
}

#[derive(Clone, Debug, PartialEq, ::cordis::serde::Serialize, ::cordis::serde::Deserialize)]
#[serde(crate = "::cordis::serde")]
pub struct Invoke {
    pub tool: String,
    pub prompt: String,
}

#[derive(Clone, Debug, PartialEq, ::cordis::serde::Serialize, ::cordis::serde::Deserialize)]
#[serde(crate = "::cordis::serde")]
pub struct Decision {
    pub allow: bool,
    pub reason: String,
}

#[events]
#[derive(Clone, Debug)]
pub enum AgentEvents {
    /// 每轮开始（默认模式 = `emit`：观察，返回值丢弃）。
    #[event("agent/pre-step")]
    PreStep(StepReport),
    /// 工具调用前（`waterfall`：下游可改写载荷，链尾的决定回流）。
    #[event("tools/before-invoke", mode = "waterfall")]
    BeforeInvoke(Invoke),
    /// 谁先给答案谁赢（`bail`：第一个非 `None` 短路）。
    #[event("tool/decide", mode = "bail")]
    Decide(Decision),
    /// 顺序观察、不短路（`serial`）。
    #[event("tool/observe", mode = "serial")]
    Observe(Invoke),
}

static T7_SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T8_TYPED: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T8_RAW: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// M6-7：名字逐字 + 四种模式的语义（`emit` / `serial` / `bail` / `waterfall`）。
#[tokio::test]
async fn events_macro_generates_verbatim_names_and_dispatch_modes() {
    // 名字**逐字**：没有任何前缀、命名空间或大小写改写。
    assert_eq!(AgentEvents::PRE_STEP, "agent/pre-step");
    assert_eq!(AgentEvents::BEFORE_INVOKE, "tools/before-invoke");
    assert_eq!(AgentEvents::DECIDE, "tool/decide");
    assert_eq!(AgentEvents::OBSERVE, "tool/observe");

    let app = App::new();
    let listeners =
        cordis::core::plugin_fn("typed-listeners", Inject::default(), |ctx: Ctx, _cfg| {
            Box::pin(async move {
                // emit：监听器看得见 typed 载荷，返回值被丢弃。
                AgentEvents::on_pre_step(&ctx, |payload: StepReport| {
                    Box::pin(async move {
                        T7_SEEN
                            .lock()
                            .unwrap()
                            .push(format!("pre-step#{}", payload.round));
                        Ok(())
                    })
                });

                // serial：两个监听器都跑、都看到载荷，`Ok(None)` 不短路。
                AgentEvents::on_observe(&ctx, |payload: Invoke| {
                    Box::pin(async move {
                        T7_SEEN
                            .lock()
                            .unwrap()
                            .push(format!("observe:{}", payload.tool));
                        Ok(None)
                    })
                });
                AgentEvents::on_observe(&ctx, |payload: Invoke| {
                    Box::pin(async move {
                        T7_SEEN
                            .lock()
                            .unwrap()
                            .push(format!("observe2:{}", payload.tool));
                        Ok(None)
                    })
                });

                // bail：第一个给出答案的赢，后面的不再跑。
                AgentEvents::on_decide(&ctx, |payload: Decision| {
                    Box::pin(async move {
                        T7_SEEN
                            .lock()
                            .unwrap()
                            .push(format!("decide:{}", payload.reason));
                        Ok(None) // 不表态 ⇒ 继续往后走
                    })
                });
                AgentEvents::on_decide(&ctx, |payload: Decision| {
                    Box::pin(async move {
                        if payload.allow {
                            Ok(Some(payload))
                        } else {
                            Ok(None)
                        }
                    })
                });
                AgentEvents::on_decide(&ctx, |_payload: Decision| {
                    Box::pin(async move {
                        T7_SEEN
                            .lock()
                            .unwrap()
                            .push("decide:不该跑到这里".to_string());
                        Ok(None)
                    })
                });
                Ok(())
            })
        });
    let fiber = app.plugin(listeners, Config::Null).await.unwrap();
    let ctx = app.root.clone();

    // emit：与 `Ctx::emit` 同形——派发、丢弃返回值、不 await 返值。
    AgentEvents::pre_step(&ctx, StepReport { round: 3 }).await;

    // serial：两个监听器都跑。
    let observed = AgentEvents::observe(
        &ctx,
        Invoke {
            tool: "search".to_string(),
            prompt: "p".to_string(),
        },
    )
    .await;
    assert!(observed.is_none(), "serial 的返回值同样被丢弃");

    // bail：第二个监听器给出答案 ⇒ 第三个不再跑。
    let decided = AgentEvents::decide(
        &ctx,
        Decision {
            allow: true,
            reason: "allow it".to_string(),
        },
    )
    .await;
    let decision = decode_ret::<Decision>(decided).unwrap().unwrap();
    assert_eq!(decision.reason, "allow it");

    let seen = T7_SEEN.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            "pre-step#3".to_string(),
            "observe:search".to_string(),
            "observe2:search".to_string(),
            "decide:allow it".to_string(),
        ],
        "四种模式的语义必须各就各位"
    );

    // 卸载即摘除：typed 监听器与裸监听器走的是同一张 hook 表。
    fiber.dispose().await;
    assert_eq!(ctx.hook_count("agent/pre-step"), 0);
    assert_eq!(ctx.hook_count("tool/observe"), 0);
    assert_eq!(ctx.hook_count("tool/decide"), 0);
}

/// M6-附加（A9 的机制）：typed 监听器与"动态库式"监听器看到**同一份 JSON 载荷**，
/// 改写后的载荷沿链下传，链尾的决定原路回流。
#[tokio::test]
async fn typed_waterfall_payload_is_shared_with_json_listeners() {
    let app = App::new();
    let interceptor =
        cordis::core::plugin_fn("typed-interceptor", Inject::default(), |ctx: Ctx, _cfg| {
            Box::pin(async move {
                // ① typed 拦截器：改写载荷再问下游（`await next(args)` 的 typed 形态）。
                AgentEvents::on_before_invoke(&ctx, |payload: Invoke, next| {
                    Box::pin(async move {
                        if payload.prompt.contains("blocked") {
                            return Ok(None); // 不调 next 即短路
                        }
                        let rewritten = Invoke {
                            tool: payload.tool,
                            prompt: format!("{} +guard", payload.prompt),
                        };
                        next.call(rewritten).await
                    })
                });

                // ② 动态库式监听器：只认 `ExternValue(json)`，与报文层写的代码一模一样。
                ctx.on("tools/before-invoke", |call| {
                    Box::pin(async move {
                        let seen = call
                            .args
                            .first()
                            .and_then(|arg| arg.downcast::<ExternValue>())?;
                        T8_RAW.lock().unwrap().push(seen.0.to_string());
                        let next_args = call.args.clone();
                        call.next.call(next_args).await
                    })
                });

                // ③ 链尾的 typed 监听器：把收到的载荷记下来，并给出决定（不再往下调）。
                AgentEvents::on_before_invoke(&ctx, |payload: Invoke, _next| {
                    Box::pin(async move {
                        T8_TYPED
                            .lock()
                            .unwrap()
                            .push(format!("{}|{}", payload.tool, payload.prompt));
                        Ok(Some(Invoke {
                            tool: payload.tool,
                            prompt: format!("{} [seen]", payload.prompt),
                        }))
                    })
                });
                Ok(())
            })
        });
    app.plugin(interceptor, Config::Null).await.unwrap();
    let ctx = app.root.clone();

    let ret = AgentEvents::before_invoke(
        &ctx,
        Invoke {
            tool: "web".to_string(),
            prompt: "p".to_string(),
        },
        None,
    )
    .await;
    let out = decode_ret::<Invoke>(ret).unwrap().unwrap();
    assert_eq!(out.prompt, "p +guard [seen]", "链尾的决定必须原路回流");

    assert_eq!(
        T8_TYPED.lock().unwrap().clone(),
        vec!["web|p +guard".to_string()],
        "typed 拦截器的改写必须先到下游"
    );
    let raw = T8_RAW.lock().unwrap().clone();
    assert_eq!(raw.len(), 1, "报文层监听器必须也被调用");
    assert_eq!(
        raw[0],
        json!({ "tool": "web", "prompt": "p +guard" }).to_string(),
        "两侧看到的必须是同一份 JSON 载荷"
    );

    // 短路：不调 next ⇒ 下游（含链尾）全都不跑。
    T8_TYPED.lock().unwrap().clear();
    T8_RAW.lock().unwrap().clear();
    let ret = AgentEvents::before_invoke(
        &ctx,
        Invoke {
            tool: "web".to_string(),
            prompt: "blocked".to_string(),
        },
        None,
    )
    .await;
    assert!(ret.is_none(), "不调 next 即短路");
    assert!(T8_TYPED.lock().unwrap().is_empty());
    assert!(T8_RAW.lock().unwrap().is_empty());

    // 原生尾：链尾仍可传一个原生 `Ret`（`ExternValue` 形态），首个监听器短路时它不跑。
    let tail: Option<cordis::core::BoxFuture<Ret>> = Some(Box::pin(async {
        Some(Dyn::new(ExternValue(
            json!({ "tool": "web", "prompt": "tail" }),
        )))
    }));
    let ret = AgentEvents::before_invoke(
        &ctx,
        Invoke {
            tool: "web".to_string(),
            prompt: "blocked".to_string(),
        },
        tail,
    )
    .await;
    assert!(ret.is_none(), "首个监听器短路 ⇒ 尾也不会跑");
}
