//! M6 红测试：**键的命名空间** `key = <name>@<major>`（§6.6 路线 1）。
//!
//! 为什么需要它（G8 的邻域、§6.6）：`tools` 这种短名在真实生态里一定会被多个
//! 不相干的组件抢用。把 major 版本并进键以后：
//! - 同名不同 major 的服务可以**共存**，各自只被声明了对应键的消费者看见；
//! - 同 key 二次注册仍然被内核不变量拒绝（一个键一个实现域内唯一）；
//! - **版本不匹配**不表现为"读到了旧实现"，而是"停在 Pending"（风险清单第 6 条）。

use std::sync::{Arc, Mutex};

use cordis::core::{App, Config, Ctx, FiberState, Inject};
use cordis::{service, service_impl};

#[service(name = "tools", version = 1)]
pub trait ToolsV1 {
    async fn invoke(&self, name: String) -> Result<String, String>;
}

#[service(name = "tools", version = 2)]
pub trait ToolsV2 {
    async fn invoke(&self, name: String) -> Result<String, String>;
}

#[derive(Default, Clone)]
struct ToolsV1Impl;

#[derive(Default, Clone)]
struct ToolsV2Impl;

#[service_impl]
impl ToolsV1 for ToolsV1Impl {
    async fn invoke(&self, name: String) -> Result<String, String> {
        Ok(format!("v1:{name}"))
    }
}

#[service_impl]
impl ToolsV2 for ToolsV2Impl {
    async fn invoke(&self, name: String) -> Result<String, String> {
        Ok(format!("v2:{name}"))
    }
}

static T8_SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[tokio::test]
async fn key_namespacing_prevents_collision_between_two_tools_at_v1() {
    assert_eq!(ToolsV1Service::KEY, "tools@1");
    assert_eq!(ToolsV2Service::KEY, "tools@2");

    let app = App::new();
    // 两个提供者各注册一个 major：键不同 ⇒ 都能装上（这正是命名空间要买的东西）。
    let v1 = cordis::core::plugin_fn("tools-v1", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ToolsV1Service::provide(&ctx, Arc::new(ToolsV1Impl))?;
            Ok(())
        })
    });
    let v2 = cordis::core::plugin_fn("tools-v2", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ToolsV2Service::provide(&ctx, Arc::new(ToolsV2Impl))?;
            Ok(())
        })
    });
    let v1_fiber = app.plugin(v1, Config::Null).await.unwrap();
    let v2_fiber = app.plugin(v2, Config::Null).await.unwrap();
    assert_eq!(v1_fiber.state(), FiberState::Active);
    assert_eq!(v2_fiber.state(), FiberState::Active);

    // 消费者声明 `tools@2`：拿到的必须是 v2 的实现；`tools@1` 它没声明 ⇒ 看不见。
    let consumer = cordis::core::plugin_fn(
        "tools-consumer",
        Inject::of(&["tools@2"]),
        |ctx: Ctx, _cfg| {
            Box::pin(async move {
                let v2 = ToolsV2Service::of(&ctx)?;
                let answer = v2
                    .invoke("search".into())
                    .await
                    .map_err(|err| -> cordis::core::Error { err.into() })?;
                let unseen = match ToolsV1Service::of(&ctx) {
                    Ok(_) => "Ok(读到了没声明的版本)".to_string(),
                    Err(err) => err.to_string(),
                };
                T8_SEEN.lock().unwrap().extend([answer, unseen]);
                Ok(())
            })
        },
    );
    app.plugin(consumer, Config::Null).await.unwrap();
    let seen = T8_SEEN.lock().unwrap().clone();
    assert_eq!(seen[0], "v2:search", "`tools@2` 的访问器必须命中 v2 的实现");
    assert!(
        seen[1].contains("without inject"),
        "没声明的键读不到：{seen:?}"
    );

    // 版本不匹配 = 停在 Pending（不是静默拿到别的版本）。
    let pending = cordis::core::plugin_fn(
        "tools-pending",
        Inject::of(&["tools@3"]),
        |ctx: Ctx, _cfg| {
            Box::pin(async move {
                let _ = ToolsV2Service::of(&ctx);
                Ok(())
            })
        },
    );
    let pending_fiber = app.plugin(pending, Config::Null).await.unwrap();
    assert_eq!(
        pending_fiber.state(),
        FiberState::Pending,
        "声明了一个没人提供的键 ⇒ 停在 Pending"
    );

    // 同 key 二次注册：内核不变量（一个键一个实现域内唯一）照旧成立。
    let duplicate =
        cordis::core::plugin_fn("tools-duplicate", Inject::default(), |ctx: Ctx, _cfg| {
            Box::pin(async move {
                match ToolsV2Service::provide(&ctx, Arc::new(ToolsV2Impl)) {
                    Ok(_) => Err("tools@2 被注册了两次".into()),
                    Err(_) => Ok(()),
                }
            })
        });
    app.plugin(duplicate, Config::Null).await.unwrap();
}
