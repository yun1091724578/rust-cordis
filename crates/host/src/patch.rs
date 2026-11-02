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

    fn overlay(&mut self, patch: &Patch) {
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
        }
        if let Some(config) = &patch.config {
            if self.group {
                self.config = None;
                self.children = config
                    .as_array()
                    .map(|list| list.iter().filter_map(Entry::from_value).collect())
                    .unwrap_or_default();
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
        if let Some(value) = inject.as_ref().filter(|value| !value.is_null()) {
            parse_inject(value).map_err(|err| format!("{err} (at inject)"))?;
        }
        let isolate = object.get("isolate").cloned();
        if let Some(value) = isolate.as_ref().filter(|value| !value.is_null()) {
            validate_isolate(value).map_err(|err| format!("{err} (at isolate)"))?;
        }
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
            intercept: object.get("intercept").cloned(),
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
        let Some(id) = entry.id.as_deref() else {
            return;
        };
        let owners = self.by_id.entry(id.to_string()).or_default();
        if entry.config.is_some() {
            owners.config = Some(layer);
        }
        if entry.disabled || entry.disabled_expr.is_some() {
            owners.disabled = Some(layer);
        }
        for child in &entry.children {
            self.note(child, layer);
        }
    }

    fn note_overlay(&mut self, entry: &Entry, patch: &Patch, layer: usize) {
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
        // group 的 config 替换会带来新的 children；它们的字段由当前层引入。
        if patch.config.is_some() {
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
                // group 的 config 会整体替换 children：旧子树的归属必须先失效，
                // 否则被移除的子 id 会留下幽灵 owner。
                if patch.config.is_some() {
                    for child in &target.children {
                        owners.untrack(child);
                    }
                }
                target.overlay(patch);
                owners.note_overlay(target, patch, layer);
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

/// 把标签 token `from` 改写为 `to`，**只改引号与注释之外**的文本。
///
/// 为什么不能盲替换：`quoted: '!!js not a tag'` 里的 `!!js` 是用户数据，
/// `# !!js ...` 里的只是注释。这条函数同时服务于解析（`!!js` → `!cordis-js`）
/// 与回写（反向），必须对称且无损。
///
/// 边界规则：`from` 整体匹配，且后面紧跟空白 / 行尾（`note!!js` 这种要放过）。
///
/// 已知边界：这不是完整的 YAML 词法分析器——块标量**正文**里裸写的 `!!js`
/// 也会被改写。由于正反两个方向对称，内容不会失真；而表达式正文是 JS，
/// 裸写 `!!js` 不构成真实场景。
fn rewrite_tag_outside_quotes(input: &str, from: &str, to: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    // 单引号内 `''` 是转义引号；双引号内 `\` 转义下一个字符。
    let mut quote: Option<u8> = None;
    let mut comment = false;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'\n' {
            comment = false;
            out.push('\n');
            i += 1;
            continue;
        }
        if comment {
            let (ch, next) = char_at(input, i);
            out.push(ch);
            i = next;
            continue;
        }
        if let Some(active) = quote {
            if byte == active {
                if active == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
                    out.push_str("''");
                    i += 2;
                } else {
                    quote = None;
                    out.push(active as char);
                    i += 1;
                }
                continue;
            }
            if byte == b'\\' && active == b'"' {
                let (ch, next) = char_at(input, i);
                out.push(ch);
                i = next;
                if i < bytes.len() {
                    let (ch, next) = char_at(input, i);
                    out.push(ch);
                    i = next;
                }
                continue;
            }
            let (ch, next) = char_at(input, i);
            out.push(ch);
            i = next;
            continue;
        }
        // 引号与注释之外。
        if byte == b'\'' || byte == b'"' {
            quote = Some(byte);
            out.push(byte as char);
            i += 1;
            continue;
        }
        // YAML 的注释必须与内容之间有空白，`v#notcomment` 不是注释。
        if byte == b'#' && (i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b'\n')) {
            comment = true;
            out.push('#');
            i += 1;
            continue;
        }
        if input[i..].starts_with(from) {
            let after = i + from.len();
            if after >= bytes.len() || matches!(bytes[after], b' ' | b'\t' | b'\r' | b'\n') {
                out.push_str(to);
                i = after;
                continue;
            }
        }
        let (ch, next) = char_at(input, i);
        out.push(ch);
        i = next;
    }
    out
}

fn char_at(input: &str, index: usize) -> (char, usize) {
    let ch = input[index..]
        .chars()
        .next()
        .expect("index 必须落在字符边界上");
    (ch, index + ch.len_utf8())
}

/// 解析前：`!!js`（标准标签，serde_yaml 会丢弃）→ `!cordis-js`（本地标签，能识别）。
fn mark_js_tags(text: &str) -> String {
    rewrite_tag_outside_quotes(text, "!!js", JS_TAG)
}

/// 回写后：`!cordis-js` → `!!js`，让磁盘上仍是 cordis 配置的标准写法。
fn restore_js_tags(text: &str) -> String {
    rewrite_tag_outside_quotes(text, JS_TAG, "!!js")
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

fn collect_entry_capabilities(entry: &Entry, out: &mut Vec<(String, crate::js::JsCapabilities)>) {
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
    out.push((entry_label(entry), caps));
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
    let yaml = yaml.strip_prefix('\u{feff}').unwrap_or(yaml);
    let value: serde_yaml::Value =
        serde_yaml::from_str(&mark_js_tags(yaml)).map_err(|e| e.to_string())?;
    let json = serde_json::to_value(value).map_err(|e| e.to_string())?;
    match json {
        Value::Array(list) => parse_entry_list(&list, ""),
        // 空文件是合法的"没有任何条目"，不该报错（patch 层经常会从空开始叠）。
        Value::Null => Ok(Vec::new()),
        Value::Object(object) => {
            // 兼容 `{ plugins: [...] }` 这种包一层容器文件的写法。
            match object.get("plugins").and_then(|v| v.as_array()) {
                Some(list) => parse_entry_list(list, "plugins "),
                None => Err("配置文件的顶层既不是数组，也没有 plugins 字段".into()),
            }
        }
        _ => Err("配置文件的顶层必须是数组".into()),
    }
}

/// 把一份 YAML 文档（patch 数组）解析成 patch 列表。
pub fn parse_patches(yaml: &str) -> Result<Vec<Patch>, String> {
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
                patches.push(patch);
            }
            Ok(patches)
        }
        Value::Null => Ok(Vec::new()),
        _ => Err("patch 文件的顶层必须是数组".into()),
    }
}

fn parse_entry_list(list: &[Value], prefix: &str) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::with_capacity(list.len());
    for (index, item) in list.iter().enumerate() {
        let entry = Entry::from_value_checked(item)
            .map_err(|err| format!("{prefix}第 {} 行不是合法条目: {err}", index + 1))?;
        entries.push(entry);
    }
    Ok(entries)
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
    let json = Value::Array(entries.iter().map(Entry::to_value).collect());
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
