use super::*;
use crate::mount;
use std::sync::atomic::AtomicUsize;

/// 造一个"假插件"：**完全不需要动态库**，用来把"换名重挂 + 回滚"这条链单独测干净。
///
/// - `value`：成功时 `provide` 的服务值；
/// - `fail_on_attempt`：第几次 `apply` 失败（`None` = 永不失败）；
/// - `trace`：记录注册 / 释放次序，用来断言"半成品被拆干净了"。
fn fake_plugin(
    name: &'static str,
    service: &'static str,
    value: u32,
    fail_on_attempt: Option<usize>,
    trace: Arc<Mutex<Vec<String>>>,
) -> Arc<dyn Plugin> {
    let attempts = Arc::new(AtomicUsize::new(0));
    Arc::new(FnPlugin {
        name,
        key: cordis_core::unique_key(name),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(move |ctx: Ctx, _config| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            let trace = trace.clone();
            Box::pin(async move {
                // 先注册一个副作用：无论后面成败，它都必须被正确处置。
                let registered = format!("{name}:registered");
                let released = format!("{name}:released");
                let a = trace.clone();
                let b = trace.clone();
                ctx.effect(move |_| {
                    a.lock().unwrap().push(registered);
                    Disposable::sync(move || b.lock().unwrap().push(released))
                });
                if Some(attempt) == fail_on_attempt {
                    return Err(format!("{name} 第 {attempt} 次 apply 故意失败").into());
                }
                ctx.provide(service, Dyn::new(value))?;
                Ok(())
            })
        }),
    })
}

fn mounted_record(label: &str, fiber: &Arc<Fiber>, plugin: Arc<dyn Plugin>) -> Mounted {
    let scope = fiber.parent_ctx().unwrap_or_else(|| fiber.ctx());
    Mounted {
        label: label.into(),
        origin: RowOrigin::Main,
        source: PathBuf::from("never-exists.dll"),
        staged: PathBuf::from("never-exists.r1.dll"),
        fingerprint: None,
        sha256: None,
        file_source: false,
        config: Config::Null,
        inject: None,
        fiber: Arc::downgrade(fiber),
        // 记录里的派生上下文：这里等价于"行被挂载时用的父 ctx"（测试挂在 root 上）。
        scope,
        declared: Entry {
            id: Some(label.into()),
            name: "entry".into(),
            ..Entry::default()
        },
        scope_inputs: Vec::new(),
        volatile: None,
        plugin,
    }
}

#[test]
fn commit_outcome_refreshes_sha256_from_the_current_staged_copy() {
    let app = cordis_core::App::new();
    let fiber = app.root_fiber();
    let plugin: Arc<dyn Plugin> = Arc::new(FnPlugin {
        name: "sha-refresh",
        key: cordis_core::unique_key("sha-refresh"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(|_ctx: Ctx, _config: Config| Box::pin(async { Ok(()) })),
    });
    let dir = std::env::temp_dir().join(format!("cordis-loader-sha-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("临时目录");
    let source = dir.join("source.dll");
    let old = dir.join("old.dll");
    let new = dir.join("new.dll");
    std::fs::write(&source, b"source-bytes").expect("source");
    std::fs::write(&old, b"old-bytes").expect("old");
    std::fs::write(&new, b"new-bytes").expect("new");

    let mut record = mounted_record("sha-refresh", &fiber, plugin.clone());
    record.source = source.clone();
    record.staged = old.clone();
    record.file_source = true;
    record.sha256 = Some("stale".into());

    let ok = record.commit_outcome(
        SwapOutcome::Ok {
            fiber: fiber.clone(),
            plugin: plugin.clone(),
            status: ReloadStatus::Active,
            pending: Vec::new(),
            dispose_report: None,
        },
        Some(new.clone()),
    );
    assert!(!ok);
    assert_eq!(
        record.sha256.as_deref(),
        Some(crate::source::sha256_file(&new).unwrap().as_str()),
        "HMR 换副本后必须重算 sha256"
    );

    record.staged = old.clone();
    record.sha256 = Some("stale-again".into());
    record.commit_outcome(
        SwapOutcome::RolledBack {
            error: "故意失败".into(),
            restored: fiber,
            pending: Vec::new(),
            dispose_report: None,
        },
        Some(new.clone()),
    );
    assert_eq!(record.staged, old, "回滚不得把新副本写进记录");
    assert_eq!(
        record.sha256.as_deref(),
        Some(crate::source::sha256_file(&old).unwrap().as_str()),
        "回滚后 sha256 必须对应仍在服务的旧副本"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn read_u32(ctx: &Ctx, name: &str) -> Option<u32> {
    ctx.get(name)
        .and_then(|value| value.downcast::<u32>())
        .map(|v| *v)
}

/// 新版本在 `apply` 阶段失败时，必须**回滚到旧实例**，而不是留下一个空位。
///
/// 论文 §5.2.2 Algorithm 10 是事务性的：*"系统绝不进入半重载状态"*。
/// 预检（`ping`）只挡得住"新库加载不起来"；配置错、依赖缺、插件自己返回 `Err`
/// 只有走到这里才发现——那时旧实例已经拆掉，不回滚就等于能力凭空消失。
#[tokio::test]
async fn a_failed_reload_rolls_back_to_the_previous_instance() {
    let app = cordis_core::App::new();
    let trace = Arc::new(Mutex::new(Vec::new()));

    let old = fake_plugin("old", "svc", 1, None, trace.clone());
    let fiber = mount(&app.root, old.clone(), Config::Null, None);
    fiber.settle().await;
    assert!(fiber.error().is_none(), "旧实例应当正常上线");
    assert_eq!(read_u32(&app.root, "svc"), Some(1));

    let record = mounted_record("entry", &fiber, old);
    // 新版本唯一一次 apply 就失败。
    let broken = fake_plugin("broken", "svc", 2, Some(0), trace.clone());

    let outcome = swap_entry(&record, broken).await;
    let message = match &outcome {
        SwapOutcome::Ok { .. } => panic!("新版本 apply 失败必须返回错误"),
        other => other.error_message().unwrap().to_string(),
    };
    assert!(
        matches!(outcome, SwapOutcome::RolledBack { .. }),
        "应回滚到旧实例，实际：{}",
        outcome.describe()
    );
    assert!(
        message.contains("故意失败"),
        "错误信息应当带上原委：{message}"
    );

    // ★ 回滚：旧实例必须重新上线，服务可用且值不变。
    assert_eq!(
        read_u32(&app.root, "svc"),
        Some(1),
        "失败后应当回滚到旧实例"
    );

    // ★ 半成品必须被拆干净：失败的新实例注册的副作用，其 disposer 跑过了。
    let seen = trace.lock().unwrap().clone();
    assert!(
        seen.contains(&"broken:released".to_string()),
        "失败的新实例的副作用应当被回滚，实际轨迹：{seen:?}"
    );
}

/// 成功路径：新实例上线、旧实例被拆、值确实换了。
#[tokio::test]
async fn a_successful_reload_swaps_in_the_new_instance() {
    let app = cordis_core::App::new();
    let trace = Arc::new(Mutex::new(Vec::new()));

    let old = fake_plugin("old", "svc", 1, None, trace.clone());
    let fiber = mount(&app.root, old.clone(), Config::Null, None);
    fiber.settle().await;
    let record = mounted_record("entry", &fiber, old);

    let fresh = fake_plugin("fresh", "svc", 2, None, trace.clone());
    let swapped = match swap_entry(&record, fresh).await {
        SwapOutcome::Ok { fiber, .. } => fiber,
        other => panic!("新版本应当换成功，实际：{}", other.describe()),
    };

    assert_eq!(swapped.state(), cordis_core::FiberState::Active);
    assert_eq!(read_u32(&app.root, "svc"), Some(2), "应当换成新实例的值");

    let seen = trace.lock().unwrap().clone();
    assert!(
        seen.contains(&"old:released".to_string()),
        "旧实例应当被拆掉：{seen:?}"
    );
}

/// 连回滚都失败时，错误信息必须**同时**说明两件事，而不是静默留个空位。
#[tokio::test]
async fn when_the_rollback_itself_fails_the_error_reports_both_failures() {
    let app = cordis_core::App::new();
    let trace = Arc::new(Mutex::new(Vec::new()));

    // 旧实例第一次成功，第二次（也就是回滚那一次）失败。
    let flaky = fake_plugin("flaky", "svc", 1, Some(1), trace.clone());
    let fiber = mount(&app.root, flaky.clone(), Config::Null, None);
    fiber.settle().await;
    assert!(fiber.error().is_none(), "旧实例首次上线应当成功");
    let record = mounted_record("entry", &fiber, flaky);

    let broken = fake_plugin("broken", "svc", 2, Some(0), trace.clone());
    let outcome = swap_entry(&record, broken).await;
    let message = match &outcome {
        SwapOutcome::Ok { .. } => panic!("新版本失败时应当返回错误"),
        other => other.error_message().unwrap().to_string(),
    };
    assert!(
        matches!(outcome, SwapOutcome::Lost { .. }),
        "新旧实例均不可用时应为 Lost，实际：{}",
        outcome.describe()
    );

    assert!(
        message.contains("broken"),
        "应当带上新版本的失败原因：{message}"
    );
    assert!(
        message.contains("flaky"),
        "应当同时带上回滚失败的原因：{message}"
    );
}

/// M1-2（红→绿）：**回滚成功之后，下一次热重载必须能够正常执行**。
///
/// 缺陷（修复前）：`commit_outcome` 在失败分支只推进指纹，`slot.fiber` 仍指向
/// 已被 dispose 的旧实例；而回滚挂起的 `restored` 实例无人记录、继续持有服务名。
/// 于是第二次重载会与"幽灵实例"冲突：新实例 `provide` 失败 → 回滚也失败 → `Lost`。
#[tokio::test]
async fn second_reload_succeeds_after_a_rollback() {
    let app = cordis_core::App::new();
    let trace = Arc::new(Mutex::new(Vec::new()));

    // 第一版上线（v1，值=1）。
    let old = fake_plugin("old", "svc", 1, None, trace.clone());
    let fiber = mount(&app.root, old.clone(), Config::Null, None);
    fiber.settle().await;
    assert_eq!(read_u32(&app.root, "svc"), Some(1));
    let mut record = mounted_record("entry", &fiber, old);

    // 第一次重载：坏版本（apply 失败）⇒ 回滚成功，记录必须指回 restored。
    let broken = fake_plugin("broken", "svc", 2, Some(0), trace.clone());
    let outcome = swap_entry(&record, broken).await;
    assert!(
        matches!(outcome, SwapOutcome::RolledBack { .. }),
        "第一次应回滚成功，实际：{}",
        outcome.describe()
    );
    record.commit_outcome(outcome, None);
    assert_eq!(
        read_u32(&app.root, "svc"),
        Some(1),
        "回滚后旧值必须仍在服务"
    );

    // 第二次重载：好版本（值=3）——必须成功（修复前：与幽灵实例冲突 ⇒ Lost）。
    let fresh = fake_plugin("fresh", "svc", 3, None, trace.clone());
    let outcome = swap_entry(&record, fresh).await;
    match outcome {
        SwapOutcome::Ok { .. } => {}
        other => panic!("回滚后的二次重载必须成功，实际：{}", other.describe()),
    }
    assert_eq!(
        read_u32(&app.root, "svc"),
        Some(3),
        "二次重载后应运行新版本"
    );
}

/// M1-2（红→绿）：`Lost`（新旧实例都不可用）必须**指示调用方移除该条目**，
/// 而不是把它留在表里伪装成"有实例"。
#[tokio::test]
async fn lost_outcome_marks_entry_for_removal() {
    let app = cordis_core::App::new();
    let trace = Arc::new(Mutex::new(Vec::new()));

    // 旧实例首次成功；回滚那一次失败 ⇒ 新版本失败 + 回滚失败 = Lost。
    let flaky = fake_plugin("flaky", "svc", 1, Some(1), trace.clone());
    let fiber = mount(&app.root, flaky.clone(), Config::Null, None);
    fiber.settle().await;
    let mut record = mounted_record("entry", &fiber, flaky);

    let broken = fake_plugin("broken", "svc", 2, Some(0), trace.clone());
    let outcome = swap_entry(&record, broken).await;
    assert!(
        matches!(outcome, SwapOutcome::Lost { .. }),
        "双失败应为 Lost：{}",
        outcome.describe()
    );
    assert!(
        record.commit_outcome(outcome, None),
        "Lost 必须指示调用方把该条目从装载表移除"
    );
}

/// M1-4 → M4-4（改写）：`!!js` 的判据从"**没有求值器** ⇒ 一律 fail loud"变成
/// "**内置子集现算**"。M1-4 的前提（没有求值器）在 M4-4 起消失，于是这一条的
/// 判据也要跟着变——**不许**为了保住旧断言而假装没有内置子集。
///
/// 现在锁定三件事：
/// 1. `disabled: !!js process.platform` 求值成非空串 ⇒ 这一行是**休眠**
///    （正常状态，不报错、不进挂载序列）；
/// 2. `config.k: !!js 1+1` 求值成功 ⇒ 这一行**照常**进挂载序列；
/// 3. 只有**求不出来**的表达式才报错，且错误带路径与原文。
///
/// 编号那一半（M4-2 修的漂移缺陷）不变：休眠行与报错行都**照样占序号**，
/// 否则 `find_by_label`（回写路径）会落到错误的行上。
#[tokio::test]
async fn desired_rows_evaluate_js_and_keep_labels_stable() {
    let app = cordis_core::App::new();
    let entries = crate::patch::parse_entries(concat!(
        "- id: keep\n  name: keep\n",
        "- id: gate\n  name: gate\n  disabled: !!js process.platform\n",
        "- id: grp\n  name: grp\n  group: true\n  config:\n",
        "    - id: inner-js\n      name: inner-js\n      config:\n        k: !!js 1+1\n",
        "    - id: inner-bad\n      name: inner-bad\n      config:\n        k: !!js \"1 +\"\n",
        "    - id: inner-ok\n      name: inner-ok\n",
        // 同一个 name 的三行：中间那行被禁用组挡住——它**照样**要占序号，
        // 否则最后一行会拿到 `alpha#1`，而回写路径认为它是 `alpha#2`。
        "- name: alpha\n",
        "- id: gated-group\n  name: gated\n  group: true\n  disabled: true\n  config:\n",
        "    - name: alpha\n",
        "- name: alpha\n",
    ))
    .expect("parse");

    let builtin = crate::js::BuiltinJs;
    let mut scopes = RowScopes::default();
    let decision = RowDecision {
        evaluator: &builtin,
    };
    let desired = desired_rows(&entries, &decision, &app.root, &mut scopes);
    let rows = &desired.rows;
    let issues = &desired.issues;
    let labels: Vec<&str> = rows.iter().map(|row| row.label.as_str()).collect();
    assert_eq!(
        labels,
        ["keep", "inner-js", "inner-ok", "alpha#0", "alpha#2"],
        "休眠的行与求值失败的行不进挂载序列（但照样占编号）"
    );
    assert_eq!(
        desired.disabled,
        ["gate", "alpha#1"],
        "休眠行要进报告用的清单（M4-6）：自己的表达式求值为真、或**被禁用组挡住**的行都要点名"
    );
    assert_eq!(issues.len(), 1, "只有求不出来的那一处报错：{issues:?}");
    assert!(
        issues[0].issue.contains("at inner-bad.config.k"),
        "错误必须带路径：{issues:?}"
    );
    assert!(
        issues[0].issue.contains("1 +"),
        "错误必须带原文：{issues:?}"
    );
    assert!(
        !issues.iter().any(|item| item.label.contains("alpha#1")),
        "禁用（休眠）的行不该报表达式问题：{issues:?}"
    );

    // 无漂移：回写路径按**同一个编号**找到的是最后一行，不是被禁组里那一行。
    let mut tree = entries.clone();
    find_by_label(&mut tree, "alpha#2")
        .expect("alpha#2 必须能找到")
        .disabled = true;
    assert!(tree[5].disabled, "alpha#2 应当是最后一个 alpha 行");
    assert!(!tree[3].disabled, "alpha#0 不得被回写成 alpha#2");
    assert!(
        !tree[4].children[0].disabled,
        "被禁组里的 alpha 行占的是 alpha#1，回写不得落到它头上"
    );
}

/// M4-3：**稳读**——两遍读不一致就当作"文件正在被写"，这一轮一个字节都不采纳。
///
/// 真实场景：编辑器"写临时文件 + rename"、或程序截断重写；两次读之间正好落在中间，
/// 就会读到半截内容。这里把读卡器注入进来，把那一刻钉成确定性的（不靠时序）。
#[tokio::test]
async fn unsettled_config_file_is_not_adopted() {
    let dir = std::env::temp_dir().join(format!("cordis-settle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建目录");
    let files = ConfigFiles {
        main: dir.join("cordis.yml"),
        patches: Vec::new(),
    };
    let files_for_stable = files.clone();

    // 读卡器：第一遍给旧内容，第二遍给新内容（"文件正在被写"）。
    let mut reads = 0usize;
    let mut flaky = move |_path: &Path| -> std::io::Result<String> {
        reads += 1;
        Ok(if reads == 1 {
            "第一版\n".into()
        } else {
            "第二版\n".into()
        })
    };
    let settled = read_settled_with(&files, Duration::from_millis(0), &mut flaky)
        .await
        .expect("读");
    assert!(settled.is_none(), "两遍不一致 ⇒ 不采纳");

    // 一致时必须采纳，而且是**两遍都读到**的同一份内容。
    let mut stable = |_path: &Path| -> std::io::Result<String> { Ok("稳定内容\n".into()) };
    let settled = read_settled_with(&files_for_stable, Duration::from_millis(0), &mut stable)
        .await
        .expect("读");
    assert_eq!(settled.expect("一致 ⇒ 采纳").main, "稳定内容\n");
    let _ = std::fs::remove_dir_all(&dir);
}

/// M4-2：`disabled` 位的取值语义是 JS 的 `Boolean(x)`——不是"能当真就行"。
///
/// 空串 / 0 / null 是假，`{}` 与 `[]` **恒为真**。这一条错了会让一行在错误平台上
/// 悄悄上线（或永远不挂），正是 P1-5 那类静默降级的近亲。
#[test]
fn js_truthiness_matches_javascript() {
    let truthy = [
        json!({}),
        json!([]),
        json!("x"),
        json!(1),
        json!(-1),
        json!(true),
    ];
    let falsy = [json!(null), json!(false), json!(0), json!(0.0), json!("")];
    for value in truthy {
        assert!(js_truthy(&value), "{value} 在 JS 里为真");
    }
    for value in falsy {
        assert!(!js_truthy(&value), "{value} 在 JS 里为假");
    }
}

/// P0-2：`dispatchPolicy` 是 `LoaderConfig` 的 camelCase 字段；缺省字段由
/// `DispatchPolicy` 的 `Default` 补齐，整个字段缺省时保持 `None`（回落全局）。
#[test]
fn loader_config_parses_dispatch_policy_from_yaml() {
    let config: LoaderConfig = serde_yaml::from_str(
        "path: cordis.yml\ndispatchPolicy: {timeoutMs: 50, outboundTimeoutMs: 20}\n",
    )
    .expect("camelCase dispatchPolicy 必须能解析");
    assert_eq!(
        config.dispatch_policy,
        Some(DispatchPolicy {
            timeout_ms: Some(50),
            outbound_timeout_ms: Some(20),
        })
    );

    let partial: LoaderConfig =
        serde_yaml::from_str("path: cordis.yml\ndispatchPolicy:\n  timeoutMs: 5\n")
            .expect("缺省字段由 DispatchPolicy::default 补齐");
    assert_eq!(
        partial.dispatch_policy,
        Some(DispatchPolicy {
            timeout_ms: Some(5),
            outbound_timeout_ms: None,
        })
    );

    let absent: LoaderConfig =
        serde_yaml::from_str("path: cordis.yml\n").expect("缺省 dispatchPolicy 不报错");
    assert_eq!(absent.dispatch_policy, None, "缺省时回落进程级策略");
}

/// P0-3：`hostLimits` 是 `LoaderConfig` 的 camelCase 字段；缺省字段由
/// `HostLimits::default` 补齐，整个字段缺省时保持 `None`（回落进程级默认）。
#[test]
fn loader_config_parses_host_limits_from_yaml() {
    let config: LoaderConfig = serde_yaml::from_str(
        "path: cordis.yml\nhostLimits: {maxInstancesPerApp: 3, maxDispatchesPerInstance: 7}\n",
    )
    .expect("camelCase hostLimits 必须能解析");
    assert_eq!(
        config.host_limits,
        Some(HostLimits {
            max_instances_per_app: 3,
            max_dispatches_per_instance: 7,
            ..HostLimits::default()
        })
    );

    let absent: LoaderConfig =
        serde_yaml::from_str("path: cordis.yml\n").expect("缺省 hostLimits 不报错");
    assert_eq!(absent.host_limits, None, "缺省时回落进程级默认");

    let zero = serde_yaml::from_str::<LoaderConfig>(
        "path: cordis.yml\nhostLimits: {maxInstancesPerApp: 0}\n",
    );
    assert!(zero.is_err(), "0 不是“无限”，必须作为非法配置拒绝");
}

#[test]
fn loader_config_new_has_safe_defaults() {
    let config = LoaderConfig::new("cordis.yml");
    assert_eq!(config.path, "cordis.yml");
    assert!(config.patches.is_empty());
    assert!(config.plugin_dir.is_none());
    assert!(config.strict, "默认 fail loud");
    assert!(!config.strict_cycles);
    assert!(!config.strict_sidecars);
    assert!(config.hmr.is_none());
    assert!(config.js_evaluator.is_none());
    assert!(config.dispatch_policy.is_none());
    assert!(config.host_limits.is_none());
    assert!(config.dependency_plan.is_empty());
}

/// P1-1：`dependencyPlan` 是 `LoaderConfig` 的 camelCase 字段；缺省为空表示
/// 不做预检。条目里的 `provides` / `requires` 也允许缺省。
#[test]
fn loader_config_parses_dependency_plan_from_yaml() {
    let config: LoaderConfig = serde_yaml::from_str(
        "path: cordis.yml\ndependencyPlan:\n  - id: a\n    version: 1.0.0\n    provides: {svc: 1.0.0}\n    requires: {svc: \"^1\"}\n",
    )
    .expect("camelCase dependencyPlan 必须能解析");
    assert_eq!(config.dependency_plan.len(), 1);
    assert_eq!(config.dependency_plan[0].id, "a");
    assert_eq!(config.dependency_plan[0].provides["svc"], "1.0.0");
    assert_eq!(config.dependency_plan[0].requires["svc"], "^1");

    let absent: LoaderConfig =
        serde_yaml::from_str("path: cordis.yml\n").expect("缺省 dependencyPlan 不报错");
    assert!(absent.dependency_plan.is_empty(), "缺省表示不做依赖预检");
}

/// `strictSidecars` 是 `LoaderConfig` 的 camelCase 字段；缺省 `false` 保持既有行为。
#[test]
fn loader_config_parses_strict_sidecars_from_yaml() {
    let absent: LoaderConfig =
        serde_yaml::from_str("path: cordis.yml\n").expect("缺省 strictSidecars 不报错");
    assert!(!absent.strict_sidecars, "缺省必须是 false（兼容既有配置）");

    let strict: LoaderConfig = serde_yaml::from_str("path: cordis.yml\nstrictSidecars: true\n")
        .expect("camelCase strictSidecars 必须能解析");
    assert!(strict.strict_sidecars);
}

/// M1-4 → M4-4（改写）：装载点对 `!!js` 的处置。
///
/// M1-4 的判据是"任何 `!!js` 都不许上线（报'暂不支持'）"——那时的前提是**没有求值器**。
/// 内置子集（M4-4a）落地后这个前提消失，判据变成三条：
/// 1. **算得出** ⇒ 真的用求值结果：`disabled: !!js process.platform` 是**休眠行**，
///    装载器整体成功、一行都不挂，且**不再**报"暂不支持"；
/// 2. **算不出**（子集外 / 语法错）⇒ strict 模式整体失败，错误带路径与原文；
/// 3. `strict=false` ⇒ 降级：那一行不挂、失败进 `last_outcome().failures`，
///    并计数进 `interpolation.failed`（`loader.report` 的汇总口径）。
///
/// 这三条**不需要任何动态库**：表达式在挂载之前就决定了这一行的命运，
/// 根本走不到插件解析（`DiskPluginSource` 在这里永远不会被调用）。
#[tokio::test]
async fn js_evaluation_gates_mount_and_respects_strict() {
    let dir = std::env::temp_dir().join(format!("cordis-js-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("cordis.yml");
    let source = Arc::new(DiskPluginSource);
    let config = |strict: bool| LoaderConfig {
        path: path.to_string_lossy().into_owned(),
        patches: Vec::new(),
        plugin_dir: None,
        strict,
        strict_cycles: strict,
        strict_sidecars: false,
        hmr: None,
        js_evaluator: None,
        interpolation: InterpolationMode::Builtin,
        dispatch_policy: None,
        host_limits: None,
        dependency_plan: Vec::new(),
    };

    // ① 算得出：休眠行 ⇒ 一行都不挂，但装载成功。
    std::fs::write(
        &path,
        "- id: a\n  name: alpha\n  disabled: !!js process.platform\n",
    )
    .expect("write config");
    let app = app_root();
    let state = load_with_source(app.root.clone(), config(true), source.clone())
        .await
        .expect("求得出就该成功（这一行只是休眠）");
    assert!(state.mounted_labels().is_empty(), "休眠行不挂");
    assert_eq!(state.interpolation_failures(), 0, "求得出不算失败");

    // ② 算不出：strict ⇒ 整体失败，错误带路径与原文。
    std::fs::write(&path, "- id: a\n  name: alpha\n  disabled: !!js \"1 +\"\n")
        .expect("write config");
    let app = app_root();
    let error = load_with_source(app.root.clone(), config(true), source.clone())
        .await
        .expect_err("strict 模式下求值不出来就不许上线");
    let message = error.to_string();
    assert!(
        message.contains("!!js 求值失败"),
        "错误必须说明是插值失败：{message}"
    );
    assert!(
        message.contains("at a.disabled"),
        "错误必须带路径：{message}"
    );
    assert!(message.contains("1 +"), "错误必须带原文：{message}");

    // ③ strict=false ⇒ 降级为警告 + 跳过 + 计数。
    let app = app_root();
    let state = load_with_source(app.root.clone(), config(false), source)
        .await
        .expect("strict=false 只降级");
    assert!(state.mounted_labels().is_empty(), "算不出来的行不挂");
    assert_eq!(state.interpolation_failures(), 1, "失败过一次");
    let outcome = state.last_outcome().expect("对账产物");
    assert!(
        outcome
            .failures
            .iter()
            .any(|failure| failure.contains("at a.disabled")),
        "失败要进对账产物：{:?}",
        outcome.failures
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// M4-5（P2）：`commit_outcome` 必须**刷新 volatile 声明缓存**。
///
/// 声明是**插件版本**的一部分：新插件可能声明另一组路径。缓存不跟着换，下一次
/// diff 就会拿旧路径判"只差 volatile"——把该重挂的更新误判成原地提交（或反过来，
/// 把能原地提交的更新拖成重挂）。
#[tokio::test]
async fn commit_outcome_refreshes_the_volatile_declaration_cache() {
    let app = cordis_core::App::new();
    let trace = Arc::new(Mutex::new(Vec::new()));

    let old = declare_volatile(fake_plugin("old", "svc", 1, None, trace.clone()), &["data"]);
    let fiber = mount(&app.root, old.clone(), json!({"data": 1, "next": 1}), None);
    fiber.settle().await;
    assert!(fiber.error().is_none(), "旧实例应当正常上线");

    let mut record = mounted_record("entry", &fiber, old);
    record.config = json!({"data": 1, "next": 1});
    record.volatile = Some(vec!["data".to_string()]);
    record.declared = Entry {
        id: Some("entry".into()),
        name: "entry".into(),
        ..Entry::default()
    };
    record.scope_inputs = vec![ScopeInput {
        label: "entry".into(),
        isolate: None,
        intercept: None,
    }];

    // 新插件声明**另一组** volatile 路径。
    let next = declare_volatile(
        fake_plugin("next", "svc", 2, None, trace.clone()),
        &["next"],
    );
    let outcome = swap_entry(&record, next).await;
    assert!(
        matches!(outcome, SwapOutcome::Ok { .. }),
        "新版本应当换成功：{}",
        outcome.describe()
    );
    record.commit_outcome(outcome, None);
    assert_eq!(
        record.volatile,
        Some(vec!["next".to_string()]),
        "声明缓存必须跟着新插件走"
    );

    // 只差 `next` 的配置变更必须走 Action::Volatile（旧缓存会把它误判成普通更新）。
    let state = LoaderState::new(
        app.root.clone(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        true,
        ConfigFiles {
            main: PathBuf::from("no-such-cordis.yml"),
            patches: Vec::new(),
        },
        Vec::new(),
        Duration::from_millis(0),
        Arc::new(DiskPluginSource),
        Arc::new(crate::js::BuiltinJs),
        Arc::new(AtomicU64::new(0)),
        InterpolationMode::Builtin,
        PatchOwners::default(),
        String::new(),
        None,
        None,
        Vec::new(),
        true,
        false,
    );
    state
        .rows
        .lock()
        .unwrap()
        .insert("entry".into(), RowSlot::Mounted(Box::new(record.clone())));
    let entries = crate::patch::parse_entries(
        "- id: entry\n  name: entry\n  config:\n    data: 1\n    next: 2\n",
    )
    .expect("parse");
    assert_eq!(
        state.plan(&entries),
        vec![Action::Volatile {
            label: "entry".into(),
            config: json!({"data": 1, "next": 2}),
        }],
        "只差声明过的 `next` ⇒ 原地提交，不重挂"
    );

    // 回滚路径：声明缓存必须退回**线上旧插件**的声明。
    //
    // 注意 `record.plugin` 此时已经是 `next`——回滚恢复的就是它。所以另起一份
    // 记录（旧插件声明 data），再让一个声明 next 的新版本 apply 失败。
    let app2 = cordis_core::App::new();
    let old_two = declare_volatile(
        fake_plugin("old-two", "svc", 1, None, trace.clone()),
        &["data"],
    );
    let fiber_two = mount(
        &app2.root,
        old_two.clone(),
        json!({"data": 1, "next": 1}),
        None,
    );
    fiber_two.settle().await;
    let mut record_two = mounted_record("entry", &fiber_two, old_two);
    record_two.config = json!({"data": 1, "next": 1});
    record_two.volatile = Some(vec!["next".to_string()]); // 假装缓存被新声明污染
    let broken = declare_volatile(fake_plugin("broken", "svc", 3, Some(0), trace), &["next"]);
    let outcome = swap_entry(&record_two, broken).await;
    assert!(
        matches!(outcome, SwapOutcome::RolledBack { .. }),
        "新版本 apply 失败必须回滚：{}",
        outcome.describe()
    );
    record_two.commit_outcome(outcome, None);
    assert_eq!(
        record_two.volatile,
        Some(vec!["data".to_string()]),
        "回滚后线上是旧插件，声明缓存必须跟着退回去"
    );
}

/// 每个用例一个干净的 root（装载器会往它上面挂服务与监听器）。
///
/// 返回 `App` 而不是单独返回 `root`：根 fiber 的强引用在 `App` 上，
/// 单独把 root 移出去会让它失去 fiber 身份。
fn app_root() -> cordis_core::App {
    cordis_core::App::new()
}

/// 并发 append 不能丢更新：两个线程各自追加一条，最终两条都在。
#[test]
fn concurrent_append_keeps_both_records() {
    let dir = std::env::temp_dir().join(format!(
        "cordis-append-{}",
        std::process::id() as u64 * 1_000_000
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as u64
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cordis.patch.yml");
    std::fs::write(&path, "- id: base\n").unwrap();

    std::thread::scope(|scope| {
        scope.spawn(|| append_atomically(&path, "- id: a\n").unwrap());
        scope.spawn(|| append_atomically(&path, "- id: b\n").unwrap());
    });

    let text = std::fs::read_to_string(&path).unwrap();
    let ids: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("- id: "))
        .collect();
    assert_eq!(
        ids.iter().filter(|id| **id == "a").count(),
        1,
        "a 必须恰好一条：{ids:?}"
    );
    assert_eq!(
        ids.iter().filter(|id| **id == "b").count(),
        1,
        "b 必须恰好一条：{ids:?}"
    );
    assert!(ids.contains(&"base"), "原有记录必须保留：{ids:?}");

    // 锁文件必须随 append 结束一起消失。
    assert!(
        !dir.join(".cordis.patch.yml.lock").exists(),
        "锁文件必须清理"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 内容相同不重写：用只读文件证明写入路径没有被触达。
#[test]
fn identical_content_does_not_attempt_replacement() {
    let dir = std::env::temp_dir().join(format!(
        "cordis-noop-{}",
        std::process::id() as u64 * 1_000_000
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as u64
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cordis.yml");
    std::fs::write(&path, "same\n").unwrap();
    let original = std::fs::metadata(&path).unwrap().permissions();
    let mut read_only = original.clone();
    read_only.set_readonly(true);
    std::fs::set_permissions(&path, read_only).unwrap();

    // 若比较逻辑失效，会尝试替换只读目标并失败（Windows 上尤其确定）。
    write_atomically(&path, "same\n").expect("相同内容必须直接返回");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "same\n");

    std::fs::set_permissions(&path, original).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
