//! M11 红测试（**无 DLL 层**，D9）：spike 的领域层只靠文档化的内核/SDK 面就能表达。
//!
//! 五条断言各自钉一件事：
//! 1. **作用域隔离**（M4）：两个 agent 的同名工具互不可见；子代理 `bind_scope_parent`
//!    之后**继承**父这一支；成环被拒。
//! 2. **typed 服务**（M6）：`#[cordis::service]` 的三个面（typed / JSON / 原生句柄）
//!    打到同一份实现，键是 `<name>@<major>`。
//! 3. **typed 事件**（M6）：名字**逐字**；坏载荷不打给 typed 监听器，只记一条可读错误。
//! 4. **replay 适配器**（领域接缝）：读录制的 chunk 序列 ⇒ 同一份录制跑两次逐字段相同，
//!    `sessions` 的 `seq` 连续且 append-only。
//! 5. **原地换适配器**（M7/A7）：依赖它的 agent-loop epoch 一动不动，但新值可见。
//!
//! 真 DLL / HMR 的断言（A4/A9/A10/A11）在 `src/main.rs`——按 D9 只能进二进制。

use std::sync::{Arc, Mutex};

use cordis::core::{plugin_fn, App, Config, Ctx, Dyn, ExternValue, FiberState, Iface, Inject};
use cordis::typed::events::decode_ret;
use harness_spike::{
    agent_scope, boot, call_tool, mount_agent, mount_tool, owner_chain, replay_llm, scripted_llm,
    Agent, AgentService, HarnessEvents, Llm, LlmService, Plan, ReplayLlm, SessionsService, Step,
    ToolsService,
};
use serde_json::{json, Value};

fn plan(tool: &str, input: Value) -> Plan {
    Plan {
        tool: tool.to_string(),
        input,
    }
}

/// typed 访问器的薄包装（省得每处都写 `unwrap`）。
fn agent_of(ctx: &Ctx) -> Arc<dyn Agent> {
    AgentService::of(ctx).expect("agent@1 必须已提供")
}

/// M11-1（**M4**）：作用域隔离——同名工具各答各的，跨作用域**没有人应答**。
#[tokio::test]
async fn scope_isolation_hides_same_named_tools_between_agents() {
    let harness = boot(&["alpha", "beta"], scripted_llm("scripted", vec![]))
        .await
        .unwrap();
    assert_eq!(harness.agent("alpha").fiber.state(), FiberState::Active);

    // 两个 agent 各挂一个**同名**工具 `search`（回答里带 owner），再各挂一个专属工具。
    mount_tool(&harness.agent_ctx("alpha"), "alpha", "search").await;
    mount_tool(&harness.agent_ctx("alpha"), "alpha", "notes").await;
    mount_tool(&harness.agent_ctx("beta"), "beta", "search").await;
    mount_tool(&harness.agent_ctx("beta"), "beta", "translate").await;

    let alpha = harness.agent_ctx("alpha");
    let beta = harness.agent_ctx("beta");

    // 同名工具：各答各的（作用域轴在**派发期**逐个裁决注册）。
    assert_eq!(
        call_tool(&alpha, "search").await.unwrap().output["owner"],
        json!("alpha")
    );
    assert_eq!(
        call_tool(&beta, "search").await.unwrap().output["owner"],
        json!("beta")
    );

    // 专属工具跨不过作用域：**没有人应答**（不是"跑成了别人的"）。
    assert!(
        call_tool(&beta, "notes").await.is_none(),
        "beta 不许看见 alpha 的 notes"
    );
    assert!(
        call_tool(&alpha, "translate").await.is_none(),
        "alpha 不许看见 beta 的 translate"
    );

    // 目录面同样按作用域链过滤：一样的名字，两张不同的表。
    assert_eq!(
        agent_of(&alpha).tools().await.unwrap(),
        vec!["notes", "search"]
    );
    assert_eq!(
        agent_of(&beta).tools().await.unwrap(),
        vec!["search", "translate"]
    );

    // 子代理：`bind_scope_parent` 之后**继承**父这一支（不是拷贝一份）。
    let sub = agent_scope(&harness.app.root);
    sub.bind_scope_parent(&alpha).unwrap();
    assert_eq!(
        call_tool(&sub, "search").await.unwrap().output["owner"],
        json!("alpha")
    );
    assert!(
        call_tool(&sub, "translate").await.is_none(),
        "子代理只继承 alpha 这一支"
    );

    let sub_agent = mount_agent(&sub, "sub").await.unwrap();
    assert_eq!(
        agent_of(&sub_agent.ctx).tools().await.unwrap(),
        vec!["notes", "search"]
    );

    // 成环被拒：作用域链参与可见性裁决，环 = "人人可见我"。
    let err = alpha.bind_scope_parent(&sub).unwrap_err();
    assert!(err.contains("成环"), "{err}");
}

/// M11-2（**M6**）：三个服务都是 `#[cordis::service]` 产出的；typed 面与 JSON 面
/// （参数信封 `[a1…aN]`）打到**同一份实现**。
#[tokio::test]
async fn typed_services_are_callable_in_process_and_over_json() {
    let harness = boot(
        &["alpha"],
        scripted_llm("scripted", vec![plan("search", json!({ "q": "x" }))]),
    )
    .await
    .unwrap();
    mount_tool(&harness.agent_ctx("alpha"), "alpha", "search").await;

    // 键 = `<name>@<major>`（§6.6 路线 1）
    assert_eq!(ToolsService::KEY, "tools@1");
    assert_eq!(LlmService::KEY, "llm@1");
    assert_eq!(SessionsService::KEY, "sessions@1");
    assert_eq!(AgentService::KEY, "agent@1");

    let ctx = harness.agent_ctx("alpha");
    // typed 面（in-process）
    assert_eq!(agent_of(&ctx).adapter().await.unwrap(), "scripted");
    assert_eq!(agent_of(&ctx).tools().await.unwrap(), vec!["search"]);
    // JSON 面：同一个键、同一个实现；`owners` 就是调用方的作用域链
    let owners = owner_chain(&ctx);
    let names = ctx
        .call_iface(ToolsService::KEY, 1, "names", json!([owners.clone()]))
        .await
        .unwrap();
    assert_eq!(names, json!(["search"]));
    // 换个作用域链（只有全局）⇒ 同一个实现给出**不同的**答案（可见性跟着链路走）。
    let names = ctx
        .call_iface(ToolsService::KEY, 1, "names", json!([[0]]))
        .await
        .unwrap();
    assert_eq!(names, json!([]), "search 属于 alpha 的作用域，不在全局链上");

    // typed 面登记 ⇒ JSON 面读回（信封就是 `[a1…aN]`）
    let tools = ToolsService::of(&ctx).unwrap();
    tools
        .register(0, "ping".into(), "探活".into(), None)
        .await
        .unwrap();
    let names = ctx
        .call_iface(ToolsService::KEY, 1, "names", json!([owners.clone()]))
        .await
        .unwrap();
    assert_eq!(names, json!(["ping", "search"]));

    // 三种失败都必须**可读**：重名、未知方法、iface 还没有提供者。
    let dup = tools
        .register(0, "ping".into(), "再来一次".into(), None)
        .await
        .unwrap_err();
    assert!(dup.contains("ping"), "{dup}");
    let err = ctx
        .call_iface(ToolsService::KEY, 1, "nope", json!([]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("nope"), "{err}");
    let err = tools
        .register(
            0,
            "ghost".into(),
            "幽灵".into(),
            Some("tool.ghost@1".into()),
        )
        .await
        .unwrap_err();
    assert!(err.contains("tool.ghost@1"), "{err}");
    // 版本协商：拿 v2 请求打到 v1 的接口上必须被明确拒绝。
    let err = ctx
        .call_iface(ToolsService::KEY, 2, "names", json!([[0]]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("版本"), "{err}");
}

/// M11-3（**M6**）：事件名逐字；typed 监听器只接受**能解出来**的载荷，解不出来就
/// 记一条可读错误并放弃这次表态（而不是 panic 或静默）。
#[tokio::test]
async fn typed_event_payloads_are_checked_and_named_verbatim() {
    // 名字**逐字**：ABI 侧的拦截名是硬编码的，宏绝不加前缀。
    assert_eq!(HarnessEvents::PRE_STEP, "agent/pre-step");
    assert_eq!(HarnessEvents::BEFORE_INVOKE, "tools/before-invoke");
    assert_eq!(HarnessEvents::RUN_TOOL, "tools/invoke");

    let app = App::new();
    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let logs = logs.clone();
        app.store
            .set_log_sink(Arc::new(move |level, name, message| {
                logs.lock()
                    .unwrap()
                    .push(format!("{level:?}:{name}:{message}"));
            }));
    }
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = {
        let seen = seen.clone();
        plugin_fn(
            "typed-listener",
            Inject::default(),
            move |ctx: Ctx, _cfg: Config| {
                let seen = seen.clone();
                Box::pin(async move {
                    HarnessEvents::on_pre_step(&ctx, move |payload: Step| {
                        let seen = seen.clone();
                        Box::pin(async move {
                            seen.lock()
                                .unwrap()
                                .push(format!("{}/{}", payload.agent, payload.round));
                            Ok(())
                        })
                    });
                    Ok(())
                })
            },
        )
    };
    app.plugin(listener, Config::Null).await.unwrap();

    // ① 合法载荷 ⇒ 监听器看到。
    HarnessEvents::pre_step(
        &app.root,
        Step {
            agent: "A".into(),
            round: 1,
        },
    )
    .await;
    assert_eq!(seen.lock().unwrap().clone(), vec!["A/1".to_string()]);

    // ② 坏载荷（少了 `round`）⇒ 监听器**不跑**、派发照常完成、日志里点名事件与字段。
    app.root
        .emit(
            HarnessEvents::PRE_STEP,
            vec![Dyn::new(ExternValue(json!({ "agent": "A" })))],
        )
        .await;
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "解不出来的载荷不许打给 typed 监听器"
    );
    let text = logs.lock().unwrap().join("\n");
    assert!(
        text.contains("agent/pre-step"),
        "错误里必须点名事件：{text}"
    );
    assert!(text.contains("round"), "错误里必须点名缺了什么：{text}");

    // ③ `decode_ret` 对坏返回值同样是可读错误（不是静默的 `None`）。
    let err = decode_ret::<Step>(Some(Dyn::new(ExternValue(json!({ "nope": 1 })))))
        .unwrap_err()
        .to_string();
    assert!(err.contains("返回值"), "{err}");
}

/// M11-4（领域接缝）：replay 适配器读**录制的 chunk 序列**——同一份录制跑两次，
/// 报告逐字段相同，`sessions` 的 `seq` 连续、append-only。
#[tokio::test]
async fn replay_adapter_drives_the_loop_deterministically() {
    let harness = boot(
        &["alpha"],
        replay_llm(
            "replay",
            vec![
                json!({ "tool": "search", "input": { "q": "cordis" } }),
                json!({ "tool": "notes", "input": { "text": "记一笔" } }),
            ],
        ),
    )
    .await
    .unwrap();
    mount_tool(&harness.agent_ctx("alpha"), "alpha", "search").await;
    mount_tool(&harness.agent_ctx("alpha"), "alpha", "notes").await;
    let alpha = harness.agent_ctx("alpha");

    let first = agent_of(&alpha).run("跑一次".into()).await.unwrap();
    let second = agent_of(&alpha).run("跑一次".into()).await.unwrap();

    assert_eq!(first.adapter, "replay");
    assert_eq!(
        first.tools,
        vec!["search", "notes"],
        "chunk 的顺序就是计划的顺序"
    );
    assert_eq!(first.rounds, 2);
    assert_eq!(first.tools, second.tools, "同一份录制 ⇒ 同样的工具序列");
    assert_eq!(first.stages, second.stages);
    assert_eq!(first.seqs.len(), second.seqs.len());
    assert_eq!(
        first.seqs[0] + first.seqs.len() as u64,
        second.seqs[0],
        "第二次会话的 seq 接着第一次继续（append-only 的日志）"
    );

    // 日志本身：每条记录都带 seq，且 seq 连续 1..N。
    let records = harness.session_records();
    assert_eq!(records.len(), first.seqs.len() + second.seqs.len());
    let seqs: Vec<u64> = records
        .iter()
        .filter_map(|record| record["seq"].as_u64())
        .collect();
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<u64>>(),
        "seq 必须连续"
    );

    // 录制的 chunk 坏掉 ⇒ fail loud（不是悄悄少跑一步）。
    let broken = ReplayLlm::new("replay", vec![json!({ "tool": "search" })]);
    let err = broken.plan("p".into()).await.unwrap_err();
    assert!(err.contains("chunk"), "{err}");
}

/// M11-5（**M7 / A7**）：原地换 `llm` 适配器 ⇒ 依赖它的 agent-loop **epoch 不变**，
/// 但读到的是**当下**的值；两张面（typed / 原生句柄）指向同一个对象。
#[tokio::test]
async fn replacing_the_llm_adapter_keeps_the_consumer_epoch() {
    let harness = boot(
        &["alpha"],
        scripted_llm("scripted", vec![plan("search", json!({}))]),
    )
    .await
    .unwrap();
    let alpha = harness.agent_ctx("alpha");
    assert_eq!(agent_of(&alpha).adapter().await.unwrap(), "scripted");
    let epoch = harness
        .agent("alpha")
        .fiber
        .running_epoch()
        .expect("agent-loop 已激活 ⇒ 有 epoch");

    // 原地换适配器：一次同步写，槽位的身份（注册表）不变。
    harness.swap_llm(replay_llm("replay", vec![])).unwrap();
    harness.agent("alpha").fiber.settle().await;

    assert_eq!(
        harness.agent("alpha").fiber.running_epoch(),
        Some(epoch),
        "换适配器不得重挂消费方（A7）"
    );
    assert_eq!(
        agent_of(&alpha).adapter().await.unwrap(),
        "replay",
        "新值必须可见"
    );

    // 原生句柄读到的是**同一个** typed 值（两张面、一份真相）。
    let via_handle = harness.registry.get(LlmService::KEY).expect("槽位必须可读");
    assert!(
        via_handle.downcast::<Iface<dyn Llm>>().is_some(),
        "槽位里是 typed 服务值"
    );

    // 跑一次：用的必须是**新**适配器。
    let report = agent_of(&alpha).run("p".into()).await.unwrap();
    assert_eq!(report.adapter, "replay");
    assert!(report.tools.is_empty(), "replay 的录制是空的 ⇒ 一步都不跑");
    assert_eq!(report.rounds, 0);
}
