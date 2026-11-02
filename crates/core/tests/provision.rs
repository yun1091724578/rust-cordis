//! M5：声明式 provision（Def 48 / Def 76）与"**声明为空 = 不透明**"这条规则。
//!
//! 论文 §6.5 是 *"a runtime **can** report it"*（能力非义务）：这里提供的是
//! **诊断面**——`Store::provision_report()` 按 fiber 报出「声明了什么 / 装了哪些 /
//! 有没有对不上」，而不是拒绝装载。
//!
//! 两条硬规则（方案 §4 M5）：
//! 1. **声明为空 = 不透明 = 跳过检查**：既有的原生插件都不声明 provision，
//!    把它们读成"声明了空集"会让 Def 48（装了未声明的 key）对每一个插件报警；
//! 2. key 命名空间与 [`Store::impls_owned_by`] **同一口径**（今天都是裸名；
//!    M6 启用 `<name>@<major>` 时两边一起换，不能只换一边）。

use cordis_core::{with_provision, App, Config, Ctx, Dyn, Inject};
use serde_json::json;
use std::sync::{Arc, Mutex};

static ISSUES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 提供者插件：按 `keys` 提供同名服务。
fn provider(name: &'static str, keys: &[&'static str]) -> Arc<dyn cordis_core::Plugin> {
    let keys: Vec<String> = keys.iter().map(|key| (*key).to_string()).collect();
    cordis_core::plugin_fn(name, Inject::default(), move |ctx: Ctx, _cfg| {
        let keys = keys.clone();
        Box::pin(async move {
            for key in keys {
                ctx.provide(&key, Dyn::new(json!({ "key": key })))?;
            }
            Ok(())
        })
    })
}

/// M5-1：**按 fiber** 报出声明与安装两边；两边都对得上时没有任何问题。
/// 同一份报告里还要能看见"没声明过"的插件——它就是"不透明"，一条检查都不做。
#[tokio::test]
async fn declared_provision_is_reported_per_fiber() {
    let app = App::new();
    let declared = app
        .plugin(
            with_provision(
                provider("declares", &["llm", "tools"]),
                vec!["tools".into(), "llm".into()],
            ),
            Config::Null,
        )
        .await
        .unwrap();
    // 不透明插件：一个 key 都不声明，装了两个——**不该**被报（规则 1）。
    app.plugin(provider("opaque", &["loose.a", "loose.b"]), Config::Null)
        .await
        .unwrap();

    let reports = app.store.provision_report();
    assert_eq!(reports.len(), 2, "每个存活 fiber 一条：{reports:?}");

    let mine = reports
        .iter()
        .find(|r| r.plugin == "declares")
        .expect("声明过的那条");
    assert_eq!(mine.uid, declared.uid());
    assert_eq!(
        mine.declared,
        vec!["tools".to_string(), "llm".to_string()],
        "声明按原文序"
    );
    assert_eq!(
        mine.installed,
        vec!["llm".to_string(), "tools".to_string()],
        "安装面按名字排序（`impls_owned_by` 来自哈希表，必须定序）"
    );
    assert!(
        mine.issues.is_empty(),
        "对得上就不该有问题：{:?}",
        mine.issues
    );

    let opaque = reports
        .iter()
        .find(|r| r.plugin == "opaque")
        .expect("不透明的那条");
    assert!(opaque.declared.is_empty(), "没声明 = 空");
    assert_eq!(
        opaque.installed,
        vec!["loose.a".to_string(), "loose.b".to_string()]
    );
    assert!(
        opaque.issues.is_empty(),
        "**声明为空 = 不透明 = 跳过检查**（否则 Def 48 会对每个既有插件报警）：{:?}",
        opaque.issues
    );
}

/// M5-2（Def 48）：装了**声明之外**的 key ⇒ 报出来（可读、带 key 名）。
#[tokio::test]
async fn installing_a_key_outside_the_declaration_is_reported() {
    let app = App::new();
    let fiber = app
        .plugin(
            with_provision(
                provider("overreach", &["tools", "secret"]),
                vec!["tools".into()],
            ),
            Config::Null,
        )
        .await
        .unwrap();

    let report = app
        .store
        .provision_report()
        .into_iter()
        .find(|r| r.uid == fiber.uid())
        .expect("一条");
    assert_eq!(report.declared, vec!["tools".to_string()]);
    assert_eq!(
        report.installed,
        vec!["secret".to_string(), "tools".to_string()]
    );
    let issue = report.issues.first().cloned().unwrap_or_default();
    assert!(issue.contains("secret"), "问题必须点名那个 key：{issue}");
    assert!(issue.contains("声明"), "也要说清是「不在声明里」：{issue}");
    ISSUES.lock().unwrap().push(issue);
}

/// M5-3（Def 76）：激活**结束**了却还没安装声明里的 key ⇒ 报出来。
///
/// 只对已经收敛到 ACTIVE 的 fiber 检查：依赖没齐的 fiber 还在 PENDING，
/// "还没装"不是问题（它压根没跑过 `apply`）。
#[tokio::test]
async fn finishing_activation_without_installing_a_declared_key_is_reported() {
    let app = App::new();
    let fiber = app
        .plugin(
            with_provision(
                provider("short", &["tools"]),
                vec!["tools".into(), "llm".into()],
            ),
            Config::Null,
        )
        .await
        .unwrap();
    assert_eq!(fiber.state(), cordis_core::FiberState::Active);

    let report = app
        .store
        .provision_report()
        .into_iter()
        .find(|r| r.uid == fiber.uid())
        .expect("一条");
    let issue = report.issues.first().cloned().unwrap_or_default();
    assert!(
        issue.contains("llm"),
        "问题必须点名没装上的那个 key：{issue}"
    );
    assert!(issue.contains("声明"), "说清是「声明了没装」：{issue}");

    // 对照面：还在 PENDING 的 fiber 不因"没装上"被报（它还没跑 apply）。
    let gated = with_provision(
        cordis_core::plugin_fn("gated", Inject::of(&["lazy.svc"]), |_ctx: Ctx, _cfg| {
            Box::pin(async { Ok(()) })
        }),
        vec!["never".into()],
    );
    let pending = app.plugin(gated, Config::Null).await.unwrap();
    pending.settle().await;
    assert_eq!(pending.state(), cordis_core::FiberState::Pending);
    let report = app
        .store
        .provision_report()
        .into_iter()
        .find(|r| r.uid == pending.uid())
        .expect("一条");
    assert_eq!(report.declared, vec!["never".to_string()]);
    assert!(
        report.issues.is_empty(),
        "依赖没齐的行不该被报 Def 76：{:?}",
        report.issues
    );
}
