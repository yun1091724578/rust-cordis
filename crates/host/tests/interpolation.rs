//! `!!js` 表达式：**识别**（M1-4）与**求值**（M4-4）。
//!
//! # 为什么必须有这一层
//! `!!js` 是 YAML 的**标准标签**（`tag:yaml.org,2002:js`）。serde_yaml 0.9 只把
//! **单 `!` 本地标签**暴露成 tagged value，`!!js` 会被直接丢弃——探针实测：
//! `disabled: !!js process.platform` → `disabled` 悄悄变 `false`，行照常挂载；
//! `config.k: !!js 1+1` → 落成字符串 `"1+1"`。这正是 cordis 配置里
//! `disabled: !!js ctx.get('profileContext')` 这类写法的现实形态——
//! **静默降级成"也许对也许错"的默认值**，是最难查的一类错误。
//!
//! # 两段交付
//! - **M1-4（识别）**：文本层把 `!!js` 改写成本地标签 `!cordis-js`（serde_yaml 能识别），
//!   解析成 `{"!cordis-js": <主体>}` 单键对象，再由 `JsExpr` 归一化；回写时对称还原成
//!   `!!js`——**引号与注释里的 `!!js` 文本不得误伤**。
//! - **M4-4（求值）**：识别判据不变，变的是"识别到之后怎么办"。内置子集
//!   （[`BuiltinJs`]）是**默认**，宿主的 `js_evaluator` 挂点是**覆盖**；算不出来的形态
//!   （子集外、语法错、值不可表达）仍然**一律报错**，绝不当默认值用。
//!
//! # 失败策略（§8 M4-4）
//! 错误必须含**路径**（`a.config.k`）与**原文片段**；`strict=false` 降级为警告并跳过那一行；
//! 失败次数汇总进 `loader.report.interpolation.failed`。

use cordis_core::{App, Config, Ctx, Dyn, ExternValue, Inject, Plugin};
use cordis_host::js::{capabilities_of, BuiltinJs};
use cordis_host::loader::{
    load_with_source, InterpolationMode, JsEvaluator, LoadedFile, LoaderConfig, PluginSource,
};
use cordis_host::patch::{
    apply_entry_patches, collect_js_expression_issues, dump_entries, js_expression_issues,
    parse_entries, parse_patches, Entry, JsExpr,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------- 夹具

/// 假插件源：**不碰动态库**。
///
/// `provides` 行把**它收到的配置**原样 `provide` 成 `ExternValue`——于是"插值后的值
/// 到底长什么样"可以直接断言（真插件源那一侧由 demo 覆盖）。
struct FakeSource;

impl PluginSource for FakeSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        // `quiet`：只挂上、不提供任何服务——一次装载里要挂多行时的夹具
        // （`seen` 一个键只能有一个提供者，多行 `provides` 会撞车）。
        let (name, provides) = match entry.name.as_str() {
            "provides" => ("provides", true),
            "quiet" => ("quiet", false),
            other => return Err(format!("假插件源不认识 \"{other}\"")),
        };
        let plugin: Arc<dyn Plugin> =
            cordis_core::plugin_fn(name, Inject::default(), move |ctx: Ctx, config| {
                Box::pin(async move {
                    if provides {
                        ctx.provide("seen", Dyn::new(ExternValue(config)))?;
                    }
                    Ok(())
                })
            });
        let path = PathBuf::from(format!("<fake:{name}>"));
        Ok(LoadedFile {
            plugin,
            source: path.clone(),
            staged: path,
            fingerprint: None,
        })
    }
}

/// `provides` 行实际收到的配置。
fn seen(app: &App) -> Value {
    app.root
        .get("seen")
        .and_then(|value| value.downcast::<ExternValue>())
        .expect("seen 服务")
        .0
        .clone()
}

/// 剧本式求值器：把闭包装成 [`JsEvaluator`]（宿主注入引擎即走这条挂点）。
struct FnJs<F>(F);

impl<F> JsEvaluator for FnJs<F>
where
    F: Fn(&str, &Ctx) -> Result<Config, String> + Send + Sync,
{
    fn evaluate(&self, expr: &str, ctx: &Ctx) -> Result<Config, String> {
        (self.0)(expr, ctx)
    }
}

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

fn config_for(path: &Path, evaluator: Option<Arc<dyn JsEvaluator>>, strict: bool) -> LoaderConfig {
    LoaderConfig {
        path: path.to_string_lossy().into_owned(),
        patches: Vec::new(),
        plugin_dir: None,
        strict,
        strict_cycles: strict,
        hmr: None,
        js_evaluator: evaluator,
        interpolation: InterpolationMode::Builtin,
        dispatch_policy: None,
        host_limits: None,
        dependency_plan: Vec::new(),
    }
}

// ---------------------------------------------------------------- M4-4a：内置子集

/// M4-4a：内置求值器覆盖文档里的**全部子集形态**（表驱动）。
///
/// 这张表就是"内置子集"的规格：每一条都对应 JS 里的一处语义。表里刻意混进几条
/// **语义边界**（`==` 的隐式转换、`'a' + null`、浮点、`undefined === null`、
/// `'abc' < 5`）——它们是"我们的值怎么对应 JS 的值"的契约。
#[tokio::test]
async fn builtin_subset_evaluates_documented_forms() {
    let app = App::new();
    let ctx = app.root.clone();
    ctx.provide(
        "profileContext",
        Dyn::new(ExternValue(
            json!({ "name": "desktop", "nested": { "port": 3080 } }),
        )),
    )
    .expect("provide");

    let js = BuiltinJs;
    let cases: Vec<(&str, Value)> = vec![
        // 字面量 / 算术 / 括号
        ("1 + 1", json!(2)),
        ("(1 + 2) * 3", json!(9)),
        ("7 / 2", json!(3.5)),
        ("7 % 3", json!(1)),
        ("2 * 3.5", json!(7)),
        ("0.1 + 0.2", json!(0.1 + 0.2)),
        // 字符串（含转义）与 `+` 的重载
        ("'a' + 1", json!("a1")),
        ("'a' + null", json!("anull")),
        ("\"b\" + \"c\"", json!("bc")),
        ("'tab\\t'.length", json!(4)),
        // 真值 / 逻辑 / 三元 / `??`
        ("!''", json!(true)),
        ("!'x'", json!(false)),
        ("!!0", json!(false)),
        ("true ? 'yes' : 'no'", json!("yes")),
        ("true && 'yes' || 'no'", json!("yes")),
        ("0 || 'no'", json!("no")),
        ("'a' && 'b'", json!("b")),
        ("null ?? 'fallback'", json!("fallback")),
        ("undefined === null", json!(true)),
        // 相等与比较（`==` 走 JS 的隐式转换，`===` 严格）
        ("'x' == 'x'", json!(true)),
        ("'5' == 5", json!(true)),
        ("0 == false", json!(true)),
        ("'5' === 5", json!(false)),
        ("2 >= 2", json!(true)),
        ("'b' > 'a'", json!(true)),
        ("'abc' < 5", json!(false)),
        // `process` / `env`
        (
            "process.platform === 'win32' ? 'win' : 'posix'",
            json!(if cfg!(windows) { "win" } else { "posix" }),
        ),
        ("process.platform.length > 2", json!(true)),
        (
            "process.version",
            json!(format!("v{}", env!("CARGO_PKG_VERSION"))),
        ),
        (
            "process.cwd()",
            json!(std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string()),
        ),
        (
            "process.env.CORDIS_M44_DEFINITELY_UNSET ?? 'unset'",
            json!("unset"),
        ),
        ("env.CORDIS_M44_DEFINITELY_UNSET ?? 'unset'", json!("unset")),
        (
            "process.env.PATH === undefined ? 'missing' : 'present'",
            json!("present"),
        ),
        // 强转
        ("Number('42')", json!(42)),
        ("Number('  42  ')", json!(42)),
        ("Number('')", json!(0)),
        ("String(3080)", json!("3080")),
        ("String(null)", json!("null")),
        ("Boolean('')", json!(false)),
        ("Boolean(ctx.get('profileContext'))", json!(true)),
        // JSON.parse（子集里唯一允许的 parse 形态）
        (r#"JSON.parse('{"port":3080}').port"#, json!(3080)),
        (r#"JSON.parse("[1,2,3]")[1]"#, json!(2)),
        // 服务读取：`ctx.get(字面量)`、`ctx.<name>`、`?.`、`??`
        ("ctx.get('profileContext').name", json!("desktop")),
        ("ctx.profileContext.nested.port", json!(3080)),
        ("ctx.get('profileContext')?.name", json!("desktop")),
        ("ctx.get('nope')?.name ?? 'unknown'", json!("unknown")),
        (
            "ctx.get('profileContext')?.missing?.deeper ?? 'unknown'",
            json!("unknown"),
        ),
        ("!ctx.get('nope')", json!(true)),
        ("'abcdef'.length", json!(6)),
        // 注释（块标量里的表达式常常带一行注释）
        ("// 平台判断\nprocess.platform.length > 2", json!(true)),
        ("/* 行内注释 */ process.platform.length > 2", json!(true)),
    ];

    for (expr, expected) in cases {
        let actual = js
            .evaluate(expr, &ctx)
            .unwrap_or_else(|err| panic!("{expr} 求值失败: {err}"));
        assert_eq!(actual, expected, "{expr}");
    }
}

/// M4-4a：**子集之外**的形态一律给出可读错误——绝不静默变成 `null` / 默认值。
///
/// 三种形态各代表一类：
/// 1. **子集外调用**：出厂 bundle 里真实存在的 `process.getBuiltinModule(...)`；
/// 2. **裸标识符**：上游 `with (ctx)` 会去 ctx 上找，本仓**明确拒绝**
///    （`dshHomePath('sessions')` 也是出厂形态；拒绝比猜一个服务名诚实）；
/// 3. **值不可表达**：JS 的 `NaN` / `Infinity` 在 JSON 里没有对应物。
///
/// 再加上装载层的判据：错误必须带**配置路径**与原文片段。
#[tokio::test]
async fn unsupported_syntax_fails_with_path() {
    let app = App::new();
    let ctx = app.root.clone();

    let err = BuiltinJs
        .evaluate("process.getBuiltinModule('node:path').resolve('x')", &ctx)
        .expect_err("子集不含 Node 内建模块");
    assert!(err.contains("getBuiltinModule"), "错误必须点名调用：{err}");
    assert!(err.contains("不支持"), "错误必须说明不在子集里：{err}");

    let err = BuiltinJs
        .evaluate("dshHomePath('sessions')", &ctx)
        .expect_err("子集不解析裸标识符");
    assert!(err.contains("dshHomePath"), "错误必须点名标识符：{err}");

    let err = BuiltinJs
        .evaluate("1 / 0", &ctx)
        .expect_err("Infinity 不是 JSON 值");
    assert!(err.contains("JSON"), "错误必须说明值不可表达：{err}");
    let err = BuiltinJs
        .evaluate("Number('abc')", &ctx)
        .expect_err("NaN 不是 JSON 值");
    assert!(err.contains("JSON"), "{err}");

    // 装载层：错误的落点是**配置路径**（`a.config.k`），不是"某一行挂了"。
    let dir = TempDir::new("unsupported");
    let path = dir.write(
        "cordis.yml",
        "- id: a\n  name: provides\n  config:\n    k: !!js \"process.getBuiltinModule('node:path').resolve('x')\"\n",
    );
    let error = load_with_source(ctx, config_for(&path, None, true), Arc::new(FakeSource))
        .await
        .expect_err("strict 模式下求值不出来就不许挂");
    let message = error.to_string();
    assert!(
        message.contains("at a.config.k"),
        "错误必须带路径：{message}"
    );
    assert!(
        message.contains("getBuiltinModule"),
        "错误必须带原文：{message}"
    );
}

/// M4-4c：**宿主注入的引擎覆盖内置子集**——挂点存在的全部意义。
///
/// 出厂 bundle 里有一整类形态（`dshHomePath(...)`、`process.getBuiltinModule(...)`）
/// 是内置子集**故意**不做的：它们的正确语义只有真 JS 引擎给得出。宿主注入引擎后，
/// 这些行照常上线，且插件拿到的是**引擎算出来的值**。
#[tokio::test]
async fn evaluator_hook_overrides_builtin() {
    let app = App::new();
    let expr = "dshHomePath('sessions')";
    assert!(
        BuiltinJs.evaluate(expr, &app.root).is_err(),
        "前提：这个表达式不在内置子集里"
    );

    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let evaluator: Arc<dyn JsEvaluator> = Arc::new(FnJs({
        let calls = calls.clone();
        move |expr: &str, _ctx: &Ctx| {
            calls.lock().unwrap().push(expr.to_string());
            Ok(json!("/tmp/cordis-sessions"))
        }
    }));

    let dir = TempDir::new("override");
    let path = dir.write(
        "cordis.yml",
        "- id: a\n  name: provides\n  config:\n    root: !!js dshHomePath('sessions')\n    keep: 3\n",
    );
    let state = load_with_source(
        app.root.clone(),
        config_for(&path, Some(evaluator), true),
        Arc::new(FakeSource),
    )
    .await
    .expect("注入引擎后必须能挂");
    assert_eq!(state.mounted_labels(), ["a"]);
    assert_eq!(
        seen(&app),
        json!({ "root": "/tmp/cordis-sessions", "keep": 3 }),
        "插件必须拿到引擎算出的值"
    );
    assert!(
        calls.lock().unwrap().iter().any(|call| call == expr),
        "注入的引擎必须真的被调用过：{:?}",
        calls.lock().unwrap()
    );
}

/// M4-4b：**表达式不缓存**——配置变了就重新算（链上那次解析才是真值来源）。
///
/// 上游每次 `internal/config` 解析都 `interpolate`（`loader/src/index.ts:112`），
/// 于是"环境/服务变了"与"配置文本不变"可以并存：同一份表达式给出新答案。
/// 这里用环境变量把这件事钉成确定性的：改文件里的**另一个字段**触发一次
/// `Action::Update`，表达式文本一字未动，但实例上的值必须跟着环境变。
#[tokio::test]
async fn config_expression_reevaluated_on_update() {
    std::env::set_var("CORDIS_M44_UPDATE", "first");
    let app = App::new();
    let dir = TempDir::new("reeval");
    let path = dir.write(
        "cordis.yml",
        "- id: a\n  name: provides\n  config:\n    root: !!js process.env.CORDIS_M44_UPDATE\n",
    );
    let state = load_with_source(
        app.root.clone(),
        config_for(&path, None, true),
        Arc::new(FakeSource),
    )
    .await
    .expect("装载");
    assert_eq!(seen(&app)["root"], json!("first"));

    std::env::set_var("CORDIS_M44_UPDATE", "second");
    dir.write(
        "cordis.yml",
        "- id: a\n  name: provides\n  config:\n    root: !!js process.env.CORDIS_M44_UPDATE\n    other: 2\n",
    );
    let outcome = state.reload().await;
    assert!(
        matches!(outcome, cordis_host::loader::ReloadOutcome::Reconciled(_)),
        "应当走到一次对账：{outcome:?}"
    );
    assert_eq!(
        seen(&app)["root"],
        json!("second"),
        "表达式必须重新求值；缓存住第一次的答案就是配置在撒谎"
    );
    assert_eq!(seen(&app)["other"], json!(2), "普通字段照常更新");
}

/// M4-4b：求值失败要**报得出来**——按行报、带路径与原文、并汇总进 `loader.report`。
///
/// 对齐 vendor 的 `disabled expression failed: SyntaxError`（`README.md` #18）：
/// 那边的失败也是一条**点名这一行**的错误，而不是静默当成 `false`。
#[tokio::test]
async fn evaluation_error_reported() {
    // strict：整体失败，两条路径各报一处（`disabled` 与 `config` 两个位置）。
    let app = App::new();
    let dir = TempDir::new("eval-error");
    let path = dir.write(
        "cordis.yml",
        concat!(
            "- id: gate\n  name: gate\n  disabled: !!js \"1 +\"\n",
            "- id: a\n  name: provides\n  config:\n    k: !!js \"1 +\"\n",
        ),
    );
    let error = load_with_source(
        app.root.clone(),
        config_for(&path, None, true),
        Arc::new(FakeSource),
    )
    .await
    .expect_err("strict 模式必须整体失败");
    let message = error.to_string();
    assert!(
        message.contains("gate.disabled"),
        "错误必须带路径：{message}"
    );
    assert!(message.contains("a.config.k"), "错误必须带路径：{message}");
    assert!(message.contains("1 +"), "错误必须带原文：{message}");
    assert!(
        message.contains("!!js 求值失败"),
        "错误必须说明是插值失败：{message}"
    );

    // strict=false：降级为警告 + 跳过那一行 + 计数进报告。
    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        config_for(&path, None, false),
        Arc::new(FakeSource),
    )
    .await
    .expect("strict=false 只降级，不失败");
    assert!(state.mounted_labels().is_empty(), "算不出来的行不许挂");
    assert_eq!(state.interpolation_failures(), 2, "两处表达式各失败一次");
    let outcome = state.last_outcome().expect("对账产物");
    assert!(
        outcome
            .failures
            .iter()
            .any(|failure| failure.contains("a.config.k")),
        "失败要进对账产物：{:?}",
        outcome.failures
    );
    let report = app
        .root
        .get("loader.report")
        .and_then(|value| value.downcast::<ExternValue>())
        .expect("loader.report 服务")
        .0
        .clone();
    assert_eq!(
        report["interpolation"]["failed"],
        json!(2),
        "报告要汇总失败数：{report}"
    );
}

// ---------------------------------------------------------------- M1-4：识别（M4-4 起不再"暂不支持"）

/// 识别（主配置 + patch 同一条解析路径）：`disabled: !!js` 不得被当成静态 `false`。
///
/// `js_expression_issues` 是**"谁都没注入引擎"口径**的清单（M1-4 的挂载点曾用它
/// fail loud；M4-4 起装载器改走"预检 + `internal/config` 链"，这个函数只作为诊断 API 保留）。
#[test]
fn js_disabled_expression_is_recognized() {
    let entries = parse_entries("- id: a\n  name: alpha\n  disabled: !!js process.platform\n")
        .expect("parse");
    assert!(!entries[0].disabled, "!!js 表达式不得被当作静态 disabled");
    let expr = entries[0]
        .disabled_expr
        .as_ref()
        .expect("!!js disabled 表达式必须被识别，而不是静默丢弃");
    assert_eq!(expr.source(), "process.platform");
    assert_eq!(expr.expression().expect("标量主体"), "process.platform");

    let issues = js_expression_issues(&entries[0]);
    assert_eq!(issues.len(), 1, "应当恰好报一条: {issues:?}");
    assert!(
        issues[0].contains("process.platform"),
        "issue 必须带表达式原文: {}",
        issues[0]
    );
    assert!(
        issues[0].contains("at a.disabled"),
        "issue 必须带路径: {}",
        issues[0]
    );

    // patch 层与主配置层是同一条解析路径（都走 `parse_*`），这里锁定它不会漏过识别。
    let entries = parse_entries("- id: a\n  name: alpha\n").expect("parse");
    let patches = parse_patches("- id: a\n  disabled: !!js process.platform\n").expect("patches");
    let mut warned: Vec<String> = Vec::new();
    let entries = apply_entry_patches(entries, &patches, &mut |message| {
        warned.push(message.to_string())
    });
    assert!(warned.is_empty(), "patch 应当命中目标: {warned:?}");
    let expr = entries[0]
        .disabled_expr
        .as_ref()
        .expect("patch 的 !!js 必须被识别");
    assert_eq!(expr.source(), "process.platform");
    assert!(!entries[0].disabled, "表达式不得被当作静态 disabled");
}

/// 识别：`config` 里任意深度的 `!!js` 都要保留**表达式本体**（而不是降级成字符串）。
#[test]
fn js_config_expression_is_recognized() {
    let entries = parse_entries("- id: b\n  name: beta\n  config:\n    k: !!js 1+1\n    n: 3\n")
        .expect("parse");
    let config = entries[0].config.as_ref().expect("config");

    assert_eq!(config["n"], json!(3), "普通字段不受影响");
    // 表达式本体必须保留（而不是降级成 "1+1"）
    assert_eq!(
        config["k"],
        json!({ "!cordis-js": "1+1" }),
        "表达式本体必须以标记形态保留"
    );
    assert_eq!(
        JsExpr::from_value(&config["k"])
            .expect("标记")
            .expression()
            .expect("标量主体"),
        "1+1"
    );

    let issues = js_expression_issues(&entries[0]);
    assert_eq!(
        issues.len(),
        1,
        "应当恰好报一条（只有 k 是表达式）: {issues:?}"
    );
    assert!(
        issues[0].contains("at b.config.k"),
        "issue 必须带完整路径: {}",
        issues[0]
    );
    assert!(
        collect_js_expression_issues(&parse_entries("- id: b\n  name: beta\n").expect("parse"))
            .is_empty(),
        "没有表达式的树不报问题"
    );
}

/// 识别：`!!js` 的**主体必须是标量**——`!!js {a: 1}` 这种"块标量对象"是出厂形态之一，
/// 它不是表达式。上游的 `__jsExpr` 进 `eval` 同样错，所以这里给它一条带形态的错误，
/// 而不是把紧凑 JSON 当表达式去 parse（那会让报错指向一段用户没写过的字符串）。
#[test]
fn js_mapping_body_is_not_an_expression() {
    let entries =
        parse_entries("- id: c\n  name: gamma\n  config:\n    obj: !!js {a: 1}\n").expect("parse");
    let marker =
        JsExpr::from_value(&entries[0].config.as_ref().expect("config")["obj"]).expect("标记");
    let err = marker.expression().expect_err("映射主体不是表达式");
    assert!(err.contains("标量"), "错误必须说明主体形态：{err}");
}

// ---------------------------------------------------------------- M1-4：往返保真

/// 往返保真：parse → dump → parse 必须稳定，且**不得泄漏**内部标记 `!cordis-js`。
///
/// （M1-4 落地，M4-4 未改判据：读写两侧的镜像改名是"serde_yaml 只认单 `!` 标签"的补偿。）
#[test]
fn js_round_trips_through_dump() {
    let source = concat!(
        "- id: a\n",
        "  name: alpha\n",
        "  disabled: !!js process.platform\n",
        "  config:\n",
        "    quoted: !!js \"!ctx.get('x')\"\n",
        "    block: !!js |\n",
        "      line1\n",
        "      line2\n",
    );
    let entries = parse_entries(source).expect("parse");
    let dumped = dump_entries(&entries).expect("dump");

    assert!(dumped.contains("!!js"), "回写必须保留 !!js 标签:\n{dumped}");
    assert!(
        !dumped.contains("!cordis-js"),
        "回写不得泄漏内部标记 !cordis-js:\n{dumped}"
    );

    // 语义稳定：parse → dump → parse 必须得到同一棵树（含块标量内容与引号标量）。
    let again = parse_entries(&dumped).expect("reparse");
    assert_eq!(entries, again, "往返必须稳定:\n{dumped}");
    assert_eq!(
        again[0].config.as_ref().expect("config")["block"],
        json!({ "!cordis-js": "line1\nline2\n" }),
        "块标量内容不得丢失"
    );
}

/// 识别只认**自己的标记**：普通标量、引号里的 `!!js` 文本、注释里的 `!!js` 都不许被改写。
#[test]
fn plain_scalars_and_other_tags_unaffected() {
    let source = concat!(
        "- id: a\n",
        "  name: alpha\n",
        "  config:\n",
        "    plain: hello\n",
        "    num: 3\n",
        "    quoted: '!!js not a tag'\n",
        "    trailing: ok # !!js in a comment\n",
    );
    let entries = parse_entries(source).expect("parse");
    let config = entries[0].config.as_ref().expect("config");

    assert_eq!(config["plain"], json!("hello"));
    assert_eq!(config["num"], json!(3));
    assert_eq!(
        config["quoted"],
        json!("!!js not a tag"),
        "引号内的 !!js 是普通文本，不得被改写"
    );
    assert_eq!(config["trailing"], json!("ok"), "注释不得影响值");
    assert!(
        js_expression_issues(&entries[0]).is_empty(),
        "普通标量不得触发 fail loud: {:?}",
        js_expression_issues(&entries[0])
    );
    assert!(
        collect_js_expression_issues(&entries).is_empty(),
        "整棵树上没有表达式"
    );

    // 往返稳定：引号里的 `!!js` 文本必须原样回来。
    let dumped = dump_entries(&entries).expect("dump");
    let again = parse_entries(&dumped).expect("reparse");
    assert_eq!(entries, again, "普通配置往返必须稳定:\n{dumped}");
}

// ---------------------------------------------------------------- M9：`!!js` 能力化（D5）

/// `config_for` 之上指定**插值模式**（M9）。
fn config_with_mode(
    path: &Path,
    evaluator: Option<Arc<dyn JsEvaluator>>,
    strict: bool,
    interpolation: InterpolationMode,
) -> LoaderConfig {
    LoaderConfig {
        interpolation,
        ..config_for(path, evaluator, strict)
    }
}

/// `loader.report` 的值（运维读的那一面）。
fn report_of(root: &Ctx) -> Value {
    root.require("loader.report")
        .ok()
        .and_then(|value| value.downcast::<ExternValue>())
        .expect("loader.report 必须由装载器提供")
        .0
        .clone()
}

/// M9-1（D5）：`!!js` 表达式静态读到的宿主面**进能力清单**——env 名 / 宿主事实 / `ctx.get` 的服务。
///
/// 这是"装载前审阅"的数据面：运维/宿主在信任一份配置之前，要能一眼看出它会去读什么。
#[tokio::test]
async fn js_expression_capabilities_are_extracted_and_reported() {
    let dir = TempDir::new("m9-caps");
    let path = dir.write(
        "cordis.yml",
        concat!(
            "- id: gate\n",
            "  name: quiet\n",
            // env 引用：变量没设 ⇒ `!!` 为假 ⇒ 这一行照常挂上（我们要看的是它的**能力面**）。
            "  disabled: !!js \"!!process.env.CORDIS_M9_UNSET\"\n",
            "- id: plain\n",
            "  name: quiet\n",
            "- id: shape\n",
            "  name: provides\n",
            "  config:\n",
            "    k: !!js \"ctx.get('profileContext') === null || process.platform !== 'linux'\"\n",
        ),
    );
    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        config_for(&path, None, true),
        Arc::new(FakeSource),
    )
    .await
    .expect("三个表达式在内置子集里都算得出 ⇒ 装载成功");
    assert_eq!(
        state.mounted_labels(),
        vec!["gate".to_string(), "plain".to_string(), "shape".to_string()],
        "三行都在（gate 的 `!!env` 为假 ⇒ 不休眠）"
    );

    let rows = state.capabilities();
    let row = |label: &str| -> Value {
        rows.iter()
            .find(|row| row["label"] == json!(label))
            .cloned()
            .unwrap_or_else(|| panic!("能力清单里必须有 {label}: {rows:?}"))
    };
    assert_eq!(
        row("gate")["js"],
        json!({ "env": ["CORDIS_M9_UNSET"], "host": [], "services": [] }),
        "环境变量只进**名字**"
    );
    assert_eq!(
        row("shape")["js"],
        json!({ "env": [], "host": ["platform"], "services": ["profileContext"] }),
        "宿主事实与服务引用都要抽出来"
    );
    assert_eq!(
        row("plain")["js"],
        json!({ "env": [], "host": [], "services": [] }),
        "没有表达式 ⇒ 空快照（形状稳定，便于逐字断言）"
    );

    // 同一份快照也进 `loader.report`（运维读的那一面，按 label 升序）。
    let report = report_of(&app.root);
    assert_eq!(
        report["capabilities"],
        json!([
            { "label": "gate", "plugin": "quiet", "inject": [], "provision": [],
              "volatile": [], "js": { "env": ["CORDIS_M9_UNSET"], "host": [], "services": [] } },
            { "label": "plain", "plugin": "quiet", "inject": [], "provision": [],
              "volatile": [], "js": { "env": [], "host": [], "services": [] } },
            { "label": "shape", "plugin": "provides", "inject": [], "provision": [],
              "volatile": [], "js": { "env": [], "host": ["platform"], "services": ["profileContext"] } },
        ]),
        "报告里的能力清单要带上 js 面：{report}"
    );

    // 抽取是**词法**的（不看这一行能不能挂上）：`?.`、`env.X`、`process.version` 同样认。
    let direct = capabilities_of(
        "ctx?.get('profileContext')?.name ?? env.CORDIS_M9_UNSET ?? process.version",
    );
    assert_eq!(direct.env, vec!["CORDIS_M9_UNSET".to_string()]);
    assert_eq!(direct.host, vec!["version".to_string()]);
    assert_eq!(direct.services, vec!["profileContext".to_string()]);
    assert_eq!(
        capabilities_of("process.platform").host,
        vec!["platform".to_string()],
        "`process.platform` 是 D5 点名的宿主事实"
    );
    // 非字面量参数抽不出名字（错误由求值那条路报）——绝不编造引用。
    assert!(
        capabilities_of("ctx.get(动态名字)").services.is_empty(),
        "认不出就是空快照，不猜"
    );
}

/// M9-2（D5）：`interpolation = "injected"` ⇒ **只用注入的引擎**；没注入就不许求值。
///
/// 判据之所以能成立，是因为 `process.platform` 在内置子集里**算得出**——
/// 悄悄回落内置子集也能让这个配置"看起来成功"，而这正是要拒绝的那件事。
#[tokio::test]
async fn interpolation_injected_mode_rejects_an_unevaluated_expression() {
    let dir = TempDir::new("m9-injected");
    let path = dir.write(
        "cordis.yml",
        "- id: gate\n  name: provides\n  disabled: !!js process.platform\n",
    );
    let app = App::new();
    let error = load_with_source(
        app.root.clone(),
        config_with_mode(&path, None, true, InterpolationMode::Injected),
        Arc::new(FakeSource),
    )
    .await
    .expect_err("injected 模式 + 没注入引擎 ⇒ 表达式不许被求值");
    let error = error.to_string();
    assert!(error.contains("injected"), "错误要说清是模式问题：{error}");
    assert!(error.contains("at gate.disabled"), "错误要带路径：{error}");
    assert!(error.contains("process.platform"), "错误要带原文：{error}");
    assert!(app.root.get("seen").is_none(), "失败的行不许挂上");

    // 注入了引擎 ⇒ injected 模式照常用它（模式的另一侧）。
    let path = dir.write(
        "cordis2.yml",
        "- id: gate\n  name: provides\n  config:\n    k: !!js \"宿主才算得出来()\"\n",
    );
    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        config_with_mode(
            &path,
            Some(Arc::new(FnJs(|expr: &str, _ctx: &Ctx| {
                Ok(Config::String(format!("宿主回答 {expr}")))
            }))),
            true,
            InterpolationMode::Injected,
        ),
        Arc::new(FakeSource),
    )
    .await
    .expect("注入引擎之后不该失败");
    assert_eq!(state.mounted_labels(), vec!["gate".to_string()]);
    assert_eq!(
        seen(&app)["k"],
        json!("宿主回答 宿主才算得出来()"),
        "injected 模式下生效的必须是注入的引擎"
    );
}

/// M9-3（**守卫**）：默认模式与显式 `builtin` 逐字保持 M4-4 的行为。
#[tokio::test]
async fn interpolation_builtin_mode_is_unchanged() {
    let dir = TempDir::new("m9-builtin");
    let path = dir.write(
        "cordis.yml",
        "- id: gate\n  name: provides\n  disabled: !!js process.platform\n",
    );

    // ① 默认（不写 `interpolation`）：内置子集算得出 ⇒ 休眠行、装载成功、不算失败。
    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        config_for(&path, None, true),
        Arc::new(FakeSource),
    )
    .await
    .expect("默认模式算得出就该成功");
    assert!(state.mounted_labels().is_empty(), "platform 为真 ⇒ 休眠");
    assert_eq!(state.interpolation_failures(), 0, "求得出，不算失败");

    // ② 显式 `builtin`：与默认同一结果（模式是**加法**，不是行为变更）。
    let app = App::new();
    let other = load_with_source(
        app.root.clone(),
        config_with_mode(&path, None, true, InterpolationMode::Builtin),
        Arc::new(FakeSource),
    )
    .await
    .expect("显式 builtin 与默认等价");
    assert!(other.mounted_labels().is_empty());
    assert_eq!(other.interpolation_failures(), 0);

    // ③ 报告里的模式字段：默认 = `"builtin"`。
    assert_eq!(
        report_of(&app.root)["interpolation"]["mode"],
        json!("builtin")
    );
}

/// M9-4（D5 的红线）：能力快照里**只有引用，没有值**——凭据绝不进快照。
#[tokio::test]
async fn credential_values_never_appear_in_the_capability_snapshot() {
    const SECRET: &str = "m9-super-secret-value";
    std::env::set_var("M9_API_KEY", SECRET);
    let dir = TempDir::new("m9-creds");
    let path = dir.write(
        "cordis.yml",
        "- id: chat\n  name: provides\n  config:\n    apiKey: !!js process.env.M9_API_KEY\n",
    );
    let app = App::new();
    load_with_source(
        app.root.clone(),
        config_for(&path, None, true),
        Arc::new(FakeSource),
    )
    .await
    .expect("装载成功");

    // 值确实到了插件（这是配置的本意）……
    assert_eq!(seen(&app)["apiKey"], json!(SECRET));
    // ……但快照里只许出现**名字**。
    let snapshot = serde_json::to_string(&report_of(&app.root)).expect("报告可序列化");
    assert!(
        !snapshot.contains(SECRET),
        "凭据值绝不能出现在能力快照里：{snapshot}"
    );
    assert!(snapshot.contains("M9_API_KEY"), "引用要在：{snapshot}");
}
