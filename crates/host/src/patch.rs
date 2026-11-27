//! `cordis.yml` / `cordis.patch.yml` 的兼容层。
//!
//! 这一层是**便宜且高价值**的兼容性：纯函数、无 IO、语义可以逐行对照
//! `vendor/include/src/index.ts:57-127`。现有 cordis 配置文件可以原样喂进来。
//!
//! # 与 JS 的唯一表示差异
//! JS 里 group 行的子条目**藏在 `config` 数组里**（`config` 同时是"配置"和"子节点列表"
//! 两个含义）。Rust 里把它归一化成真正的 `children` 字段：加载时从 `config` 提升出来，
//! 保存时再降回去。于是 patch 算法不需要任何嵌套可变引用，从 80 行降到 30 行。
//!
//! # 必须照抄的四条语义
//! 1. `insert` **不带 `id`** → 追加到根；**带 `id`** → 目标必须是 `group: true` 的行，
//!    追加到它的子条目末尾。
//! 2. 非 `insert` 的 patch **必须带 `id`**，且 `config` 是**整体替换，绝不深合并**。
//! 3. 匹配不到目标 → 警告并跳过（不是报错）。
//! 4. **边插入边建索引**：同一层里后面的 patch 能命中前面刚插入的行
//!    （这是 harness 对上游的修改之一，`vendor/README.md` #11）——
//!    只要每次都从根重新查找，这条语义就天然成立。
//!
//! # `!!js` 表达式（M1-4）
//! cordis 配置里 `disabled: !!js ctx.get('profileContext')` 是**标准写法**。
//! `!!js` 是 YAML 标准标签，而 serde_yaml 0.9 只把单 `!` 本地标签暴露成 tagged
//! value——**`!!js` 会被直接丢弃**，于是 `disabled` 悄悄变 `false`、配置值悄悄变裸值，
//! 行照常挂载。这是最难查的一类静默降级。
//!
//! 本层的处理是"识别 + fail loud"（求值器属 M4-4）：
//! - 解析前把 `!!js` 文本改写成本地标签 `!cordis-js`（**引号与注释之外的**才改）；
//! - 识别成 [`JsExpr`]，`disabled` 位置进 `Entry::disabled_expr`，其余位置留在
//!   `config` 里（标记形态 `{"!cordis-js": <主体>}`）；
//! - [`js_expression_issues`] 把它们逐条变成"暂不支持求值"错误，挂载点 fail loud
//!   （M4-2 起 `disabled` 位置在装载器里由注入的求值器现算，见
//!   [`crate::loader::JsEvaluator`]；`config` 位置仍归本清单，M4-4b 才接配置链）；
//! - 回写时构造 `serde_yaml::Value::Tagged` 再对称还原成 `!!js` 原文。

use crate::RowStartup;
use cordis_core::Config;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

/// 归一化后的行级 `inject` 声明。
///
/// 三种线上形状都收敛到这里：
/// - `["svc"]`：必需依赖；
/// - `{ "svc": <config> }`：带 intercept 配置的必需依赖；
/// - `{ "required": [...], "optional": [...] }`：必需/可选依赖。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedInject {
    pub required: Vec<(String, Config)>,
    pub optional: Vec<String>,
    /// 平铺对象形态显式给了每个依赖的配置，应覆盖插件自带配置；
    /// 数组/`{required, optional}` 只声明名字，不覆盖。
    pub merge_config: bool,
}

/// 解析行级 `inject`；非法形状报错而不是静默把 `required`/`optional` 当服务名。
///
/// 公开给 `cordis-cli inspect` 复用：装载前的能力审阅与装载时的解析必须
/// 是**同一份**实现，否则 `required`/`optional` 又会被当成两个服务名。
pub fn parse_inject(value: &Value) -> std::result::Result<ParsedInject, String> {
    match value {
        Value::Array(list) => {
            let names = parse_inject_names(list, "inject")?;
            Ok(ParsedInject {
                required: names.into_iter().map(|name| (name, Value::Null)).collect(),
                optional: Vec::new(),
                merge_config: false,
            })
        }
        Value::Object(map) if map.contains_key("required") || map.contains_key("optional") => {
            if let Some(other) = map
                .keys()
                .find(|key| key.as_str() != "required" && key.as_str() != "optional")
            {
                return Err(format!(
                    "inject 的 {{required, optional}} 形状不能混入服务名 \"{other}\""
                ));
            }
            let required: Vec<(String, Config)> =
                parse_optional_names(map.get("required"), "inject.required")?
                    .into_iter()
                    .map(|name| (name, Value::Null))
                    .collect();
            let required_set: HashSet<&str> = required
                .iter()
                .map(|(name, _): &(String, Config)| name.as_str())
                .collect();
            let optional = parse_optional_names(map.get("optional"), "inject.optional")?
                .into_iter()
                .filter(|name| !required_set.contains(name.as_str()))
                .collect();
            Ok(ParsedInject {
                required: dedup_required(required),
                optional,
                merge_config: false,
            })
        }
        Value::Object(map) => {
            let mut required = Vec::new();
            for (name, config) in map {
                if name.is_empty() {
                    return Err("inject 的服务名不能为空".to_string());
                }
                required.push((name.clone(), config.clone()));
            }
            Ok(ParsedInject {
                required: dedup_required(required),
                optional: Vec::new(),
                merge_config: true,
            })
        }
        other => Err(format!(
            "inject 必须是数组或映射（收到{}）",
            json_kind(other)
        )),
    }
}

fn parse_optional_names(
    value: Option<&Value>,
    path: &str,
) -> std::result::Result<Vec<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(list)) => parse_inject_names(list, path),
        Some(other) => Err(format!(
            "{path} 必须是字符串数组（收到{}）",
            json_kind(other)
        )),
    }
}

fn parse_inject_names(list: &[Value], path: &str) -> std::result::Result<Vec<String>, String> {
    let mut names = Vec::new();
    for (index, item) in list.iter().enumerate() {
        let name = item
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                format!(
                    "{path}[{index}] 必须是非空字符串（收到{}）",
                    json_kind(item)
                )
            })?;
        if !names.iter().any(|seen| seen == name) {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

fn dedup_required(required: Vec<(String, Config)>) -> Vec<(String, Config)> {
    let mut seen = HashSet::new();
    required
        .into_iter()
        .filter(|(name, _)| seen.insert(name.clone()))
        .collect()
}

/// `disabled` 的合法形态：静态布尔（含显式 null = 不写）或 `!!js` 表达式。
///
/// 字符串 `"true"` 这类"看起来像布尔"的值必须报错：静默落成 `false`
/// 会让平台门控行在错误平台上照常挂载。
fn parse_disabled(
    value: Option<&Value>,
) -> std::result::Result<(Option<bool>, Option<JsExpr>), String> {
    let Some(value) = value else {
        return Ok((None, None));
    };
    if let Some(expr) = JsExpr::from_value(value) {
        return Ok((None, Some(expr)));
    }
    match value {
        Value::Bool(flag) => Ok((Some(*flag), None)),
        Value::Null => Ok((None, None)),
        other => Err(format!(
            "disabled 必须是布尔值或 !!js 表达式（收到{}）",
            json_kind(other)
        )),
    }
}

/// 解析行级启动策略（`startup` / `required` / `optional`）。
///
/// 返回值：
/// - `Ok(None)`：三个键均未出现（条目默认继承/Required，patch 不修改该字段）；
/// - `Ok(Some(None))`：显式设为 `null`（条目视为未声明，patch 清除显式覆盖）；
/// - `Ok(Some(Some(RowStartup)))`：显式声明为 `Required` 或 `Optional`。
///
/// 类型非法、含 `!!js` 或同条目内互相冲突（例如同时 `required: true` 与 `optional: true`）
/// 一律 fail loud 返回可读错误。
fn parse_row_startup(
    object: &Map<String, Value>,
) -> std::result::Result<Option<Option<RowStartup>>, String> {
    let mut seen_any = false;
    let mut resolved: Option<RowStartup> = None;

    let mut merge =
        |field: &str, candidate: Option<RowStartup>| -> std::result::Result<(), String> {
            seen_any = true;
            if let Some(next) = candidate {
                if let Some(prev) = resolved {
                    if prev != next {
                        return Err(format!(
                            "{field} 与同行其它启动策略声明冲突（已声明 {}，又声明 {}）",
                            prev.as_str(),
                            next.as_str()
                        ));
                    }
                }
                resolved = Some(next);
            }
            Ok(())
        };

    if let Some(value) = object.get("startup") {
        if let Some((path, expr)) = first_js_expr(value, "startup") {
            return Err(format!(
                "entry-level startup 的 !!js 尚未支持（at {path}: {expr}）"
            ));
        }
        match value {
            Value::Null => merge("startup", None)?,
            Value::String(text) => match text.as_str() {
                "required" => merge("startup", Some(RowStartup::Required))?,
                "optional" => merge("startup", Some(RowStartup::Optional))?,
                other => {
                    return Err(format!(
                        "startup 必须是 \"required\" 或 \"optional\"（收到 {other:?}）"
                    ));
                }
            },
            other => {
                return Err(format!(
                    "startup 必须是字符串 \"required\" 或 \"optional\"（收到{}）",
                    json_kind(other)
                ));
            }
        }
    }

    if let Some(value) = object.get("required") {
        if let Some((path, expr)) = first_js_expr(value, "required") {
            return Err(format!(
                "entry-level required 的 !!js 尚未支持（at {path}: {expr}）"
            ));
        }
        match value {
            Value::Null => merge("required", None)?,
            Value::Bool(flag) => merge(
                "required",
                Some(if *flag {
                    RowStartup::Required
                } else {
                    RowStartup::Optional
                }),
            )?,
            other => {
                return Err(format!("required 必须是布尔值（收到{}）", json_kind(other)));
            }
        }
    }

    if let Some(value) = object.get("optional") {
        if let Some((path, expr)) = first_js_expr(value, "optional") {
            return Err(format!(
                "entry-level optional 的 !!js 尚未支持（at {path}: {expr}）"
            ));
        }
        match value {
            Value::Null => merge("optional", None)?,
            Value::Bool(flag) => merge(
                "optional",
                Some(if *flag {
                    RowStartup::Optional
                } else {
                    RowStartup::Required
                }),
            )?,
            other => {
                return Err(format!("optional 必须是布尔值（收到{}）", json_kind(other)));
            }
        }
    }

    if seen_any {
        Ok(Some(resolved))
    } else {
        Ok(None)
    }
}

/// 与 `Vec<Entry>` 一一对应的行级启动策略树（不向 0.1 公开 DTO `Entry` 新增字段，见 ADR 0006）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StartupNode {
    pub explicit: Option<RowStartup>,
    pub children: Vec<StartupNode>,
}

/// 与 `Patch` 并行的启动策略变更元数据。
#[derive(Clone, Debug)]
pub(crate) struct PatchWithStartup {
    pub patch: Patch,
    pub startup: Option<Option<RowStartup>>,
    pub insert_startup: Option<Vec<StartupNode>>,
}

/// `isolate` 的合法形态：`{ "<service>": true | "<label>" }`。
///
/// 值不是 `true`/字符串时 loader 现在会静默 `continue`（完全不应用隔离）；
/// 解析期直接拒绝，错误里带上具体服务名。
fn validate_isolate(value: &Value) -> std::result::Result<(), String> {
    let map = value.as_object().ok_or_else(|| {
        format!(
            "isolate 必须是映射（服务名 → true|标签字符串，收到{}）",
            json_kind(value)
        )
    })?;
    for (name, spec) in map {
        if name.is_empty() {
            return Err("isolate 的服务名不能为空".to_string());
        }
        match spec {
            Value::Bool(true) => {}
            Value::String(label) if !label.is_empty() => {}
            other => {
                return Err(format!(
                    "isolate.{name} 必须是 true 或非空字符串标签（收到{}）",
                    if *other == Value::Bool(false) {
                        "布尔值 false".to_string()
                    } else {
                        json_kind(other).to_string()
                    }
                ));
            }
        }
    }
    Ok(())
}

/// 一行插件配置（归一化后）。字段与 `vendor/loader/src/config/entry.ts:10-23` 对齐。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Entry {
    pub id: Option<String>,
    pub name: String,
    /// 非 group 行：插件自己的配置；group 行：忽略（子条目在 `children` 里）。
    pub config: Option<Config>,
    pub group: bool,
    pub disabled: bool,
    /// `disabled: !!js <表达式>` 的表达式本体（静态 `disabled` 为布尔值，不走这里）。
    ///
    /// 与 `disabled` 并存而不是合一：`disabled` 保持 `bool` 让既有代码零改动，
    /// 表达式单独留痕，**绝不静默降级**。
    pub disabled_expr: Option<JsExpr>,
    pub inject: Option<Config>,
    /// `{ "<service>": true | "<label>" }`：`true` = 行内私有域，字符串 = 具名共享域。
    pub isolate: Option<Config>,
    pub intercept: Option<Config>,
    /// 仅 group 行非空。
    pub children: Vec<Entry>,
}

impl Entry {
    /// 从磁盘形状（YAML/JSON 的映射）构造。
    ///
    /// 兼容入口：非法输入返回 `None`。解析磁盘配置请走
    /// [`Entry::from_value_checked`]，它会给出可定位的错误。
    pub fn from_value(value: &Value) -> Option<Entry> {
        Self::from_value_checked(value).ok()
    }

    /// 从磁盘形状构造；非法条目不静默丢弃，返回带原因的 `Err`。
    pub fn from_value_checked(value: &Value) -> std::result::Result<Entry, String> {
        let object = value
            .as_object()
            .ok_or_else(|| format!("条目必须是映射（收到{}）", json_kind(value)))?;
        let group = object
            .get("group")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let raw_config = object.get("config").cloned().filter(|v| !v.is_null());
        let children = if group {
            // group 行的 `config` 就是子条目数组；非法子条目必须报错，不能丢。
            match object.get("config") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(list)) => parse_entry_children(list)?,
                Some(other) => {
                    return Err(format!(
                        "group 的 config 必须是子条目数组（收到{}）",
                        json_kind(other)
                    ));
                }
            }
        } else {
            Vec::new()
        };
        // `disabled` 两种形态：静态布尔，或 `!!js` 表达式。表达式**不**当布尔用——
        // `as_bool` 会给 `false`，那正是要消灭的静默降级。
        let (disabled, disabled_expr) =
            parse_disabled(object.get("disabled")).map_err(|err| format!("{err} (at disabled)"))?;
        let disabled = disabled.unwrap_or(false);
        let inject = object.get("inject").cloned().filter(|v| !v.is_null());
        let isolate = object.get("isolate").cloned().filter(|v| !v.is_null());
        if let Some(value) = &isolate {
            validate_isolate(value).map_err(|err| format!("{err} (at isolate)"))?;
        }
        let intercept = object.get("intercept").cloned().filter(|v| !v.is_null());
        // entry-level `inject` 的 `!!js` 仍不参与求值；静默把原始标记装进
        // inject/intercept 会让插件拿到一个永远不成立的对象，比直接报错更危险。
        for (field, value) in [("inject", &inject), ("intercept", &intercept)] {
            if let Some((path, expr)) = value.as_ref().and_then(|v| first_js_expr(v, field)) {
                return Err(format!(
                    "entry-level {field} 的 !!js 尚未支持（at {path}: {expr}）；\
                     目前只有 config 与 disabled 支持求值"
                ));
            }
        }
        // 形状校验放在解析期：`required`/`optional` 绝不能被当成两个服务名装进 fiber。
        if let Some(value) = &inject {
            parse_inject(value).map_err(|err| format!("{err} (at inject)"))?;
        }
        // 校验行级启动策略（`startup` / `required` / `optional`）：非法或冲突一律 fail loud。
        parse_row_startup(object)?;
        Ok(Entry {
            id: object
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            name: object
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            config: if group { None } else { raw_config },
            group,
            disabled,
            disabled_expr,
            inject,
            isolate,
            intercept,
            children,
        })
    }

    /// 回到磁盘形状。
    pub fn to_value(&self) -> Value {
        let mut object = Map::new();
        if let Some(id) = &self.id {
            object.insert("id".into(), Value::String(id.clone()));
        }
        object.insert("name".into(), Value::String(self.name.clone()));
        if self.group {
            object.insert("group".into(), Value::Bool(true));
            object.insert(
                "config".into(),
                Value::Array(self.children.iter().map(Entry::to_value).collect()),
            );
        } else if let Some(config) = &self.config {
            if !config.is_null() {
                object.insert("config".into(), config.clone());
            }
        }
        if let Some(expr) = &self.disabled_expr {
            // 表达式形态：dump 时经 `yaml_with_tags` + `restore_js_tags` 还原成 `!!js <原文>`。
            object.insert("disabled".into(), expr.to_value());
        } else if self.disabled {
            object.insert("disabled".into(), Value::Bool(true));
        }
        for (key, value) in [
            ("inject", &self.inject),
            ("isolate", &self.isolate),
            ("intercept", &self.intercept),
        ] {
            if let Some(value) = value {
                object.insert(key.into(), value.clone());
            }
        }
        Value::Object(object)
    }

    fn overlay(&mut self, patch: &Patch, warn: &mut dyn FnMut(&str)) {
        // `patch.name` 在这里**故意不处理**：它是对目标行身份的断言，
        // 不匹配时整条 patch 都要跳过（`patch.ts:97-100`）。
        // 那个判断属于调用方 `apply_entry_patches`——只有它手里有 `warn`，
        // 也只有它能"什么都不做"；`overlay` 的本事只有"改"。
        //
        // group 行在磁盘上的 `config` 就是子条目数组，归一化后落在 `children`：
        // 补丁整体替换 `config` 时，必须把新数组同步成新的子条目列表，否则
        // 后续装载与保存都还会看见旧列表。
        if let Some(group) = patch.group {
            self.group = group;
            if !group {
                self.children.clear();
            }
        }
        if let Some(config) = &patch.config {
            if self.group {
                self.config = None;
                if config.is_null() {
                    self.children.clear();
                } else if let Some(list) = config.as_array() {
                    let mut children = Vec::with_capacity(list.len());
                    for (index, item) in list.iter().enumerate() {
                        match Entry::from_value_checked(item) {
                            Ok(child) => children.push(child),
                            Err(err) => warn(&format!(
                                "patch: group \"{}\" 的第 {} 个子条目非法（已跳过）: {err}",
                                entry_label(self),
                                index + 1
                            )),
                        }
                    }
                    self.children = children;
                } else {
                    warn(&format!(
                        "patch: group \"{}\" 的 config 必须是子条目数组（收到{}），已忽略",
                        entry_label(self),
                        json_kind(config)
                    ));
                }
            } else if config.is_null() {
                self.config = None;
            } else {
                self.config = Some(config.clone());
            }
        }
        if let Some(expr) = &patch.disabled_expr {
            self.disabled_expr = Some(expr.clone());
            // 两种形态互斥：表达式整体接管这一位，旧的静态 `disabled: true` 必须让位
            // （否则表达式永远轮不到求值）。
            self.disabled = false;
        }
        if let Some(disabled) = patch.disabled {
            self.disabled = disabled;
            // 静态布尔覆盖后表达式整体作废。**`false` 也要清**：
            // 否则"显式启用"会继续被旧表达式禁掉（与 `set_disabled` 的语义一致）。
            self.disabled_expr = None;
        }
        // 显式 `null` 是"清空字段"；`None`（字段没出现）才是不动。
        if let Some(inject) = &patch.inject {
            self.inject = (!inject.is_null()).then(|| inject.clone());
        }
        if let Some(isolate) = &patch.isolate {
            self.isolate = (!isolate.is_null()).then(|| isolate.clone());
        }
        if let Some(intercept) = &patch.intercept {
            self.intercept = (!intercept.is_null()).then(|| intercept.clone());
        }
    }
}

/// 一条 patch 操作。
///
/// 全部用 `Option`：必须能区分"字段不存在"（不动）与"显式 null"（清空）。
#[derive(Debug, Clone, Default)]
pub struct Patch {
    pub id: Option<String>,
    pub insert: Option<Vec<Entry>>,
    pub name: Option<String>,
    pub config: Option<Config>,
    pub group: Option<bool>,
    pub disabled: Option<bool>,
    /// `disabled: !!js <表达式>`（patch 同样支持表达式形态）。
    pub disabled_expr: Option<JsExpr>,
    pub inject: Option<Config>,
    pub isolate: Option<Config>,
    pub intercept: Option<Config>,
}

impl Patch {
    /// 兼容入口：非法输入返回 `None`。解析磁盘 patch 请走
    /// [`Patch::from_value_checked`]，它会给出可定位的错误。
    pub fn from_value(value: &Value) -> Option<Patch> {
        Self::from_value_checked(value).ok()
    }

    /// 从磁盘形状构造；非法 patch 不静默丢弃。
    pub fn from_value_checked(value: &Value) -> std::result::Result<Patch, String> {
        let object = value
            .as_object()
            .ok_or_else(|| format!("patch 必须是映射（收到{}）", json_kind(value)))?;
        let insert = match object.get("insert") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let list = value
                    .as_array()
                    .ok_or_else(|| format!("insert 必须是数组（收到{}）", json_kind(value)))?;
                let mut entries = Vec::with_capacity(list.len());
                for (index, item) in list.iter().enumerate() {
                    let entry = Entry::from_value_checked(item).map_err(|err| {
                        format!("insert 第 {} 个条目不是合法条目: {err}", index + 1)
                    })?;
                    entries.push(entry);
                }
                Some(entries)
            }
        };
        let (disabled, disabled_expr) =
            parse_disabled(object.get("disabled")).map_err(|err| format!("{err} (at disabled)"))?;
        let inject = object.get("inject").cloned();
        let intercept = object.get("intercept").cloned();
        for (field, value) in [("inject", &inject), ("intercept", &intercept)] {
            if let Some((path, expr)) = value
                .as_ref()
                .filter(|v| !v.is_null())
                .and_then(|v| first_js_expr(v, field))
            {
                return Err(format!(
                    "entry-level {field} 的 !!js 尚未支持（at {path}: {expr}）；\
                     目前只有 config 与 disabled 支持求值"
                ));
            }
        }
        if let Some(value) = inject.as_ref().filter(|value| !value.is_null()) {
            parse_inject(value).map_err(|err| format!("{err} (at inject)"))?;
        }
        let isolate = object.get("isolate").cloned();
        if let Some(value) = isolate.as_ref().filter(|value| !value.is_null()) {
            validate_isolate(value).map_err(|err| format!("{err} (at isolate)"))?;
        }
        parse_row_startup(object)?;
        Ok(Patch {
            id: object
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            insert,
            name: object
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            // 显式 null 必须保留：外层 `None` 表示"没提供这个字段"，
            // 内层 `Null` 表示"把它清空"。
            config: object.get("config").cloned(),
            group: object.get("group").and_then(|v| v.as_bool()),
            disabled,
            disabled_expr,
            inject,
            isolate,
            intercept,
        })
    }
}

/// 某一行的 `config` / `disabled` 最后一次被哪一层 patch 覆盖。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FieldOwners {
    pub config: Option<usize>,
    pub disabled: Option<usize>,
}

/// patch 层 → 字段归属表。
///
/// 行标（label）由结构决定，但运行期回写需要知道“这个字段最后被文件里的哪一层
/// 改过”。只看主配置/合并树会把主配置行写回主配置，随后又被 patch 覆盖——
/// 用户看到的是“运行期改了，reload 又变回去”。
#[derive(Clone, Debug, Default)]
pub(crate) struct PatchOwners {
    by_id: HashMap<String, FieldOwners>,
}

impl PatchOwners {
    pub(crate) fn config_layer(&self, id: &str) -> Option<usize> {
        self.by_id.get(id).and_then(|owners| owners.config)
    }

    pub(crate) fn disabled_layer(&self, id: &str) -> Option<usize> {
        self.by_id.get(id).and_then(|owners| owners.disabled)
    }

    fn note(&mut self, entry: &Entry, layer: usize) {
        if let Some(id) = entry.id.as_deref() {
            let owners = self.by_id.entry(id.to_string()).or_default();
            if entry.config.is_some() {
                owners.config = Some(layer);
            }
            if entry.disabled || entry.disabled_expr.is_some() {
                owners.disabled = Some(layer);
            }
        }
        for child in &entry.children {
            self.note(child, layer);
        }
    }

    fn note_overlay(
        &mut self,
        entry: &Entry,
        patch: &Patch,
        layer: usize,
        children_replaced: bool,
    ) {
        let Some(id) = entry.id.as_deref() else {
            return;
        };
        let owners = self.by_id.entry(id.to_string()).or_default();
        if patch.config.is_some() {
            owners.config = Some(layer);
        }
        if patch.disabled.is_some() || patch.disabled_expr.is_some() {
            owners.disabled = Some(layer);
        }
        // 只有 group 的 config 真正替换了 children 时，才将新子项归属记为当前层。
        if children_replaced {
            for child in &entry.children {
                self.note(child, layer);
            }
        }
    }

    fn untrack(&mut self, entry: &Entry) {
        if let Some(id) = entry.id.as_deref() {
            self.by_id.remove(id);
        }
        for child in &entry.children {
            self.untrack(child);
        }
    }
}

/// 应用一层 patch，返回新的条目树。
///
/// 这是公共入口的兼容包装；需要字段归属时用 [`apply_entry_patches_tracked`]。
pub fn apply_entry_patches(
    root: Vec<Entry>,
    patches: &[Patch],
    warn: &mut dyn FnMut(&str),
) -> Vec<Entry> {
    let mut owners = PatchOwners::default();
    apply_entry_patches_tracked(root, patches, 0, &mut owners, warn)
}

/// 应用一层 patch，并把 `config` / `disabled` 的最后覆盖层记进 `owners`。
pub(crate) fn apply_entry_patches_tracked(
    mut root: Vec<Entry>,
    patches: &[Patch],
    layer: usize,
    owners: &mut PatchOwners,
    warn: &mut dyn FnMut(&str),
) -> Vec<Entry> {
    for patch in patches {
        if let Some(insert) = &patch.insert {
            match patch.id.as_deref() {
                // `insert` 不带 id → 追加到根。
                None => {
                    root.extend(insert.iter().cloned());
                    for entry in insert {
                        owners.note(entry, layer);
                    }
                }
                // `insert` 带 id → 目标必须是 group 行，追加到它的子条目末尾。
                Some(group_id) => match find_mut(&mut root, group_id) {
                    Some(target) if target.group => {
                        target.children.extend(insert.iter().cloned());
                        for entry in insert {
                            owners.note(entry, layer);
                        }
                    }
                    Some(_) => warn(&format!("insert 目标 \"{group_id}\" 不是 group 行，已跳过")),
                    None => warn(&format!("insert 目标 \"{group_id}\" 未找到，已跳过")),
                },
            }
            continue;
        }
        let Some(id) = patch.id.as_deref() else {
            warn("非 insert 的 patch 必须带 id，已跳过");
            continue;
        };
        match find_mut(&mut root, id) {
            Some(target) => {
                // `name` 是对目标行身份的**断言**，不是改名（`patch.ts:97-100`）：
                // 不匹配就**跳过整条 patch** 并警告。
                // 只跳过 `name` 而照常应用其余字段是最坏的选择——
                // 一条本该落在 A 行的配置会静默落到 B 行。
                if let Some(name) = patch.name.as_deref().filter(|name| !name.is_empty()) {
                    if name != target.name {
                        warn(&format!(
                            "patch: name mismatch for \"{id}\" (expected \"{}\", got \"{name}\")，已跳过",
                            target.name
                        ));
                        continue;
                    }
                }
                // group 被关闭（group: false）或其 config 被 null/array 替换时，旧子树归属失效。
                let next_group = patch.group.unwrap_or(target.group);
                let clears_children = patch.group == Some(false)
                    || (next_group
                        && patch
                            .config
                            .as_ref()
                            .is_some_and(|c| c.is_null() || c.is_array()));
                let children_replaced =
                    next_group && patch.config.as_ref().is_some_and(|c| c.is_array());
                if clears_children {
                    for child in &target.children {
                        owners.untrack(child);
                    }
                }
                target.overlay(patch, warn);
                owners.note_overlay(target, patch, layer, children_replaced);
            }
            None => warn(&format!("patch 目标 \"{id}\" 未找到，已跳过")),
        }
    }
    root
}

/// 按 id 深度优先查找。因为子条目已经是真字段，这里就是普通的递归。
fn find_mut<'a>(entries: &'a mut [Entry], id: &str) -> Option<&'a mut Entry> {
    for entry in entries.iter_mut() {
        if entry.id.as_deref() == Some(id) {
            return Some(entry);
        }
        if let Some(found) = find_mut(&mut entry.children, id) {
            return Some(found);
        }
    }
    None
}

fn find_mut_with_startup<'a>(
    entries: &'a mut [Entry],
    nodes: &'a mut [StartupNode],
    id: &str,
) -> Option<(&'a mut Entry, &'a mut StartupNode)> {
    for (entry, node) in entries.iter_mut().zip(nodes.iter_mut()) {
        if entry.id.as_deref() == Some(id) {
            return Some((entry, node));
        }
        if let Some(found) = find_mut_with_startup(&mut entry.children, &mut node.children, id) {
            return Some(found);
        }
    }
    None
}

fn entry_from_value_with_startup(
    value: &Value,
) -> std::result::Result<(Entry, StartupNode), String> {
    let entry = Entry::from_value_checked(value)?;
    let object = value
        .as_object()
        .expect("Entry::from_value_checked 已校验对象");
    let explicit = parse_row_startup(object)?.flatten();
    let children = if entry.group {
        match object.get("config") {
            Some(Value::Array(list)) => {
                let mut child_nodes = Vec::with_capacity(list.len());
                for item in list {
                    let (_, node) = entry_from_value_with_startup(item)?;
                    child_nodes.push(node);
                }
                child_nodes
            }
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    Ok((entry, StartupNode { explicit, children }))
}

/// 应用一层 patch（同步维护行级启动策略树与字段归属）。
pub(crate) fn apply_entry_patches_full(
    mut root: Vec<Entry>,
    mut root_startup: Vec<StartupNode>,
    patches: &[PatchWithStartup],
    layer: usize,
    owners: &mut PatchOwners,
    warn: &mut dyn FnMut(&str),
) -> (Vec<Entry>, Vec<StartupNode>) {
    for item in patches {
        let patch = &item.patch;
        if let Some(insert) = &patch.insert {
            let insert_nodes = item
                .insert_startup
                .clone()
                .unwrap_or_else(|| vec![StartupNode::default(); insert.len()]);
            match patch.id.as_deref() {
                None => {
                    root.extend(insert.iter().cloned());
                    root_startup.extend(insert_nodes);
                    for entry in insert {
                        owners.note(entry, layer);
                    }
                }
                Some(group_id) => {
                    match find_mut_with_startup(&mut root, &mut root_startup, group_id) {
                        Some((target, target_node)) if target.group => {
                            target.children.extend(insert.iter().cloned());
                            target_node.children.extend(insert_nodes);
                            for entry in insert {
                                owners.note(entry, layer);
                            }
                        }
                        Some(_) => {
                            warn(&format!("insert 目标 \"{group_id}\" 不是 group 行，已跳过"))
                        }
                        None => warn(&format!("insert 目标 \"{group_id}\" 未找到，已跳过")),
                    }
                }
            }
            continue;
        }
        let Some(id) = patch.id.as_deref() else {
            warn("非 insert 的 patch 必须带 id，已跳过");
            continue;
        };
        match find_mut_with_startup(&mut root, &mut root_startup, id) {
            Some((target, target_node)) => {
                if let Some(name) = patch.name.as_deref().filter(|name| !name.is_empty()) {
                    if name != target.name {
                        warn(&format!(
                            "patch: name mismatch for \"{id}\" (expected \"{}\", got \"{name}\")，已跳过",
                            target.name
                        ));
                        continue;
                    }
                }
                let prev_group = target.group;
                let next_group = patch.group.unwrap_or(prev_group);
                let clears_children = patch.group == Some(false)
                    || (next_group
                        && patch
                            .config
                            .as_ref()
                            .is_some_and(|c| c.is_null() || c.is_array()));
                let children_replaced =
                    next_group && patch.config.as_ref().is_some_and(|c| c.is_array());
                if clears_children {
                    for child in &target.children {
                        owners.untrack(child);
                    }
                }
                target.overlay(patch, warn);
                if let Some(group) = patch.group {
                    if !group {
                        target_node.children.clear();
                    }
                }
                if let Some(config) = &patch.config {
                    if target.group {
                        if config.is_null() {
                            target_node.children.clear();
                        } else if let Some(list) = config.as_array() {
                            let mut child_nodes = Vec::with_capacity(list.len());
                            for item_val in list {
                                if let Ok((_, child_node)) = entry_from_value_with_startup(item_val)
                                {
                                    child_nodes.push(child_node);
                                }
                            }
                            target_node.children = child_nodes;
                        } else if !prev_group {
                            target_node.children.clear();
                        }
                    }
                }
                if let Some(startup_override) = item.startup {
                    target_node.explicit = startup_override;
                }
                owners.note_overlay(target, patch, layer, children_replaced);
            }
            None => warn(&format!("patch 目标 \"{id}\" 未找到，已跳过")),
        }
    }
    (root, root_startup)
}

/// 按与装载器完全一致的 DFS 行标与首行获胜规则把 `StartupNode` 树展平成 `label -> RowStartup`（含组行继承）。
pub(crate) fn flatten_startup_table(
    entries: &[Entry],
    nodes: &[StartupNode],
) -> HashMap<String, RowStartup> {
    fn walk(
        entries: &[Entry],
        nodes: &[StartupNode],
        inherited: Option<RowStartup>,
        ordinals: &mut HashMap<String, usize>,
        seen: &mut std::collections::HashSet<String>,
        out: &mut HashMap<String, RowStartup>,
    ) {
        for (index, entry) in entries.iter().enumerate() {
            let label = if let Some(id) = &entry.id {
                id.clone()
            } else {
                let counter = ordinals.entry(entry.name.clone()).or_insert(0);
                let label = format!("{}#{}", entry.name, *counter);
                *counter += 1;
                label
            };
            // 与 `DesiredWalk` 保持同一条“第一行获胜”不变量：重复 label 行绝不覆盖或污染首行的启动策略。
            if !seen.insert(label.clone()) {
                continue;
            }
            let node = nodes.get(index);
            let effective = node.and_then(|n| n.explicit).or(inherited);
            if entry.group {
                let child_nodes = node.map(|n| n.children.as_slice()).unwrap_or(&[]);
                walk(&entry.children, child_nodes, effective, ordinals, seen, out);
            } else if let Some(startup) = effective {
                out.insert(label, startup);
            }
        }
    }
    let mut ordinals = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    let mut out = HashMap::new();
    walk(entries, nodes, None, &mut ordinals, &mut seen, &mut out);
    out
}

/// 内部标记标签：`!!js` 是 YAML 标准标签，serde_yaml 0.9 只把单 `!` 本地标签暴露成
/// tagged value，所以解析前把文本里的 `!!js` 改写成本标签，回写时再对称还原。
const JS_TAG: &str = "!cordis-js";

/// 在任意配置子树里找第一个尚未求值的 `!!js`，返回 `(路径, 原文形态)`。
fn first_js_expr(value: &Value, path: &str) -> Option<(String, String)> {
    if let Some(expr) = JsExpr::from_value(value) {
        return Some((path.to_string(), expr.to_value().to_string()));
    }
    match value {
        Value::Array(list) => list
            .iter()
            .enumerate()
            .find_map(|(index, item)| first_js_expr(item, &format!("{path}[{index}]"))),
        Value::Object(object) => object
            .iter()
            .find_map(|(key, item)| first_js_expr(item, &format!("{path}.{key}"))),
        _ => None,
    }
}

/// 一个尚未求值的 `!!js` 表达式（求值器属 M4-4）。
///
/// `!cordis-js <主体>` 经 `serde_json::to_value` 落成**单键对象**
/// `{"!cordis-js": <主体>}`——serde_yaml 用标签名作键、主体作值。
///
/// 注意识别只认**本标记**，不认任意 `!` 开头的键：探针实测 `!other 5` 也会
/// 落成 `{"!other": 5}` 单键对象，把"单键且键以 `!` 开头"当规则会误报别人的标签。
#[derive(Debug, Clone, PartialEq)]
pub struct JsExpr {
    /// 标签名（正常恒为本层标记；保留字段以便回写保真）。
    pub tag: String,
    /// 标签主体（标量 / 序列 / 映射，原样保留）。
    pub body: Value,
}

impl JsExpr {
    /// 从一个配置值里认出表达式标记；不是标记形态则返回 `None`。
    pub fn from_value(value: &Value) -> Option<JsExpr> {
        let object = value.as_object()?;
        if object.len() != 1 {
            return None;
        }
        let (tag, body) = object.iter().next()?;
        if tag != JS_TAG {
            return None;
        }
        Some(JsExpr {
            tag: tag.clone(),
            body: body.clone(),
        })
    }

    /// 表达式源码文本：标量主体取其字符串，其他形态给出紧凑 JSON。
    ///
    /// 给**诊断/回写保真**用（`unsupported_issue` 的消息里要带原文形态）。
    /// 真正要**求值**时用 [`JsExpr::expression`]：主体不是标量时那是一个错误，
    /// 不是一个表达式。
    pub fn source(&self) -> String {
        match &self.body {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    }

    /// 表达式文本（**求值入口**）：主体必须是标量字符串。
    ///
    /// YAML 允许 `!!js` 挂在映射/序列上（`!!js {a: 1}`），但那**不是表达式**——
    /// 上游的 `__jsExpr` 进 `eval` 同样是错的。这里给一条能读懂的错误，
    /// 而不是把紧凑 JSON 当表达式去解析（那会让报错指向一段用户没写过的字符串）。
    ///
    /// 调用方负责把**路径**接到消息后面（`(at a.config.k)`）。
    pub fn expression(&self) -> std::result::Result<String, String> {
        match &self.body {
            Value::String(text) => Ok(text.clone()),
            Value::Null => Err("!!js 后面没有表达式（主体是空）".to_string()),
            Value::Object(_) => {
                Err("!!js 的表达式主体必须是标量字符串，收到的是**映射**".to_string())
            }
            Value::Array(_) => {
                Err("!!js 的表达式主体必须是标量字符串，收到的是**序列**".to_string())
            }
            other => Err(format!(
                "!!js 的表达式主体必须是标量字符串，收到的是 {other:?}"
            )),
        }
    }

    /// 回到单键对象形态（dump 时由 `yaml_with_tags` 变成真标签）。
    pub fn to_value(&self) -> Value {
        let mut object = Map::new();
        object.insert(self.tag.clone(), self.body.clone());
        Value::Object(object)
    }

    /// M1-4 口径的错误消息：`!!js 暂不支持求值: <表达式> (at <路径>)`。
    ///
    /// **M4-4 起这只是"谁都没注入引擎"口径的诊断文本**：装载器改走"决策期预检 +
    /// `internal/config` 链上的真求值"（`crate::loader`），不再用它拒绝一行。
    /// 保留它是为了那两条识别测试与"表达式清单"这类外部诊断。
    pub fn unsupported_issue(&self, path: &str) -> String {
        format!("!!js 暂不支持求值: {} (at {path})", self.source())
    }
}

/// 使用 `unsafe-libyaml` 真实 YAML 词法扫描器（`yaml_parser_scan`）定位所有匹配
/// `(from_handle, from_suffix)` 的真实 `YAML_TAG_TOKEN`（如 `("!!", "js")` 或
/// `("!", "cordis-js")`），并按 libyaml 返回的精确 UTF-8 字节偏移原地替换为 `replacement`。
///
/// 相比任何手写文本/行状态机，块标量（含换行 `key:\n  |`）、单/双引号标量、多行普通标量、
/// flow 集合多行标量、多行引号键、锚点与注释均由 libyaml 完整 YAML 语法机裁决，
/// 彻底消除“字面量误改写”与“合法 YAML 漏识标签”的打地鼠循环。
fn rewrite_yaml_tag_tokens(
    input: &str,
    from_handle: &str,
    from_suffix: &str,
    replacement: &str,
) -> String {
    use std::ffi::CStr;
    use std::mem::MaybeUninit;
    use std::os::raw::c_char;

    struct ParserGuard(unsafe_libyaml::yaml_parser_t);
    impl Drop for ParserGuard {
        fn drop(&mut self) {
            unsafe {
                unsafe_libyaml::yaml_parser_delete(&mut self.0);
            }
        }
    }

    struct TokenGuard(unsafe_libyaml::yaml_token_t);
    impl Drop for TokenGuard {
        fn drop(&mut self) {
            unsafe {
                unsafe_libyaml::yaml_token_delete(&mut self.0);
            }
        }
    }

    let bytes = input.as_bytes();
    let mut parser_raw = MaybeUninit::<unsafe_libyaml::yaml_parser_t>::zeroed();
    if !unsafe { unsafe_libyaml::yaml_parser_initialize(parser_raw.as_mut_ptr()) }.ok {
        return input.to_string();
    }
    let mut parser = ParserGuard(unsafe { parser_raw.assume_init() });
    unsafe {
        unsafe_libyaml::yaml_parser_set_encoding(&mut parser.0, unsafe_libyaml::YAML_UTF8_ENCODING);
        unsafe_libyaml::yaml_parser_set_input_string(
            &mut parser.0,
            bytes.as_ptr(),
            bytes.len() as u64,
        );
    }

    let mut spans: Vec<(usize, usize)> = Vec::new();
    loop {
        let mut tok_raw = MaybeUninit::<unsafe_libyaml::yaml_token_t>::zeroed();
        if !unsafe { unsafe_libyaml::yaml_parser_scan(&mut parser.0, tok_raw.as_mut_ptr()) }.ok {
            // 非法 YAML 原样交回上层 `serde_yaml::from_str` 报告原始语法错误。
            return input.to_string();
        }
        let tok = TokenGuard(unsafe { tok_raw.assume_init() });
        match tok.0.type_ {
            unsafe_libyaml::YAML_STREAM_END_TOKEN => break,
            unsafe_libyaml::YAML_TAG_TOKEN => {
                let (handle, suffix) = unsafe {
                    let tag = &tok.0.data.tag;
                    let h = if tag.handle.is_null() {
                        ""
                    } else {
                        CStr::from_ptr(tag.handle as *const c_char)
                            .to_str()
                            .unwrap_or("")
                    };
                    let s = if tag.suffix.is_null() {
                        ""
                    } else {
                        CStr::from_ptr(tag.suffix as *const c_char)
                            .to_str()
                            .unwrap_or("")
                    };
                    (h, s)
                };
                if handle == from_handle && suffix == from_suffix {
                    let start = tok.0.start_mark.index as usize;
                    let end = tok.0.end_mark.index as usize;
                    if start < end
                        && end <= bytes.len()
                        && input.is_char_boundary(start)
                        && input.is_char_boundary(end)
                    {
                        spans.push((start, end));
                    }
                }
            }
            _ => {}
        }
    }

    if spans.is_empty() {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len() + spans.len() * replacement.len());
    let mut cursor = 0usize;
    for (start, end) in spans {
        if start >= cursor {
            out.push_str(&input[cursor..start]);
            out.push_str(replacement);
            cursor = end;
        }
    }
    out.push_str(&input[cursor..]);
    out
}

/// 解析前：`!!js`（标准标签，serde_yaml 会丢弃）→ `!cordis-js`（本地标签，能识别）。
fn mark_js_tags(text: &str) -> String {
    rewrite_yaml_tag_tokens(text, "!!", "js", JS_TAG)
}

/// 回写后：`!cordis-js` → `!!js`，让磁盘上仍是 cordis 配置的标准写法。
fn restore_js_tags(text: &str) -> String {
    rewrite_yaml_tag_tokens(text, "!", JS_TAG.trim_start_matches('!'), "!!js")
}

/// `serde_json::Value` → `serde_yaml::Value`，并把表达式标记还原成**真标签**。
///
/// 为什么必须走这一步：单键对象 `{"!cordis-js": x}` 直接序列化只会得到带引号的键
/// （`'!cordis-js': x`）——那是映射，不是标签。构造 `Value::Tagged` 才会发射
/// `!cordis-js x`，再经 [`restore_js_tags`] 还原成 `!!js x`。
fn yaml_with_tags(value: &Value) -> serde_yaml::Value {
    match value {
        Value::Null => serde_yaml::Value::Null,
        Value::Bool(item) => serde_yaml::Value::Bool(*item),
        Value::Number(item) => {
            if let Some(int) = item.as_i64() {
                serde_yaml::Value::Number(int.into())
            } else if let Some(uint) = item.as_u64() {
                serde_yaml::Value::Number(uint.into())
            } else {
                serde_yaml::Value::Number(item.as_f64().unwrap_or_default().into())
            }
        }
        Value::String(item) => serde_yaml::Value::String(item.clone()),
        Value::Array(list) => {
            serde_yaml::Value::Sequence(list.iter().map(yaml_with_tags).collect())
        }
        Value::Object(object) => {
            if let Some(expr) = JsExpr::from_value(value) {
                return serde_yaml::Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
                    tag: serde_yaml::value::Tag::new(&expr.tag),
                    value: yaml_with_tags(&expr.body),
                }));
            }
            serde_yaml::Value::Mapping(
                object
                    .iter()
                    .map(|(key, item)| {
                        (serde_yaml::Value::String(key.clone()), yaml_with_tags(item))
                    })
                    .collect(),
            )
        }
    }
}

/// 一条目的展示名：优先 `id`，退回 `name`（与装载器的 label 规则一致）。
fn entry_label(entry: &Entry) -> String {
    match (&entry.id, entry.name.is_empty()) {
        (Some(id), _) => id.clone(),
        (None, false) => entry.name.clone(),
        (None, true) => "<unnamed>".to_string(),
    }
}

/// 收集一条目上**未求值**的 `!!js` 表达式（`disabled` 与 `config` 两处位置）。
///
/// **M4-4 起这是"诊断口径"的清单**（谁都没注入引擎时的完整清单）：装载器不再用它
/// 拒绝一行——`disabled` 位置在挂载决策里现算，`config` 位置在决策期预检、在
/// `internal/config` 链上真求值（`crate::loader`）。保留它是因为它是"配置里有哪些
/// 表达式"的唯一入口（启动审计、配置检查工具）。
/// 覆盖 `disabled` 与 `config` 里任意深度的位置；无表达式时返回空列表。
pub fn js_expression_issues(entry: &Entry) -> Vec<String> {
    let label = entry_label(entry);
    let mut issues = Vec::new();
    if let Some(expr) = &entry.disabled_expr {
        issues.push(expr.unsupported_issue(&format!("{label}.disabled")));
    }
    issues.extend(config_js_issues(entry));
    issues
}

/// 只看 `config` 位置的表达式（[`js_expression_issues`] 的一半，供分类统计用）。
pub fn config_js_issues(entry: &Entry) -> Vec<String> {
    let mut issues = Vec::new();
    if let Some(config) = &entry.config {
        collect_config_issues_into(
            config,
            &format!("{}.config", entry_label(entry)),
            &mut issues,
        );
    }
    issues
}

/// 整棵树上的 `!!js` **能力快照**（M9，D5）：每行一份，label → 静态读到的宿主面。
///
/// 与 [`collect_js_expression_issues`] 同一套走法（含 group 子树），区别是它**不报错**：
/// 能力面是"这一行可能碰到什么"，语法错的表达式由求值那条路去报。没有表达式的行也会
/// 出现（空快照）——报告里的形状因此是稳定的，能逐字断言。
pub fn collect_js_capabilities(entries: &[Entry]) -> Vec<(String, crate::js::JsCapabilities)> {
    let mut out = Vec::new();
    for entry in entries {
        collect_entry_capabilities(entry, &mut out);
    }
    out
}

pub(crate) fn entry_js_capabilities(entry: &Entry) -> crate::js::JsCapabilities {
    let mut caps = crate::js::JsCapabilities::default();
    if let Some(expr) = &entry.disabled_expr {
        if let Ok(text) = expr.expression() {
            caps.absorb(crate::js::capabilities_of(&text));
        }
    }
    if let Some(config) = &entry.config {
        walk_config_exprs(
            config,
            &format!("{}.config", entry_label(entry)),
            &mut |expr, _path| {
                if let Ok(text) = expr.expression() {
                    caps.absorb(crate::js::capabilities_of(&text));
                }
            },
        );
    }
    caps.normalize();
    caps
}

fn collect_entry_capabilities(entry: &Entry, out: &mut Vec<(String, crate::js::JsCapabilities)>) {
    out.push((entry_label(entry), entry_js_capabilities(entry)));
    for child in &entry.children {
        collect_entry_capabilities(child, out);
    }
}

/// 整棵树上的表达式问题（含 group 及其子树），供装载器一次性汇总上报。
pub fn collect_js_expression_issues(entries: &[Entry]) -> Vec<(String, Vec<String>)> {
    collect_issues(entries, js_expression_issues)
}

fn collect_issues(
    entries: &[Entry],
    per_entry: fn(&Entry) -> Vec<String>,
) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for entry in entries {
        let issues = per_entry(entry);
        if !issues.is_empty() {
            out.push((entry_label(entry), issues));
        }
        out.extend(collect_issues(&entry.children, per_entry));
    }
    out
}

/// 深度优先遍历配置树，把每个 `!!js` 表达式连同它所在的**路径**交给 `visit`。
///
/// 修的是"两处各写一遍同样的走法"：问题清单（[`collect_config_issues_into`]）与
/// 能力快照（[`collect_entry_capabilities`]）要看见的是同一批表达式，
/// 走法分家就会出现"报告里看得见、快照里看不见"这种最难查的偏差。
fn walk_config_exprs(value: &Value, path: &str, visit: &mut impl FnMut(&JsExpr, &str)) {
    if let Some(expr) = JsExpr::from_value(value) {
        visit(&expr, path);
        return;
    }
    match value {
        Value::Object(object) => {
            for (key, item) in object {
                walk_config_exprs(item, &format!("{path}.{key}"), visit);
            }
        }
        Value::Array(list) => {
            for (index, item) in list.iter().enumerate() {
                walk_config_exprs(item, &format!("{path}[{index}]"), visit);
            }
        }
        _ => {}
    }
}

fn collect_config_issues_into(value: &Value, path: &str, issues: &mut Vec<String>) {
    walk_config_exprs(value, path, &mut |expr, path| {
        issues.push(expr.unsupported_issue(path))
    });
}

/// 把一份 YAML 文档（条目数组）解析成归一化条目树。
pub fn parse_entries(yaml: &str) -> Result<Vec<Entry>, String> {
    parse_entries_with_startup(yaml).map(|(entries, _)| entries)
}

/// 解析主配置条目树，并同步返回一一对应的行级启动策略树。
pub(crate) fn parse_entries_with_startup(
    yaml: &str,
) -> Result<(Vec<Entry>, Vec<StartupNode>), String> {
    let yaml = yaml.strip_prefix('\u{feff}').unwrap_or(yaml);
    let value: serde_yaml::Value =
        serde_yaml::from_str(&mark_js_tags(yaml)).map_err(|e| e.to_string())?;
    let json = serde_json::to_value(value).map_err(|e| e.to_string())?;
    match json {
        Value::Array(list) => parse_entry_list_with_startup(&list, ""),
        // 空文件是合法的"没有任何条目"，不该报错（patch 层经常会从空开始叠）。
        Value::Null => Ok((Vec::new(), Vec::new())),
        Value::Object(object) => {
            // 兼容 `{ plugins: [...] }` 这种包一层容器文件的写法。
            match object.get("plugins").and_then(|v| v.as_array()) {
                Some(list) => parse_entry_list_with_startup(list, "plugins "),
                None => Err("配置文件的顶层既不是数组，也没有 plugins 字段".into()),
            }
        }
        _ => Err("配置文件的顶层必须是数组".into()),
    }
}

/// 把一份 YAML 文档（patch 数组）解析成 patch 列表。
pub fn parse_patches(yaml: &str) -> Result<Vec<Patch>, String> {
    parse_patches_with_startup(yaml).map(|items| items.into_iter().map(|item| item.patch).collect())
}

/// 解析 patch 列表，并同步返回行级启动策略覆盖元数据。
pub(crate) fn parse_patches_with_startup(yaml: &str) -> Result<Vec<PatchWithStartup>, String> {
    let yaml = yaml.strip_prefix('\u{feff}').unwrap_or(yaml);
    let value: serde_yaml::Value =
        serde_yaml::from_str(&mark_js_tags(yaml)).map_err(|e| e.to_string())?;
    let json = serde_json::to_value(value).map_err(|e| e.to_string())?;
    match json {
        Value::Array(list) => {
            let mut patches = Vec::with_capacity(list.len());
            for (index, item) in list.iter().enumerate() {
                let patch = Patch::from_value_checked(item)
                    .map_err(|err| format!("patch 第 {} 条不是合法 patch: {err}", index + 1))?;
                let object = item
                    .as_object()
                    .expect("Patch::from_value_checked 已校验对象");
                let startup = parse_row_startup(object)
                    .map_err(|err| format!("patch 第 {} 条不是合法 patch: {err}", index + 1))?;
                let insert_startup = match object.get("insert") {
                    Some(Value::Array(entries)) => {
                        let mut nodes = Vec::with_capacity(entries.len());
                        for entry_val in entries {
                            let (_, node) = entry_from_value_with_startup(entry_val)?;
                            nodes.push(node);
                        }
                        Some(nodes)
                    }
                    _ => None,
                };
                patches.push(PatchWithStartup {
                    patch,
                    startup,
                    insert_startup,
                });
            }
            Ok(patches)
        }
        Value::Null => Ok(Vec::new()),
        _ => Err("patch 文件的顶层必须是数组".into()),
    }
}

fn parse_entry_list_with_startup(
    list: &[Value],
    prefix: &str,
) -> Result<(Vec<Entry>, Vec<StartupNode>), String> {
    let mut entries = Vec::with_capacity(list.len());
    let mut nodes = Vec::with_capacity(list.len());
    for (index, item) in list.iter().enumerate() {
        let (entry, node) = entry_from_value_with_startup(item)
            .map_err(|err| format!("{prefix}第 {} 行不是合法条目: {err}", index + 1))?;
        entries.push(entry);
        nodes.push(node);
    }
    Ok((entries, nodes))
}

fn parse_entry_children(list: &[Value]) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::with_capacity(list.len());
    for (index, item) in list.iter().enumerate() {
        let entry = Entry::from_value_checked(item)
            .map_err(|err| format!("第 {} 个子条目不是合法条目: {err}", index + 1))?;
        entries.push(entry);
    }
    Ok(entries)
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "空值",
        Value::Bool(_) => "布尔值",
        Value::Number(_) => "数字",
        Value::String(_) => "字符串",
        Value::Array(_) => "序列",
        Value::Object(_) => "映射",
    }
}

/// 序列化回 YAML（配置回写用）。
///
/// 表达式标记经 `yaml_with_tags` 变成真标签 `!cordis-js ...` 再还原成 `!!js ...`——
/// 直接序列化只会得到带引号的键（映射，不是标签）。
pub fn dump_entries(entries: &[Entry]) -> Result<String, String> {
    dump_entries_with_startup(entries, &[])
}

/// 带行级启动策略树回写 YAML（无显式声明时输出与 [`dump_entries`] 逐字节一致）。
pub(crate) fn dump_entries_with_startup(
    entries: &[Entry],
    nodes: &[StartupNode],
) -> Result<String, String> {
    fn entry_to_value_with_startup(entry: &Entry, node: Option<&StartupNode>) -> Value {
        let mut value = entry.to_value();
        if let Value::Object(object) = &mut value {
            if let Some(startup) = node.and_then(|n| n.explicit) {
                object.insert("startup".into(), Value::String(startup.as_str().into()));
            }
            if entry.group {
                let child_nodes = node.map(|n| n.children.as_slice()).unwrap_or(&[]);
                let children = entry
                    .children
                    .iter()
                    .enumerate()
                    .map(|(index, child)| {
                        entry_to_value_with_startup(child, child_nodes.get(index))
                    })
                    .collect();
                object.insert("config".into(), Value::Array(children));
            }
        }
        value
    }
    let json = Value::Array(
        entries
            .iter()
            .enumerate()
            .map(|(index, entry)| entry_to_value_with_startup(entry, nodes.get(index)))
            .collect(),
    );
    let text = serde_yaml::to_string(&yaml_with_tags(&json)).map_err(|e| e.to_string())?;
    Ok(restore_js_tags(&text))
}

/// 构造一条**追加到 patch 层**的 overlay（`id` + 可选 `config` / `disabled`）。
///
/// patch 插入的行只能写回 patch 层；主配置里根本没有它。形状是 patch 数组的一条：
/// `- id: x` + `config`（整体替换）或 `disabled`。与 [`dump_entries`] 走同一条
/// 序列化路径，所以 `!!js` 标签同样保真。
pub fn dump_overlay(
    id: &str,
    config: Option<&Config>,
    disabled: Option<bool>,
) -> Result<String, String> {
    let mut object = Map::new();
    object.insert("id".into(), Value::String(id.to_string()));
    if let Some(config) = config {
        object.insert("config".into(), config.clone());
    }
    if let Some(disabled) = disabled {
        object.insert("disabled".into(), Value::Bool(disabled));
    }
    let json = Value::Array(vec![Value::Object(object)]);
    let text = serde_yaml::to_string(&yaml_with_tags(&json)).map_err(|e| e.to_string())?;
    Ok(restore_js_tags(&text))
}

#[cfg(test)]
mod review_regression_tests {
    use super::*;

    #[test]
    fn patch_rejects_js_expr_in_inject_and_intercept() {
        let err = parse_patches("- id: a\n  inject:\n    svc: !!js foo\n").unwrap_err();
        assert!(err.contains("entry-level inject"), "unexpected err: {err}");

        let err = parse_patches("- id: a\n  intercept:\n    svc: !!js bar\n").unwrap_err();
        assert!(
            err.contains("entry-level intercept"),
            "unexpected err: {err}"
        );
    }

    #[test]
    fn group_overlay_warns_on_invalid_children_and_clears_on_ungroup() {
        let root = parse_entries(
            "- id: g\n  name: grp\n  group: true\n  config:\n    - id: c1\n      name: p1\n",
        )
        .unwrap();
        let patches = parse_patches(
            "- id: g\n  config:\n    - id: c2\n      name: p2\n      disabled: \"not-a-bool\"\n",
        )
        .unwrap();
        let mut warnings = Vec::new();
        let out = apply_entry_patches(root.clone(), &patches, &mut |w| {
            warnings.push(w.to_string())
        });
        assert!(
            warnings.iter().any(|w| w.contains("子条目非法")),
            "expected warning for invalid child, got {warnings:?}"
        );
        assert!(out[0].children.is_empty());

        let ungroup = parse_patches("- id: g\n  group: false\n").unwrap();
        let out = apply_entry_patches(root, &ungroup, &mut |_| {});
        assert!(!out[0].group);
        assert!(out[0].children.is_empty());
    }

    #[test]
    fn rewrite_tag_ignores_token_glued_to_preceding_word() {
        let yaml = "- name: p\n  config:\n    text: note!!js hello\n";
        let entries = parse_entries(yaml).unwrap();
        let text = entries[0].config.as_ref().unwrap()["text"]
            .as_str()
            .unwrap();
        assert_eq!(text, "note!!js hello");
    }

    #[test]
    fn patch_owners_tracks_named_child_inside_anonymous_group() {
        let patches = parse_patches(
            "- insert:\n  - name: grp\n    group: true\n    config:\n      - id: child1\n        name: p1\n        config:\n          k: 1\n",
        )
        .unwrap();
        let mut owners = PatchOwners::default();
        let _ = apply_entry_patches_tracked(Vec::new(), &patches, 2, &mut owners, &mut |_| {});
        assert_eq!(owners.config_layer("child1"), Some(2));
    }

    #[test]
    fn r4_p1_rewrite_tag_handles_space_preceded_quotes_in_plain_and_block_scalars() {
        // 1. Plain scalars containing space-preceded apostrophe / double quote
        let yaml_plain = concat!(
            "- id: a\n",
            "  name: llm\n",
            "  config:\n",
            "    note: The '90s era\n",
            "    desc: Say \"hello world\n",
            "- id: b\n",
            "  name: gated\n",
            "  disabled: !!js false\n",
            "  config:\n",
            "    token: !!js process.env.API_KEY\n",
        );
        let entries = parse_entries(yaml_plain).expect("parse plain scalar YAML");
        assert_eq!(
            entries[1].disabled_expr.as_ref().map(|e| e.source()),
            Some("false".to_string()),
            "普通标量空格后的撇号/引号不得吞掉后续 disabled: !!js 标签"
        );
        assert!(
            JsExpr::from_value(&entries[1].config.as_ref().unwrap()["token"]).is_some(),
            "普通标量空格后的撇号/引号不得吞掉后续 config 中的 !!js 标签: {:?}",
            entries[1].config
        );
        let dumped = dump_entries(&entries).expect("dump_entries");
        assert!(
            !dumped.contains(JS_TAG),
            "回写磁盘的 YAML 绝不得残留内部临时标签 !cordis-js:\n{dumped}"
        );
        assert!(
            dumped.contains("!!js"),
            "回写 YAML 必须还原 !!js:\n{dumped}"
        );
        let reparsed = parse_entries(&dumped).expect("reparse dumped YAML");
        assert_eq!(
            reparsed[1].disabled_expr.as_ref().map(|e| e.source()),
            Some("false".to_string())
        );
        assert!(JsExpr::from_value(&reparsed[1].config.as_ref().unwrap()["token"]).is_some());

        // 2. Multiline `|` and `>` block scalars containing quotes & apostrophes
        let yaml_block = concat!(
            "- id: a\n",
            "  name: llm\n",
            "  config:\n",
            "    prompt: |\n",
            "      Use \"quotes freely\n",
            "      and 'apostrophes in '90s prose\n",
            "    folded: >-\n",
            "      Another \"unclosed quote\n",
            "- id: b\n",
            "  name: gated\n",
            "  config:\n",
            "    key: !!js process.env.BLOCK_KEY\n",
        );
        let entries2 = parse_entries(yaml_block).expect("parse block scalar YAML");
        assert!(
            JsExpr::from_value(&entries2[1].config.as_ref().unwrap()["key"]).is_some(),
            "块标量 `|`/`>` 内的未闭合引号不得吞掉后续 !!js 标签: {:?}",
            entries2[1].config
        );
        let dumped2 = dump_entries(&entries2).expect("dump_entries block");
        assert!(
            !dumped2.contains(JS_TAG),
            "含块标量回写后的 YAML 不得残留 !cordis-js:\n{dumped2}"
        );
        let reparsed2 = parse_entries(&dumped2).expect("reparse block YAML");
        assert!(JsExpr::from_value(&reparsed2[1].config.as_ref().unwrap()["key"]).is_some());
    }

    #[test]
    fn r4_p1_yaml_tag_scanner_roundtrip_matrix_never_leaks_internal_tag() {
        let plain_samples = [
            "The '90s era",
            "Say \"hello world",
            "it's a \"mixed 'quote case",
            "hello !!js literal in plain",
            "multi\n      '80s continuation",
        ];
        let block_headers = ["|", "|-", "|+", ">", ">-", "|2"];
        let block_bodies = [
            "      Use \"unclosed quote\n      and '90s apostrophe\n      !!js not-a-tag-in-block",
            "      line1\n\n      'single and \"double",
        ];
        let mut checked = 0usize;
        for plain in plain_samples {
            for header in block_headers {
                for body in block_bodies {
                    let yaml = format!(
                        "- id: a\n  name: llm\n  config:\n    plain: {plain}\n    blk: {header}\n{body}\n    flow: [!!js a, 'quoted !!js', plain '90s, !!js b]\n    fmap: {{\"k\":!!js c, m: Say \"hi, n: !!js d}}\n- id: b\n  name: gated\n  disabled: !!js false\n  config:\n    token: !!js process.env.KEY\n"
                    );
                    let entries = parse_entries(&yaml).unwrap_or_else(|e| {
                        panic!("parse failed for header={header} plain={plain:?}: {e}\n{yaml}")
                    });
                    assert_eq!(
                        entries[1].disabled_expr.as_ref().map(|e| e.source()),
                        Some("false".to_string()),
                        "disabled_expr lost:\n{yaml}"
                    );
                    assert!(
                        JsExpr::from_value(&entries[1].config.as_ref().unwrap()["token"]).is_some(),
                        "token expr lost:\n{yaml}"
                    );
                    let flow = entries[0].config.as_ref().unwrap()["flow"]
                        .as_array()
                        .unwrap();
                    assert!(JsExpr::from_value(&flow[0]).is_some());
                    assert_eq!(flow[1], Value::String("quoted !!js".to_string()));
                    assert!(JsExpr::from_value(&flow[3]).is_some());
                    let fmap = entries[0].config.as_ref().unwrap()["fmap"]
                        .as_object()
                        .unwrap();
                    assert!(JsExpr::from_value(&fmap["k"]).is_some());
                    assert!(JsExpr::from_value(&fmap["n"]).is_some());

                    let dumped = dump_entries(&entries).expect("dump_entries");
                    assert!(
                        !dumped.contains(JS_TAG),
                        "leaked internal tag !cordis-js in dumped YAML:\n{dumped}"
                    );
                    let reparsed = parse_entries(&dumped).expect("reparse dumped");
                    assert_eq!(entries, reparsed, "round-trip mismatch:\n{dumped}");
                    checked += 1;
                }
            }
        }
        assert!(checked >= 60);
    }

    #[test]
    fn r5_p1_libyaml_tag_engine_handles_all_four_yaml_1_2_edge_cases() {
        // 1. Next-line block scalar header with unclosed quote & literal `!!js` inside block body.
        let yaml_next_line_block = concat!(
            "- id: a\n",
            "  name: llm\n",
            "  config:\n",
            "    prompt:\n",
            "      |\n",
            "      \"unclosed\n",
            "      !!js literal-in-block\n",
            "    token: !!js x\n",
        );
        let e1 = parse_entries(yaml_next_line_block).expect("parse next-line block scalar");
        let cfg1 = e1[0].config.as_ref().unwrap();
        assert_eq!(
            cfg1["prompt"].as_str().unwrap(),
            "\"unclosed\n!!js literal-in-block\n"
        );
        assert_eq!(
            JsExpr::from_value(&cfg1["token"]).map(|e| e.source()),
            Some("x".to_string())
        );

        // 2. Next-line multiline plain scalar with apostrophe & literal `!!js` on continuation line.
        let yaml_next_line_plain = concat!(
            "- id: a\n",
            "  name: llm\n",
            "  config:\n",
            "    prompt:\n",
            "      First line\n",
            "      '90s continuation\n",
            "      !!js literal-in-plain\n",
            "    token: !!js x\n",
        );
        let e2 = parse_entries(yaml_next_line_plain).expect("parse next-line multiline plain");
        let cfg2 = e2[0].config.as_ref().unwrap();
        assert_eq!(
            cfg2["prompt"].as_str().unwrap(),
            "First line '90s continuation !!js literal-in-plain"
        );
        assert_eq!(
            JsExpr::from_value(&cfg2["token"]).map(|e| e.source()),
            Some("x".to_string())
        );

        // 3. Multiline plain scalar inside flow collection.
        let yaml_flow_multiline = concat!(
            "- id: a\n",
            "  name: llm\n",
            "  config:\n",
            "    flow: [\n",
            "      The\n",
            "      '90s era,\n",
            "      !!js x\n",
            "    ]\n",
        );
        let e3 = parse_entries(yaml_flow_multiline).expect("parse flow multiline plain");
        let flow3 = e3[0].config.as_ref().unwrap()["flow"].as_array().unwrap();
        assert_eq!(flow3[0], Value::String("The '90s era".to_string()));
        assert_eq!(
            JsExpr::from_value(&flow3[1]).map(|e| e.source()),
            Some("x".to_string())
        );

        // 4. Explicit multiline quoted mapping key, multiline quoted value containing `!!js`,
        //    and anchored mapping key with block scalar.
        let yaml_quoted_and_anchored = concat!(
            "- id: a\n",
            "  name: llm\n",
            "  config:\n",
            "    ? \"multi\n",
            "      key\"\n",
            "    : \"line1\n",
            "      !!js inside-double-quotes\"\n",
            "    &k anchored: |\n",
            "      \"unclosed\n",
            "    token: !!js y\n",
        );
        let e4 = parse_entries(yaml_quoted_and_anchored).expect("parse multiline quoted + anchor");
        let cfg4 = e4[0].config.as_ref().unwrap();
        assert_eq!(
            cfg4["multi key"],
            Value::String("line1 !!js inside-double-quotes".to_string())
        );
        assert_eq!(cfg4["anchored"], Value::String("\"unclosed\n".to_string()));
        assert_eq!(
            JsExpr::from_value(&cfg4["token"]).map(|e| e.source()),
            Some("y".to_string())
        );

        // Round-trip all 4 cases cleanly without ever leaking `!cordis-js`.
        for entries in [&e1, &e2, &e3, &e4] {
            let dumped = dump_entries(entries).expect("dump_entries");
            assert!(
                !dumped.contains(JS_TAG),
                "leaked internal tag !cordis-js:\n{dumped}"
            );
            let reparsed = parse_entries(&dumped).expect("reparse dumped");
            assert_eq!(*entries, reparsed);
        }
    }
}
