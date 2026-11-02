//! M4：scope / Filter（G4/G18）——**注册期 scope + 父键关系**，新原语。
//!
//! 与既有两轴（`Store::realm_filter` 服务域 / `Store::fiber_filter` 生命周期子树）
//! **正交**：那两条只管"派发期怎么筛既有的钩子"，这里管的是"注册**在哪里**"——
//! 作用域 ctx 上的注册带着 scope 身份，派发时由**调用点自己**的谓词决定谁参与
//! （逐条对齐上游 `events.ts:76-85` 的 `thisArg[Context.filter].call(thisArg, hook.ctx)`）：
//!
//! - `hook.global` 永远参与；
//! - 派发点**没有** scope ⇒ 这一轴不参与（既有行为逐字不变）；
//! - 有 scope ⇒ 用它的谓词逐个裁决；`Ctx::filter_subtree` 是"只放行自己这一支"
//!   的那个常用组合（多 agent / 会话 / 子代理树的标准写法）。
//!
//! 身份与生命周期：scope 由 `ScopeAnchor` 标识，锚点只持 `Weak<Fiber>`——
//! 作用域比 fiber 活得久，而锚点不把 fiber 吊住（防环）。

use cordis_core::{
    App, Call, Config, Ctx, Dyn, ExternValue, FiberState, Inject, Mode, Ret, ScopeAnchor,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// 记一笔"这个钩子跑过"，并带回一个可断言的值。
fn hook(
    tag: &'static str,
    log: &'static Mutex<Vec<String>>,
) -> impl Fn(Call) -> cordis_core::BoxFuture<Ret> + Send + Sync + 'static {
    move |_call: Call| {
        log.lock().unwrap().push(tag.to_string());
        let value = json!({ "handled": tag });
        Box::pin(async move { Some(Dyn::new(ExternValue(value))) })
    }
}

// ------------------------------------------------------------------ 测试 1

static T1_HITS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T1_SITE: Mutex<Option<Ctx>> = Mutex::new(None);

/// M4-1（G18）：`filter` 派生的子 context **带调用者的 fiber**——注册事件因此是合法操作，
/// 而且两条注册各自的 scope 都能被诊断面看到；派发点带 scope 时按**它自己的谓词**筛选。
#[tokio::test]
async fn filtered_context_can_register_events_and_carries_the_fiber() {
    let app = App::new();
    let owner = cordis_core::plugin_fn("m4-owner", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            // G18：子 context 必须带 fiber（否则 `Store::on` 里的 `ctx.fiber()` 直接 panic）。
            let mine = ctx.filter_subtree();
            let stranger = ctx.filter_subtree();
            let deny_all = ctx.filter(Arc::new(|_| false));
            assert!(
                Arc::ptr_eq(&mine.fiber(), &ctx.fiber()),
                "作用域 ctx 必须继承调用者的 fiber"
            );
            assert!(stranger.fiber_opt().is_some(), "`fiber_opt` 同样要看得见");

            // 三种注册：无 scope（对谁都参与）、自己这一支、另一支（以及一个全拒的作用域）。
            ctx.on("m4/ping", hook("plain", &T1_HITS));
            mine.extend().on("m4/ping", hook("mine", &T1_HITS));
            stranger.extend().on("m4/ping", hook("stranger", &T1_HITS));
            deny_all.extend().on("m4/ping", hook("denied", &T1_HITS));
            *T1_SITE.lock().unwrap() = Some(mine);
            Ok(())
        })
    });
    let fiber = app.plugin(owner, Config::Null).await.unwrap();
    assert_eq!(fiber.state(), FiberState::Active);

    // 三条作用域注册各带一个 scope；诊断面按事件名查得到，且互不相同。
    let scopes = app.store.hook_scopes("m4/ping");
    assert_eq!(scopes.len(), 3, "三条作用域注册都该在诊断面里：{scopes:?}");
    assert_ne!(scopes[0], scopes[1]);

    // ① 派发点没有 scope ⇒ 这一轴不参与：四条都跑（既有行为逐字不变）。
    app.root.emit("m4/ping", vec![]).await;
    assert_eq!(
        *T1_HITS.lock().unwrap(),
        ["plain", "mine", "stranger", "denied"]
    );

    // ② 派发点 = `mine` 的作用域：只放行自己这一支（+ 无 scope 的注册）。
    T1_HITS.lock().unwrap().clear();
    let site = T1_SITE
        .lock()
        .unwrap()
        .clone()
        .expect("作用域 ctx 已交出来");
    site.emit("m4/ping", vec![]).await;
    assert_eq!(
        *T1_HITS.lock().unwrap(),
        ["plain", "mine"],
        "别支与全拒作用域都不得参与"
    );
}

// ------------------------------------------------------------------ 测试 2

static T2_HITS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 原生服务的惯用法（本仓"没有 Proxy"的设计）：方法首参就是**调用点的 ctx**。
///
/// 于是方法体能把调用者的上下文（连同它的 scope）用在**派发点**上——这正是
/// `service_method_can_observe_the_callers_scope` 要证明的能力，也是 M11 里
/// "每 agent 一个 scope、同名工具互不可见"的机制。
struct Tools;

impl Tools {
    async fn invoke(&self, caller: &Ctx, name: &str) {
        let args = vec![Dyn::new(ExternValue(json!({ "name": name })))];
        let tail: cordis_core::BoxFuture<Ret> =
            Box::pin(async { Some(Dyn::new(ExternValue(json!({ "noop": true })))) });
        caller
            .waterfall("tools/before-invoke", args, Some(tail), None)
            .await;
    }
}

/// M4-2：服务方法**看得见调用者的 scope**——用调用者的 ctx 做派发点，
/// 于是只有"A 这一支"的拦截器参与，B 的拦截器对这次调用不可见（反之亦然）。
#[tokio::test]
async fn service_method_can_observe_the_callers_scope() {
    let app = App::new();
    let provider = cordis_core::plugin_fn("m4-tools", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("tools", Dyn::new(Tools))?;
            Ok(())
        })
    });
    app.plugin(provider, Config::Null).await.unwrap();

    // 两个 agent：各自的 scope 只放行**自己这一支**（`filter_subtree`）。
    let agent_a = app.root.filter_subtree();
    let agent_b = app.root.filter_subtree();
    agent_a
        .extend()
        .on("tools/before-invoke", hook("a", &T2_HITS));
    agent_b
        .extend()
        .on("tools/before-invoke", hook("b", &T2_HITS));

    let tools = app.root.require_as::<Tools>("tools").unwrap();

    // A 的会话调用：只有 a 的拦截器看得见。
    tools.invoke(&agent_a.extend(), "search").await;
    assert_eq!(
        *T2_HITS.lock().unwrap(),
        ["a"],
        "B 的拦截器对 A 的调用不可见"
    );
    T2_HITS.lock().unwrap().clear();

    // B 的会话调用：反过来。
    tools.invoke(&agent_b.extend(), "search").await;
    assert_eq!(
        *T2_HITS.lock().unwrap(),
        ["b"],
        "A 的拦截器对 B 的调用不可见"
    );
}

// ------------------------------------------------------------------ 测试 3

/// M4-3：父键关系**可查询、可重绑**，并且沿链传递（子代理树的形状）。
#[tokio::test]
async fn scope_parent_chain_is_queryable_and_rebindable() {
    let app = App::new();
    let parent = app.root.filter_subtree();
    let child = app.root.filter_subtree();
    let grand = app.root.filter_subtree();
    let other = app.root.filter_subtree();

    assert!(child.in_scope_of(&child), "自己永远在自己里面");
    assert!(!child.in_scope_of(&parent), "还没绑：互不可见");

    child.bind_scope_parent(&parent).unwrap();
    assert!(child.in_scope_of(&parent), "绑定之后子键关系成立");

    grand.bind_scope_parent(&child).unwrap();
    assert!(grand.in_scope_of(&parent), "祖先关系沿链传递");
    assert!(
        !parent.in_scope_of(&grand),
        "反向不成立（父不继承子的可见性）"
    );

    // 可重绑：换一个父之后，旧父不再在链上。
    child.bind_scope_parent(&other).unwrap();
    assert!(child.in_scope_of(&other));
    assert!(!child.in_scope_of(&parent), "重绑之后旧父不在链上");
    assert!(!grand.in_scope_of(&parent), "孙的链跟着子一起改");
    assert!(grand.in_scope_of(&other), "孙仍沿链到达新父");
}

// ------------------------------------------------------------------ 测试 4

/// M4-4：成环必须被**拒**（可读错误），而且拒绝时关系原封不动。
#[tokio::test]
async fn scope_parent_cycle_is_rejected() {
    let app = App::new();
    let a = app.root.filter_subtree();
    let b = app.root.filter_subtree();

    a.bind_scope_parent(&b).unwrap();
    let error = b
        .bind_scope_parent(&a)
        .expect_err("把父绑到自己的后代上 = 成环，必须拒");
    assert!(error.to_string().contains("环"), "错误要说清是环：{error}");
    assert!(a.bind_scope_parent(&a).is_err(), "绑自己也是环");

    // 被拒之后关系没有被改动。
    assert!(a.in_scope_of(&b));
    assert!(!b.in_scope_of(&a));
}

// ------------------------------------------------------------------ 测试 5

static T5_HITS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// M4-5（**守卫**）：既有的两条派发轴逐字不变——派发点没有 scope 时，
/// scope 轴不参与，连"注册在带 scope 的 ctx 上"的钩子也照旧由那两条轴裁决。
#[tokio::test]
async fn realm_and_fiber_dispatch_filters_are_unaffected() {
    let app = App::new();
    // 同一个 fiber 上两条钩子：一条注册在普通 ctx，一条注册在带 scope 的 ctx。
    let owner = cordis_core::plugin_fn("m4-axis", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.on("m4/axis", hook("plain", &T5_HITS));
            ctx.filter_subtree().on("m4/axis", hook("scoped", &T5_HITS));
            Ok(())
        })
    });
    let fiber = app.plugin(owner, Config::Null).await.unwrap();
    assert_eq!(app.store.hook_count("m4/axis"), 2);

    // ① 别的 fiber 的子树过滤：两条都不在 ⇒ 一条也不跑。
    let stranger = cordis_core::plugin_fn("m4-stranger", Inject::default(), |_ctx: Ctx, _cfg| {
        Box::pin(async { Ok(()) })
    });
    let stranger_fiber = app.plugin(stranger, Config::Null).await.unwrap();
    let filter = app.store.fiber_filter(&stranger_fiber);
    let ret = app
        .store
        .dispatch(
            Mode::Parallel,
            "m4/axis",
            &app.root,
            vec![],
            None,
            filter.as_ref(),
        )
        .await;
    assert!(ret.is_none(), "两轴把两条钩子都滤掉了");
    assert!(T5_HITS.lock().unwrap().is_empty());

    // ② 自己的 fiber 子树过滤：两条都在（scope 轴没有插嘴）。
    let filter = app.store.fiber_filter(&fiber);
    let ret = app
        .store
        .dispatch(
            Mode::Parallel,
            "m4/axis",
            &app.root,
            vec![],
            None,
            filter.as_ref(),
        )
        .await;
    assert_eq!(
        *T5_HITS.lock().unwrap(),
        ["plain", "scoped"],
        "带 scope 的那条也照旧被两轴放行"
    );
    assert!(ret.is_none(), "Parallel 不带回单值");
}

// ------------------------------------------------------------------ 测试 6

static T6_HITS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T6_SLOT: Mutex<Option<(Ctx, Arc<ScopeAnchor>)>> = Mutex::new(None);

/// M4-6：作用域**比 fiber 活得久**，而锚点只持 `Weak<Fiber>`（不把 fiber 吊住）。
#[tokio::test]
async fn scope_survives_the_anchor_fiber_going_away() {
    let app = App::new();
    let owner = cordis_core::plugin_fn("m4-scoped", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            // "锚点只持 Weak"的第一手证据：派生作用域**不得**增加 fiber 的强引用计数。
            let fiber = ctx.fiber();
            let before = Arc::strong_count(&fiber);
            let scoped = ctx.filter_subtree();
            let _plain = ctx.filter(Arc::new(|_| true));
            assert_eq!(
                Arc::strong_count(&fiber),
                before,
                "作用域锚点只持 Weak：派生不得给 fiber 加计数"
            );
            let anchor = scoped.scope().expect("作用域 ctx 必须带锚点");
            scoped.on("m4/owned", hook("owned", &T6_HITS));
            *T6_SLOT.lock().unwrap() = Some((scoped, anchor));
            Ok(())
        })
    });
    let fiber = app.plugin(owner, Config::Null).await.unwrap();
    let (scoped, anchor) = T6_SLOT.lock().unwrap().take().expect("作用域已交出来");
    let uid = anchor.uid();
    assert_eq!(
        app.store.hook_scopes("m4/owned"),
        vec![uid],
        "钩子带着这个 scope"
    );
    assert!(anchor.fiber().is_some(), "锚点能（弱）升级到创建它的 fiber");

    // 作用域本身照旧可用：它仍参与父键关系（锚点级别的比较与查询）。
    let fresh = app.root.filter_subtree();
    let fresh_anchor = fresh.scope().expect("新作用域也有锚点");
    let child = app.root.filter_subtree();
    child.bind_scope_parent(&fresh).unwrap();
    let child_anchor = child.scope().unwrap();
    assert!(fresh_anchor.is_ancestor_or_self_of(&child_anchor));
    assert!(
        !fresh_anchor.is_ancestor_or_self_of(&anchor),
        "另一个锚点仍在外面"
    );
    assert!(anchor.is_ancestor_or_self_of(&anchor));

    // 卸载：钩子随 fiber 摘除、装载表里也不再有它——而**作用域身份**还活着。
    fiber.dispose().await;
    assert!(
        app.store.hook_scopes("m4/owned").is_empty(),
        "钩子随 fiber 一起摘除"
    );
    assert!(
        app.store.fibers_by_plugin_name("m4-scoped").is_empty(),
        "装载表里不再有这条 fiber"
    );

    // 把测试自己的强引用也放掉：作用域不依赖任何 ctx/fiber 的存活。
    // （fiber 对象随最后一个强引用释放；父 fiber 的 effect 只持 Weak，
    //  不参与保活。证据是上面那两次**计数不增** + 这里的身份与查询照旧。）
    drop(scoped);
    drop(fiber);
    drop(app);
    assert_eq!(anchor.uid(), uid, "身份稳定（单调 uid，永不复用）");
    assert!(
        anchor.is_ancestor_or_self_of(&anchor),
        "作用域仍在，可比较可查询"
    );
}

/// `filter_subtree` 的谓词由锚点持有；槽里若强持锚点就会形成引用环。
#[test]
fn filter_subtree_anchor_is_not_kept_alive_by_its_predicate() {
    let app = App::new();
    for round in 0..4 {
        let scoped = app.root.filter_subtree();
        let anchor = scoped.scope().expect("作用域 ctx 必须带锚点");
        let weak = Arc::downgrade(&anchor);
        drop(anchor);
        drop(scoped);
        assert!(
            weak.upgrade().is_none(),
            "第 {round} 次派生：丢弃作用域 ctx 后锚点仍被谓词环吊住"
        );
    }

    // 绑定的祖先也不能被后代谓词环间接吊住。
    let parent = app.root.filter_subtree();
    let child = app.root.filter_subtree();
    child.bind_scope_parent(&parent).unwrap();
    let parent_anchor = Arc::downgrade(&parent.scope().unwrap());
    let child_anchor = Arc::downgrade(&child.scope().unwrap());
    drop(child);
    drop(parent);
    assert!(
        parent_anchor.upgrade().is_none(),
        "祖先锚点被后代引用环保留"
    );
    assert!(
        child_anchor.upgrade().is_none(),
        "后代锚点被自己的谓词环保留"
    );
}

/// 并发的 A→B 与 B→A 只能成功一个。检查与写父指针若不在同一临界区，
/// 两边都会通过祖先检查并互相成为父节点：查询失去终止性，Arc 也成环。
#[test]
fn concurrent_opposite_binds_cannot_form_a_scope_cycle() {
    for round in 0..100 {
        let app = App::new();
        let a = app.root.filter_subtree().scope().expect("a 锚点");
        let b = app.root.filter_subtree().scope().expect("b 锚点");
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let (a1, b1, barrier1) = (a.clone(), b.clone(), barrier.clone());
        let t1 = std::thread::spawn(move || {
            barrier1.wait();
            a1.bind_parent(&b1)
        });
        let (a2, b2, barrier2) = (a.clone(), b.clone(), barrier.clone());
        let t2 = std::thread::spawn(move || {
            barrier2.wait();
            b2.bind_parent(&a2)
        });

        let first = t1.join().expect("线程 1");
        let second = t2.join().expect("线程 2");
        let successes = usize::from(first.is_ok()) + usize::from(second.is_ok());
        assert_eq!(
            successes, 1,
            "第 {round} 轮：相反方向的绑定必须恰好成功一个（{first:?} / {second:?}）"
        );
        assert!(
            !a.is_ancestor_or_self_of(&b) || !b.is_ancestor_or_self_of(&a),
            "第 {round} 轮：作用域父链成环"
        );
    }
}
