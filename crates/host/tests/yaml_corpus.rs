//! O12 YAML 语料回归套件：钉住 `cordis.yml` / `cordis.patch.yml` 的全部保留语义与边界。
//!
//! 覆盖项（对照 `docs/runtime-optimization-plan.md` O12）：
//! 1. `!!js` 标签（`disabled` 与嵌套 `config`）及 `dump_entries`/`dump_overlay` 往返；
//! 2. 引号内字面量（单引号 `''` 转义、双引号 `\"` 转义）与注释、词尾粘连 `note!!js`；
//! 3. 块标量（`|` / `>`）与非 `!!js` 本地标签（`!other 5` 不误判为 `JsExpr`）；
//! 4. UTF-8 BOM（`\u{feff}`）、显式 `null` 清空 vs 缺省字段保留；
//! 5. 嵌套 `group`、边插入边命中、`group: false` 清子项；
//! 6. 各类非法输入必须 fail loud。
//!
//! 本语料作为未来若从 `serde_yaml 0.9` 迁移到 `yaml_serde` / `serde-saphyr` 时的
//! **语义等价基线**；注意 `dump_entries` 是值级语义序列化，不承诺保留注释、键序或引号风格。

use cordis_host::patch::{
    apply_entry_patches, collect_js_expression_issues, dump_entries, dump_overlay, parse_entries,
    parse_patches, JsExpr,
};
use serde_json::json;

#[test]
fn corpus_js_tags_round_trip_in_disabled_and_nested_config() {
    let yaml = "\
- id: gated
  name: plugin-a
  disabled: !!js ctx.get('env') === 'prod'
  config:
    nested:
      flag: !!js 1 + 1 === 2
      items:
        - !!js 'hello'
        - plain
";
    let entries = parse_entries(yaml).expect("parse entries with !!js");
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert!(!entry.disabled, "disabled_expr 不得静默落成静态 disabled");
    assert_eq!(
        entry.disabled_expr.as_ref().unwrap().expression().unwrap(),
        "ctx.get('env') === 'prod'"
    );
    let issues = collect_js_expression_issues(&entries);
    assert_eq!(issues.len(), 1);
    assert_eq!(
        issues[0].1.len(),
        3,
        "应收集到 disabled + 2 处 config 表达式"
    );

    // 序列化再解析：值与 !!js 标签必须无损往返。
    let dumped = dump_entries(&entries).expect("dump entries");
    assert!(
        dumped.contains("!!js "),
        "回写必须还原标准 !!js 标签:\n{dumped}"
    );
    assert!(
        !dumped.contains("!cordis-js"),
        "回写不得残留内部标记:\n{dumped}"
    );
    let reparsed = parse_entries(&dumped).expect("reparse dumped entries");
    assert_eq!(entries, reparsed);
}

#[test]
fn corpus_quotes_comments_and_word_boundary_preserve_literal_js_text() {
    let yaml = "\
# !!js comment should be ignored
- id: q
  name: plugin-q
  config:
    single: '!!js not a tag ''still'' !!js'
    double: \"!!js not a tag \\\"still\\\" !!js\"
    hash_in_val: v#notcomment
    glued: note!!js hello
";
    let entries = parse_entries(yaml).expect("parse quoted/commented yaml");
    assert!(
        collect_js_expression_issues(&entries).is_empty(),
        "引号、注释与词尾粘连文本不得被误识别为 !!js 标签"
    );
    let cfg = entries[0].config.as_ref().unwrap();
    assert_eq!(cfg["single"], json!("!!js not a tag 'still' !!js"));
    assert_eq!(cfg["double"], json!("!!js not a tag \"still\" !!js"));
    assert_eq!(cfg["hash_in_val"], json!("v#notcomment"));
    assert_eq!(cfg["glued"], json!("note!!js hello"));

    let dumped = dump_entries(&entries).expect("dump");
    let reparsed = parse_entries(&dumped).expect("reparse");
    assert_eq!(entries, reparsed);
}

#[test]
fn corpus_block_scalars_and_other_local_tags() {
    let yaml = "\
- id: blk
  name: plugin-blk
  config:
    literal: |
      line 1
      line 2
    folded: >
      hello
      world
    custom_tag: !other 5
";
    let entries = parse_entries(yaml).expect("parse block scalars and !other tag");
    assert!(
        collect_js_expression_issues(&entries).is_empty(),
        "!other 标签不得误判为 !!js 表达式"
    );
    let cfg = entries[0].config.as_ref().unwrap();
    assert_eq!(cfg["literal"], json!("line 1\nline 2\n"));
    assert_eq!(cfg["folded"], json!("hello world\n"));
    assert!(JsExpr::from_value(&cfg["custom_tag"]).is_none());
}

#[test]
fn corpus_bom_null_clearing_and_nested_groups_with_patches() {
    let base_yaml = "\u{feff}\
- id: grp
  name: group-root
  group: true
  config:
    - id: child-a
      name: plugin-a
      config:
        k: 1
      inject:
        - svc-a
      isolate:
        svc-a: shared
";
    let patch_yaml = "\u{feff}\
- id: grp
  insert:
    - id: child-b
      name: plugin-b
      config:
        k: 2
- id: child-b
  config:
    k: 20
- id: child-a
  inject: null
  isolate: null
  disabled: true
";
    let root = parse_entries(base_yaml).expect("parse base with BOM");
    let patches = parse_patches(patch_yaml).expect("parse patches with BOM");
    let mut warnings = Vec::new();
    let merged = apply_entry_patches(root, &patches, &mut |w| warnings.push(w.to_string()));
    assert!(warnings.is_empty(), "合法 patch 不应告警: {warnings:?}");

    let grp = &merged[0];
    assert!(grp.group);
    assert_eq!(grp.children.len(), 2);

    let child_a = &grp.children[0];
    assert!(child_a.disabled);
    assert!(child_a.inject.is_none(), "显式 null 必须清空 inject");
    assert!(child_a.isolate.is_none(), "显式 null 必须清空 isolate");
    assert_eq!(
        child_a.config,
        Some(json!({ "k": 1 })),
        "未出现在 patch 中的 config 必须保留"
    );

    let child_b = &grp.children[1];
    assert_eq!(
        child_b.config,
        Some(json!({ "k": 20 })),
        "同层后续 patch 必须能命中前面刚插入的行"
    );

    // dump_overlay 往返
    let overlay_yaml =
        dump_overlay("child-b", Some(&json!({ "k": 99 })), Some(false)).expect("dump_overlay");
    let overlay_patches = parse_patches(&overlay_yaml).expect("parse dumped overlay");
    let merged2 = apply_entry_patches(merged, &overlay_patches, &mut |w| {
        warnings.push(w.to_string())
    });
    assert!(warnings.is_empty());
    assert_eq!(merged2[0].children[1].config, Some(json!({ "k": 99 })));
}

#[test]
fn corpus_invalid_inputs_fail_loudly() {
    // 1. disabled 为字符串 "true" 必须拒绝
    assert!(parse_entries("- name: a\n  disabled: \"true\"\n").is_err());
    // 2. inject 混入 required 与裸服务名必须拒绝
    assert!(parse_entries("- name: a\n  inject:\n    required: [x]\n    other: {}\n").is_err());
    // 3. isolate 值为 false 必须拒绝
    assert!(parse_entries("- name: a\n  isolate:\n    svc: false\n").is_err());
    // 4. entry-level inject / intercept 中的 !!js 必须拒绝（主配置与 patch 均拒绝）
    assert!(parse_entries("- name: a\n  inject:\n    svc: !!js 1\n").is_err());
    assert!(parse_patches("- id: a\n  intercept:\n    svc: !!js 1\n").is_err());
    // 5. group 的 config 非数组必须拒绝
    assert!(parse_entries("- name: g\n  group: true\n  config:\n    not: array\n").is_err());
}
