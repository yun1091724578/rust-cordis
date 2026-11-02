//! `cordis.patch.yml` 兼容层的语义测试。
//!
//! 每条都对应 `vendor/include/src/index.ts:57-127` 的一条行为，
//! 也是现有 cordis 配置文件能不能原样喂进 Rust 内核的判据。

use cordis_core::Config;
use cordis_host::patch::{apply_entry_patches, dump_overlay, parse_entries, parse_patches};
use serde_json::json;

fn run(main: &str, patch: &str) -> (Vec<cordis_host::patch::Entry>, Vec<String>) {
    let entries = parse_entries(main).expect("主配置解析失败");
    let patches = parse_patches(patch).expect("patch 解析失败");
    let mut warnings = Vec::new();
    let result = apply_entry_patches(entries, &patches, &mut |message| {
        warnings.push(message.to_string())
    });
    (result, warnings)
}

#[test]
fn entry_inject_js_fails_loud() {
    let error = parse_entries(
        "- id: a\n  name: alpha\n  inject:\n    settings:\n      namespace: !!js env.NAMESPACE\n",
    )
    .expect_err("entry-level inject !!js 必须 fail loud");
    assert!(error.contains("inject"), "{error}");
    assert!(error.contains("!!js"), "{error}");
    assert!(error.contains("inject.settings.namespace"), "{error}");
}

#[test]
fn entry_intercept_js_fails_loud() {
    let error = parse_entries(
        "- id: a\n  name: alpha\n  intercept:\n    http:\n      proxy: !!js env.PROXY\n",
    )
    .expect_err("entry-level intercept !!js 必须 fail loud");
    assert!(error.contains("intercept"), "{error}");
    assert!(error.contains("!!js"), "{error}");
    assert!(error.contains("intercept.http.proxy"), "{error}");
}

#[test]
fn entry_inject_scoped_shape_is_accepted() {
    let entries = parse_entries(
        "- id: a\n  name: alpha\n  inject:\n    required: [svc]\n    optional: [opt]\n",
    )
    .expect("required/optional 形状必须被解析");
    assert_eq!(
        entries[0].inject,
        Some(json!({"required": ["svc"], "optional": ["opt"]}))
    );
}

#[test]
fn entry_inject_rejects_mixed_scoped_shape() {
    let error =
        parse_entries("- id: a\n  name: alpha\n  inject:\n    required: [svc]\n    extra: {}\n")
            .expect_err("required/optional 形状不能混入服务名");
    assert!(error.contains("inject"), "{error}");
    assert!(error.contains("extra"), "{error}");
}

#[test]
fn entry_inject_array_rejects_non_string_items() {
    let error = parse_entries("- id: a\n  name: alpha\n  inject: [svc, 7]\n")
        .expect_err("inject 数组项必须是字符串");
    assert!(error.contains("inject[1]"), "{error}");
}

#[test]
fn quoted_disabled_is_rejected_instead_of_becoming_false() {
    let error = parse_entries("- id: a\n  name: alpha\n  disabled: 'true'\n")
        .expect_err("字符串 'true' 不是合法的 disabled 声明");
    assert!(error.contains("disabled"), "{error}");
    assert!(error.contains("布尔"), "{error}");
    assert!(error.contains("at disabled"), "错误必须带字段路径：{error}");

    let error = parse_patches("- id: a\n  disabled: 'false'\n")
        .expect_err("patch 的字符串 disabled 同样必须拒绝");
    assert!(error.contains("disabled"), "{error}");
}

#[test]
fn non_mapping_isolate_is_rejected_instead_of_being_ignored() {
    let error = parse_entries("- id: a\n  name: alpha\n  isolate: [sessions]\n")
        .expect_err("isolate 必须是映射，数组不能静默失效");
    assert!(error.contains("isolate"), "{error}");
    assert!(error.contains("映射"), "{error}");

    let error = parse_entries("- id: a\n  name: alpha\n  isolate:\n    sessions: false\n")
        .expect_err("isolate 的值只能是 true 或字符串标签");
    assert!(error.contains("isolate.sessions"), "{error}");
    assert!(error.contains("true"), "{error}");

    let _ = parse_patches("- id: a\n  isolate: [sessions]\n")
        .expect_err("patch 的 isolate 形状也必须校验");
}

#[test]
fn patch_inject_shape_is_validated_before_it_reaches_mount() {
    let error = parse_patches("- id: a\n  inject:\n    required: not-an-array\n")
        .expect_err("非法 patch inject 不能留到 mount 里 panic");
    assert!(error.contains("inject.required"), "{error}");
}

#[test]
fn isolate_null_is_still_the_explicit_clear_form() {
    let entries = parse_entries("- id: a\n  name: alpha\n  isolate:\n    sessions: shared\n")
        .expect("合法的具名隔离要能解析");
    assert_eq!(
        entries[0].isolate,
        Some(json!({ "sessions": "shared" })),
        "解析期不能改写声明"
    );

    let patches = parse_patches("- id: a\n  isolate: null\n").expect("null 是清空");
    assert_eq!(patches[0].isolate, Some(serde_json::Value::Null));
}

#[test]
fn plain_entry_inject_and_intercept_still_parse() {
    let entries = parse_entries(
        "- id: a\n  name: alpha\n  inject:\n    settings: {}\n  intercept:\n    http: { proxy: null }\n",
    )
    .expect("普通 inject/intercept 不受影响");
    assert_eq!(entries.len(), 1);
}

#[test]
fn insert_without_id_appends_to_root() {
    let (entries, warnings) = run(
        "- id: a\n  name: alpha\n",
        "- insert:\n    - id: b\n      name: beta\n",
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].id.as_deref(), Some("b"));
}

#[test]
fn insert_with_id_appends_into_group() {
    let (entries, warnings) = run(
        "- id: g\n  name: cordis:group\n  group: true\n  config: []\n",
        "- insert:\n    - id: child\n      name: alpha\n  id: g\n",
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(entries[0].children.len(), 1);
    assert_eq!(entries[0].children[0].id.as_deref(), Some("child"));
}

#[test]
fn insert_into_non_group_is_warned_and_skipped() {
    let (entries, warnings) = run(
        "- id: a\n  name: alpha\n",
        "- insert:\n    - id: b\n      name: beta\n  id: a\n",
    );
    assert_eq!(entries.len(), 1, "不该被插入");
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("不是 group"));
}

/// **最关键的一条**：`config` 是整体替换，不是深合并。
#[test]
fn config_is_replaced_wholesale_not_merged() {
    let (entries, _) = run(
        "- id: a\n  name: alpha\n  config:\n    keep: 1\n    drop: 2\n",
        "- id: a\n  config:\n    keep: 9\n",
    );
    let config = entries[0].config.as_ref().unwrap();
    assert_eq!(config.get("keep").and_then(|v| v.as_i64()), Some(9));
    assert!(
        config.get("drop").is_none(),
        "整体替换：patch 里没写的键必须消失，而不是保留原值"
    );
}

#[test]
fn later_patch_in_same_layer_can_hit_newly_inserted_row() {
    // 同一层里：先 insert 一行，再 patch 它 —— 边插入边建索引。
    let (entries, warnings) = run(
        "",
        concat!(
            "- insert:\n",
            "    - id: fresh\n",
            "      name: alpha\n",
            "      config:\n",
            "        n: 1\n",
            "- id: fresh\n",
            "  config:\n",
            "    n: 2\n",
        ),
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0]
            .config
            .as_ref()
            .unwrap()
            .get("n")
            .and_then(|v| v.as_i64()),
        Some(2),
        "同层的后续 patch 必须能命中前面刚插入的行"
    );
}

#[test]
fn missing_target_is_warned_not_fatal() {
    let (entries, warnings) = run("- id: a\n  name: alpha\n", "- id: nope\n  disabled: true\n");
    assert_eq!(entries.len(), 1);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("未找到"));
}

#[test]
fn patch_without_insert_needs_id() {
    let (_, warnings) = run("- id: a\n  name: alpha\n", "- config:\n    x: 1\n");
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("必须带 id"));
}

#[test]
fn disabled_and_inject_are_overlaid() {
    let (entries, _) = run(
        "- id: a\n  name: alpha\n",
        "- id: a\n  disabled: true\n  inject: [\"tools\"]\n",
    );
    assert!(entries[0].disabled);
    assert_eq!(
        entries[0]
            .inject
            .as_ref()
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        Some(1)
    );
}

/// group 行的子条目在磁盘上放在 `config` 里，解析时提升为 `children`。
#[test]
fn group_children_round_trip() {
    let yaml = concat!(
        "- id: g\n",
        "  name: cordis:group\n",
        "  group: true\n",
        "  config:\n",
        "    - id: inner\n",
        "      name: alpha\n",
        "      config:\n",
        "        k: v\n",
    );
    let entries = parse_entries(yaml).unwrap();
    assert_eq!(entries[0].children.len(), 1);
    let dumped = cordis_host::patch::dump_entries(&entries).unwrap();
    let reparsed = parse_entries(&dumped).unwrap();
    assert_eq!(reparsed, entries, "序列化再解析必须等价");
}

/// 替换 group 的 `config` 必须同步替换 `children`，否则装载与保存仍用旧子条目。
#[test]
fn group_config_patch_replaces_the_child_list() {
    let (entries, warnings) = run(
        concat!(
            "- id: g\n",
            "  name: cordis:group\n",
            "  group: true\n",
            "  config:\n",
            "    - id: old\n",
            "      name: alpha\n",
        ),
        concat!(
            "- id: g\n",
            "  config:\n",
            "    - id: new\n",
            "      name: beta\n",
        ),
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(entries[0].children.len(), 1, "旧列表必须被整体替换");
    assert_eq!(entries[0].children[0].id.as_deref(), Some("new"));
    assert!(
        entries[0].config.is_none(),
        "group 的配置已归一化进 children"
    );

    let dumped = cordis_host::patch::dump_entries(&entries).unwrap();
    let reparsed = parse_entries(&dumped).unwrap();
    assert_eq!(
        reparsed[0].children, entries[0].children,
        "保存后仍应是新列表"
    );
}

/// 显式 `null` 是"清空这个字段"，不能被当成"patch 里没写"。
#[test]
fn explicit_null_clears_config_and_overlay_fields() {
    let (entries, warnings) = run(
        concat!(
            "- id: a\n",
            "  name: alpha\n",
            "  config:\n",
            "    x: 1\n",
            "  inject: [\"tools\"]\n",
            "  isolate: { \"tools\": true }\n",
            "  intercept: { \"tools\": { \"k\": \"v\" } }\n",
        ),
        concat!(
            "- id: a\n",
            "  config: null\n",
            "  inject: null\n",
            "  isolate: null\n",
            "  intercept: null\n",
        ),
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(entries[0].config.is_none(), "config: null 必须清空原配置");
    assert!(entries[0].inject.is_none(), "inject: null 必须清空");
    assert!(entries[0].isolate.is_none(), "isolate: null 必须清空");
    assert!(entries[0].intercept.is_none(), "intercept: null 必须清空");
}

/// `name` 是对目标行身份的**断言**，不是改名。
///
/// 不匹配时 JS 的行为是"警告 + **跳过整条 patch**"（`patch.ts:97-100`）：
/// ```ts
/// if (name && name !== target.name) {
///   warn('patch: name mismatch for %C (expected %C, got %C), skipping', ...)
///   continue
/// }
/// ```
/// 既不是报错、更不是照常应用——后者会让一条本该落在 A 行的修改静默落到 B 行。
///
/// （修好之前：debug 构建里会 panic，release 构建里会静默应用。两种都不对。）
#[test]
fn name_mismatch_warns_and_skips_the_whole_patch() {
    let (entries, warnings) = run(
        "- id: a\n  name: alpha\n  config:\n    keep: 1\n",
        "- id: a\n  name: beta\n  disabled: true\n  config:\n    injected: 2\n",
    );
    assert_eq!(warnings.len(), 1, "应当给出警告：{warnings:?}");
    assert!(
        warnings[0].contains("name"),
        "警告应当点明是 name 不匹配：{warnings:?}"
    );

    // ★ 整条 patch 都不能落地：`disabled` 与 `config` 都不生效。
    assert!(
        !entries[0].disabled,
        "name 不匹配应当跳过整条 patch，而不是只跳过 name"
    );
    let config = entries[0].config.as_ref().unwrap();
    assert_eq!(config.get("keep").and_then(|v| v.as_i64()), Some(1));
    assert!(config.get("injected").is_none(), "config 也不该被应用");

    // `name` 只作断言，绝不被写进条目。
    assert_eq!(entries[0].name, "alpha");
}

/// `name` 匹配时照常应用——它是断言，不是障碍。
#[test]
fn name_match_applies_the_patch() {
    let (entries, warnings) = run(
        "- id: a\n  name: alpha\n",
        "- id: a\n  name: alpha\n  disabled: true\n",
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(entries[0].disabled);
    assert_eq!(entries[0].name, "alpha", "name 是断言，不该被覆盖");
}

/// `disabled` 的两种形态（静态布尔 / `!!js` 表达式）**互斥**，patch 覆盖时整体替换。
///
/// 修的是"只清一半"：静态 `false` 原先不清表达式，于是"显式启用"仍然被旧表达式
/// 禁掉（`Entry::disabled` 为假、`disabled_expr` 又求值为真 ⇒ 行永远不挂）。
#[test]
fn static_disabled_false_clears_a_previous_expression() {
    let (entries, warnings) = run(
        "- id: a\n  name: alpha\n  disabled: !!js gate.on\n",
        "- id: a\n  disabled: false\n",
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(!entries[0].disabled, "显式 false ⇒ 静态位为假");
    assert!(
        entries[0].disabled_expr.is_none(),
        "显式 false 必须清掉旧表达式，否则行仍会被它禁掉"
    );
}

#[test]
fn static_disabled_true_clears_a_previous_expression() {
    let (entries, warnings) = run(
        "- id: a\n  name: alpha\n  disabled: !!js gate.on\n",
        "- id: a\n  disabled: true\n",
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(entries[0].disabled);
    assert!(
        entries[0].disabled_expr.is_none(),
        "静态 true 同样让表达式作废"
    );
}

/// 反方向：表达式覆盖静态位（否则 `disabled: true` 会短路表达式求值）。
#[test]
fn expression_overlay_replaces_a_static_disable() {
    let (entries, warnings) = run(
        "- id: a\n  name: alpha\n  disabled: true\n",
        "- id: a\n  disabled: !!js gate.on\n",
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(!entries[0].disabled, "表达式形态不置静态位（两种形态互斥）");
    assert_eq!(
        entries[0]
            .disabled_expr
            .as_ref()
            .and_then(|expr| expr.expression().ok()),
        Some("gate.on".to_string()),
        "表达式必须整体接管这一位"
    );
}

/// Windows 编辑器常写 UTF-8 BOM；它不能把首行的 `-` 挤到第 2 列。
#[test]
fn utf8_bom_is_accepted_by_entries_and_patches() {
    let entries = parse_entries("\u{feff}- id: a\n  name: alpha\n").expect("带 BOM 的配置应能解析");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id.as_deref(), Some("a"));

    let patches =
        parse_patches("\u{feff}- id: a\n  disabled: true\n").expect("带 BOM 的 patch 应能解析");
    assert_eq!(patches.len(), 1);
}

/// 不能解析的条目必须报位置，而不是被 `filter_map` 静默丢掉。
#[test]
fn invalid_top_level_entry_is_rejected_with_position() {
    let error = parse_entries("- 42\n").expect_err("标量不是合法条目");
    assert!(error.contains("第 1 行"), "{error}");
    assert!(error.contains("映射"), "{error}");
}

/// 组的子条目同样不能静默丢弃。
#[test]
fn invalid_group_child_is_rejected_with_position() {
    let error = parse_entries("- id: g\n  name: group\n  group: true\n  config:\n    - 42\n")
        .expect_err("组里的标量不是合法子条目");
    assert!(error.contains("子条目"), "{error}");
    assert!(error.contains("映射"), "{error}");
}

/// patch 数组里的非法元素同样要报位置。
#[test]
fn invalid_patch_is_rejected_with_position() {
    let error = parse_patches("- 42\n").expect_err("标量不是合法 patch");
    assert!(error.contains("patch 第 1 条"), "{error}");
    assert!(error.contains("映射"), "{error}");
}

/// patch `insert` 里的非法条目同样要报位置。
#[test]
fn invalid_patch_insert_child_is_rejected_with_position() {
    let error = parse_patches("- insert:\n    - 42\n").expect_err("insert 里的标量不是合法条目");
    assert!(error.contains("insert 第 1 个条目"), "{error}");
    assert!(error.contains("映射"), "{error}");
}

/// `config: null` 是“清空配置”的显式指令，overlay 保存时必须保留。
#[test]
fn dump_overlay_preserves_explicit_null_config() {
    let null = Config::Null;
    let text = dump_overlay("x", Some(&null), None).expect("dump overlay");
    assert!(text.contains("config: null"), "{text}");

    let patches = parse_patches(&text).expect("overlay 必须能重新解析");
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0].config, Some(Config::Null));
}
