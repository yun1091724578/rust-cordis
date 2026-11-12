//! `cordis-cli` 的**库面**（M5 起）。
//!
//! 子命令的实现在这里、`main.rs` 只做参数解析与打印——这样 A5 才可判定：
//! "能力清单能在 mount 之前得到"这件事要能在**进程内**被断言
//! （只有进程内才能塞假 `PluginSource`；D9 把真 DLL 的断言留给 spike）。
//!
//! # `inspect` 的两条纪律
//! 1. **永不挂载**：不建 fiber、不注册实例——`InspectReport.mounted` 恒为 0，
//!    这正是 A5 的判据。探针仍会加载动态库并调用 `OP_PING` / `OP_VOLATILE_PATHS`；
//!    它是“不建实例”，不是“不执行插件代码”。
//! 2. 每行给全 **inject / provision / volatile** 三项，未声明 provision 的行
//!    单独列进 `undeclared`（"这行的权限面没人声明过"，装载前就该看得见）。

use cordis_core::App;
use cordis_host::loader::{
    load_with_source, InterpolationMode, LoaderConfig, LoaderState, PendingRow, PluginSource,
};
use cordis_host::patch::{apply_entry_patches, parse_entries, parse_inject, parse_patches, Entry};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod service;
pub mod shutdown;

const CAPABILITY_ORIGIN: &str = cordis_host::CAPABILITY_ORIGIN;
const INJECT_SOURCE: &str = "plugin+config-entry";
const PROVENANCE_MANIFEST: &str = "manifest";
const PROVENANCE_MISSING: &str = "missing/manifest";

/// artifact 的 provenance 状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProvenanceSource {
    /// sidecar `<artifact>.manifest.json` 存在且可解析。
    Manifest,
    /// sidecar 缺失或不可解析（`source = missing/manifest`）。
    MissingManifest,
}

impl ProvenanceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ProvenanceSource::Manifest => PROVENANCE_MANIFEST,
            ProvenanceSource::MissingManifest => PROVENANCE_MISSING,
        }
    }
}

/// 一份 artifact 的来源摘要（审计面；不参与准入决策）。
///
/// `sha256` 是宿主对**实际文件**的计算值；manifest 的 id / version / revision /
/// abi_version 是发布方自述。`source == missing/manifest` 表示 sidecar 缺失或
/// 不可解析——此时 manifest 字段全部为 `None`，但 artifact 路径与（可读时）
/// SHA256 仍会显示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    pub artifact: String,
    pub sha256: Option<String>,
    pub source_revision: Option<String>,
    pub id: Option<String>,
    pub version: Option<String>,
    pub abi_version: Option<u32>,
    /// manifest 自述的 hash，便于与 `sha256` 对照。
    pub manifest_sha256: Option<String>,
    /// `sha256 == manifest_sha256`；sidecar 缺失时为 None。
    pub manifest_hash_matches: Option<bool>,
    /// 当前进程的准入模式：enforced / dev-unverified / denied。
    pub admission: String,
    pub source: ProvenanceSource,
}

impl Provenance {
    /// 只读审计：读取 artifact 的实际 SHA256 与其 sidecar manifest。永不 panic。
    pub fn from_artifact(artifact: impl AsRef<Path>) -> Self {
        let path = artifact.as_ref();
        let sha256 = cordis_host::source::sha256_file(path).ok();
        let admission = cordis_host::source::admission_mode().to_string();
        match cordis_host::source::read_manifest(path) {
            Ok(manifest) => {
                let manifest_hash_matches = sha256
                    .as_deref()
                    .map(|actual| actual.eq_ignore_ascii_case(&manifest.sha256));
                Self {
                    artifact: path.display().to_string(),
                    sha256,
                    source_revision: Some(manifest.source_revision),
                    id: Some(manifest.id),
                    version: Some(manifest.version),
                    abi_version: Some(manifest.abi_version),
                    manifest_sha256: Some(manifest.sha256),
                    manifest_hash_matches,
                    admission,
                    source: ProvenanceSource::Manifest,
                }
            }
            Err(_) => Self {
                artifact: path.display().to_string(),
                sha256,
                source_revision: None,
                id: None,
                version: None,
                abi_version: None,
                manifest_sha256: None,
                manifest_hash_matches: None,
                admission,
                source: ProvenanceSource::MissingManifest,
            },
        }
    }

    fn render_lines(&self, indent: &str, out: &mut String) {
        out.push_str(&format!(
            "{indent}provenance source = {}\n",
            self.source.as_str()
        ));
        out.push_str(&format!("{indent}admission: {}\n", self.admission));
        out.push_str(&format!(
            "{indent}artifact: {}\n",
            display_or_missing(Some(self.artifact.as_str()))
        ));
        out.push_str(&format!(
            "{indent}sha256: {}\n",
            display_or_missing(self.sha256.as_deref())
        ));
        out.push_str(&format!(
            "{indent}manifest sha256: {}\n",
            display_or_missing(self.manifest_sha256.as_deref())
        ));
        out.push_str(&format!(
            "{indent}hash matches manifest: {}\n",
            self.manifest_hash_matches
                .map(|matches| matches.to_string())
                .unwrap_or_else(|| "(missing)".to_string())
        ));
        out.push_str(&format!(
            "{indent}source_revision: {}\n",
            display_or_missing(self.source_revision.as_deref())
        ));
        if self.source == ProvenanceSource::Manifest {
            out.push_str(&format!(
                "{indent}manifest: id={} version={} abi_version={}\n",
                display_or_missing(self.id.as_deref()),
                display_or_missing(self.version.as_deref()),
                self.abi_version
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| "(missing)".to_string()),
            ));
        } else {
            out.push_str(&format!("{indent}manifest: (missing)\n"));
        }
    }

    fn to_json(&self) -> serde_json::Value {
        let manifest = self.source.eq(&ProvenanceSource::Manifest).then(|| {
            serde_json::json!({
                "id": self.id,
                "version": self.version,
                "abi_version": self.abi_version,
            })
        });
        serde_json::json!({
            "artifact": self.artifact,
            "sha256": self.sha256,
            "source": self.source.as_str(),
            "admission": self.admission,
            "manifest_sha256": self.manifest_sha256,
            "manifest_hash_matches": self.manifest_hash_matches,
            "source_revision": self.source_revision,
            "manifest": manifest,
        })
    }
}

fn display_or_missing(value: Option<&str>) -> &str {
    value.filter(|text| !text.is_empty()).unwrap_or("(missing)")
}

/// `status` 里每行挂载的 provenance。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowProvenance {
    pub label: String,
    pub provenance: Provenance,
}

/// 一行插件的能力面（装载**之前**读到的）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectRow {
    /// 行标识（`id`，缺省时用插件名）。
    pub label: String,
    /// 插件显示名（来自 desc 或插件自己）。
    pub plugin: String,
    /// 这一行要消费的服务：**行级 inject ∪ 插件声明**（升序去重）。
    pub inject: Vec<String>,
    /// 必需依赖：插件声明 + 行级 `required`（升序去重）。
    pub inject_required: Vec<String>,
    /// 可选依赖：行级 `optional`（升序去重；与 required 冲突时归 required）。
    pub inject_optional: Vec<String>,
    /// 插件声明能安装的 key（升序去重）。
    pub provision: Vec<String>,
    /// volatile 路径声明（升序去重）。
    pub volatile: Vec<String>,
    /// 插件自述的配置 schema（合法 JSON 对象；未声明 ⇒ `None`）。
    pub schema: Option<String>,
    /// 声明了 schema 但不可展示时的原因（未声明 ⇒ `None`）。
    pub schema_error: Option<String>,
    /// artifact 路径、SHA256 与 sidecar manifest 的来源摘要。
    pub provenance: Provenance,
}

/// `inspect` 的产物。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectReport {
    pub rows: Vec<InspectRow>,
    /// **恒为 0**：inspect 不挂载（A5 的判据）。
    pub mounted: usize,
    /// 未声明 provision 的行（按行序）。
    pub undeclared: Vec<String>,
}

impl InspectReport {
    /// 打印面（`cordis inspect` 的输出；测试也断言它）。
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("mounted = {}\n", self.mounted));
        if self.undeclared.is_empty() {
            out.push_str("undeclared provision = (none)\n");
        } else {
            out.push_str(&format!(
                "undeclared provision = {}\n",
                self.undeclared.join(", ")
            ));
        }
        out.push_str(&format!(
            "capability source = {CAPABILITY_ORIGIN} (not approval)\n"
        ));
        out.push_str(&format!("inject source = {INJECT_SOURCE}\n"));
        for row in &self.rows {
            out.push_str(&format!("- {} (plugin: {})\n", row.label, row.plugin));
            row.provenance.render_lines("  ", &mut out);
            out.push_str(&format!(
                "  inject required: {}\n",
                list(&row.inject_required)
            ));
            out.push_str(&format!(
                "  inject optional: {}\n",
                list(&row.inject_optional)
            ));
            out.push_str(&format!("  provision: {}\n", list(&row.provision)));
            out.push_str(&format!("  volatile: {}\n", list(&row.volatile)));
            let schema = match (&row.schema, &row.schema_error) {
                (Some(text), _) => compact_json(text),
                (None, Some(error)) => format!("(invalid: {error})"),
                (None, None) => "(none)".to_string(),
            };
            out.push_str(&format!("  schema: {schema}\n"));
        }
        out
    }

    /// 机器可读的输出面（`cordis inspect --json`）。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "mounted": self.mounted,
            "source": CAPABILITY_ORIGIN,
            "inject_source": INJECT_SOURCE,
            "undeclared": self.undeclared,
            "rows": self.rows.iter().map(|row| serde_json::json!({
                "label": row.label,
                "plugin": row.plugin,
                "inject": {
                    "required": row.inject_required,
                    "optional": row.inject_optional,
                },
                "provision": row.provision,
                "volatile": row.volatile,
                "schema": row.schema,
                "schema_error": row.schema_error,
                "provenance": row.provenance.to_json(),
            })).collect::<Vec<_>>(),
        })
    }
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_string()
    } else {
        items.join(", ")
    }
}

/// schema 原文可能是多行；文本报告按行输出，必须压成单行 JSON。
fn compact_json(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| text.to_string())
}

/// 审阅一份配置文件的**每一行**：解析行 → 预挂载探测（[`cordis_host::probe_capabilities`]）
/// → 汇总。**一行都不挂**。
pub fn inspect(config_path: &Path) -> Result<InspectReport, String> {
    let dir = config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    inspect_in(config_path, &[dir])
}

/// [`inspect`] 的显式搜索目录版本（CLI 的 `--plugin-dir` 用）。
pub fn inspect_in(config_path: &Path, search: &[PathBuf]) -> Result<InspectReport, String> {
    let text = std::fs::read_to_string(config_path)
        .map_err(|err| format!("读取 {} 失败: {err}", config_path.display()))?;
    let entries = parse_entries(&text)?;
    inspect_entries(&entries, search)
}

/// [`inspect`] 的内核：直接给条目与搜索目录（测试与将来 `cordis validate` 共用）。
pub fn inspect_entries(entries: &[Entry], search: &[PathBuf]) -> Result<InspectReport, String> {
    let mut rows = Vec::new();
    let mut undeclared = Vec::new();
    // 与装载器共用 `stable_label`：组行已被展开，空组也不会被当成插件。
    for (index, labeled) in cordis_host::loader::labeled_entries(entries)
        .into_iter()
        .enumerate()
    {
        let entry = &labeled.entry;
        let label = labeled.label;
        let caps = cordis_host::probe_capabilities_in(Path::new(&entry.name), search)
            .map_err(|err| format!("第 {} 行（{label}）探测失败: {err}", index + 1))?;
        // 与装载器共用同一份解析：`{required, optional}` 是**声明形状**，
        // 不能把两个键名当成服务名。
        let schema = caps.schema().map(str::to_string);
        let schema_error = caps.schema_error().map(str::to_string);
        let mut required = caps.inject;
        let mut optional = Vec::new();
        if let Some(declared) = &entry.inject {
            let parsed = parse_inject(declared).map_err(|err| {
                format!("第 {} 行（{label}）的 inject 声明无效: {err}", index + 1)
            })?;
            required.extend(parsed.required.into_iter().map(|(name, _)| name));
            optional.extend(parsed.optional);
        }
        required.sort();
        required.dedup();
        optional.retain(|name| !required.contains(name));
        optional.sort();
        optional.dedup();

        let mut inject = required.clone();
        inject.extend(optional.iter().cloned());
        inject.sort();
        inject.dedup();

        // 审计面用**原始 artifact**：解析不到（例如测试注入的假源）时保留配置里的
        // 名字，`Provenance` 会明确显示 `missing/manifest`，而不是拒绝整个 inspect。
        let artifact = cordis_host::resolve_plugin_path(&entry.name, search)
            .unwrap_or_else(|_| PathBuf::from(&entry.name));
        let row = InspectRow {
            label,
            plugin: caps.plugin,
            inject,
            inject_required: required,
            inject_optional: optional,
            provision: caps.provision,
            volatile: caps.volatile,
            schema,
            schema_error,
            provenance: Provenance::from_artifact(artifact),
        };
        if row.provision.is_empty() {
            undeclared.push(row.label.clone());
        }
        rows.push(row);
    }
    Ok(InspectReport {
        rows,
        mounted: 0,
        undeclared,
    })
}

/// `validate` 的产物：审阅面 + patch 层警告。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidateReport {
    pub inspect: InspectReport,
    pub warnings: Vec<String>,
}

impl ValidateReport {
    pub fn render(&self) -> String {
        let mut out = self.inspect.render();
        if self.warnings.is_empty() {
            out.push_str("patch warnings = (none)\n");
        } else {
            out.push_str("patch warnings:\n");
            for warning in &self.warnings {
                out.push_str(&format!("- {warning}\n"));
            }
        }
        out
    }
}

/// 验证主配置 + patch 层，**不挂载任何插件**。
pub fn validate(config_path: &Path, patches: &[PathBuf]) -> Result<ValidateReport, String> {
    let base_dir = config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    validate_in(config_path, patches, &[base_dir])
}

/// [`validate`] 的显式搜索目录版本（CLI 的 `--plugin-dir` 用）。
pub fn validate_in(
    config_path: &Path,
    patches: &[PathBuf],
    search: &[PathBuf],
) -> Result<ValidateReport, String> {
    let text = std::fs::read_to_string(config_path)
        .map_err(|err| format!("读取 {} 失败: {err}", config_path.display()))?;
    let mut entries = parse_entries(&text)?;
    let base_dir = config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut warnings = Vec::new();
    for patch_path in patches {
        let path = if patch_path.is_absolute() {
            patch_path.clone()
        } else {
            base_dir.join(patch_path)
        };
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("读取 patch {} 失败: {err}", path.display()))?;
        let layer = parse_patches(&text)?;
        let mut warn = |message: &str| warnings.push(message.to_string());
        entries = apply_entry_patches(entries, &layer, &mut warn);
    }

    let inspect = inspect_entries(&entries, search)?;
    Ok(ValidateReport { inspect, warnings })
}

/// `run` 启动错误的分类。入口据此区分配置错误与运行时错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunErrorKind {
    Config,
    Runtime,
}

/// CLI 运行错误：消息 + 可供入口映射退出码的分类。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunError {
    pub kind: RunErrorKind,
    pub message: String,
}

impl RunError {
    pub fn config(message: impl Into<String>) -> Self {
        Self {
            kind: RunErrorKind::Config,
            message: message.into(),
        }
    }

    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            kind: RunErrorKind::Runtime,
            message: message.into(),
        }
    }

    /// 配置错误 → 3，运行时错误 → 4。
    pub fn exit_code(&self) -> u8 {
        match self.kind {
            RunErrorKind::Config => 3,
            RunErrorKind::Runtime => 4,
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RunError {}

/// 启动前的纯配置检查：只读、只解析，不探测、不挂载。
///
/// 这样 `RunSession::start` 能把“配置本身不合法”与“配置合法但插件启动失败”
/// 分开报给入口，后者映射为运行时退出码。
fn precheck_config(config_path: &Path, patches: &[PathBuf]) -> Result<(), RunError> {
    let text = std::fs::read_to_string(config_path)
        .map_err(|err| RunError::config(format!("读取 {} 失败: {err}", config_path.display())))?;
    parse_entries(&text).map_err(RunError::config)?;

    let base_dir = config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    for patch_path in patches {
        let path = if patch_path.is_absolute() {
            patch_path.clone()
        } else {
            base_dir.join(patch_path)
        };
        let text = std::fs::read_to_string(&path).map_err(|err| {
            RunError::config(format!("读取 patch {} 失败: {err}", path.display()))
        })?;
        parse_patches(&text).map_err(RunError::config)?;
    }
    Ok(())
}

/// `run` 的启动参数。
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub patches: Vec<PathBuf>,
    pub plugin_dir: Option<PathBuf>,
    pub strict: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            patches: Vec::new(),
            plugin_dir: None,
            strict: true,
        }
    }
}

/// 一次运行结束后的快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReport {
    /// 关闭前装载表里的行标（排序）。
    pub mounted: Vec<String>,
}

/// 装载后的诊断快照（`status` 命令）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusReport {
    pub mounted: Vec<String>,
    /// 因 `disabled`（自身或祖先）而休眠的行标。
    pub disabled: Vec<String>,
    pub pending: Vec<PendingRow>,
    /// 声明级依赖环路径（首尾同名）。
    pub cycles: Vec<Vec<String>>,
    /// 最近一次对账的逐行失败消息（`--no-strict` 下仍会返回）。
    pub failed: Vec<String>,
    /// 每个已挂载行的 artifact / SHA256 / sidecar manifest 来源摘要。
    pub provenance: Vec<RowProvenance>,
}

impl StatusReport {
    /// 失败行为空才算健康；`disabled` / `pending` / 声明级环是诊断信息，不是失败。
    pub fn is_healthy(&self) -> bool {
        self.failed.is_empty()
    }

    pub fn render(&self) -> String {
        let mut out = format!("mounted = {}\n", self.mounted.len());
        out.push_str(&format!(
            "capability source = {CAPABILITY_ORIGIN} (not approval)\n"
        ));
        out.push_str(&format!("inject source = {INJECT_SOURCE}\n"));
        if self.provenance.is_empty() {
            out.push_str("provenance = (none)\n");
        } else {
            out.push_str("provenance:\n");
            for row in &self.provenance {
                out.push_str(&format!("- {}\n", row.label));
                row.provenance.render_lines("  ", &mut out);
            }
        }
        if self.disabled.is_empty() {
            out.push_str("disabled = (none)\n");
        } else {
            out.push_str("disabled:\n");
            for label in &self.disabled {
                out.push_str(&format!("- {label}\n"));
            }
        }
        if self.cycles.is_empty() {
            out.push_str("cycles = (none)\n");
        } else {
            out.push_str("cycles:\n");
            for cycle in &self.cycles {
                out.push_str(&format!("- {}\n", cycle.join(" -> ")));
            }
        }
        if self.pending.is_empty() {
            out.push_str("pending = (none)\n");
        } else {
            out.push_str("pending:\n");
            for row in &self.pending {
                out.push_str(&format!("- {} (plugin: {})\n", row.label, row.plugin));
                for item in &row.missing {
                    out.push_str(&format!("  missing {}: {}", item.key, item.reason.as_str()));
                    if !item.related.is_empty() {
                        out.push_str(&format!(" (related: {})", item.related.join(", ")));
                    }
                    out.push('\n');
                }
            }
        }
        if self.failed.is_empty() {
            out.push_str("failed = (none)\n");
        } else {
            out.push_str("failed:\n");
            for failure in &self.failed {
                out.push_str(&format!("- {failure}\n"));
            }
        }
        out
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            // 与 `LoaderState::report()` 对齐：mounted 是计数，行标另列。
            "mounted": self.mounted.len(),
            "mounted_labels": self.mounted,
            "disabled": self.disabled,
            "source": CAPABILITY_ORIGIN,
            "inject_source": INJECT_SOURCE,
            "cycles": self.cycles,
            "provenance": self.provenance.iter().map(|row| {
                let mut value = row.provenance.to_json();
                value["label"] = serde_json::Value::String(row.label.clone());
                value
            }).collect::<Vec<_>>(),
            "pending": self.pending.iter().map(|row| serde_json::json!({
                "label": row.label,
                "plugin": row.plugin,
                "missing": row.missing.iter().map(|item| serde_json::json!({
                    "key": item.key,
                    "reason": item.reason.as_str(),
                    "related": item.related,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "failed": self.failed,
        })
    }
}

/// 已启动的运行会话：先装载，调用方决定何时关闭。
pub struct RunSession {
    app: App,
    state: Arc<LoaderState>,
}

impl RunSession {
    pub async fn start(
        config_path: &Path,
        options: RunOptions,
        source: Arc<dyn PluginSource>,
    ) -> Result<Self, RunError> {
        precheck_config(config_path, &options.patches)?;
        let app = App::new();
        let config = LoaderConfig {
            path: config_path.to_string_lossy().into_owned(),
            patches: options
                .patches
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            plugin_dir: options
                .plugin_dir
                .map(|path| path.to_string_lossy().into_owned()),
            strict: options.strict,
            strict_cycles: false,
            hmr: None,
            js_evaluator: None,
            interpolation: InterpolationMode::Builtin,
            dispatch_policy: None,
            host_limits: None,
            dependency_plan: Vec::new(),
        };
        match load_with_source(app.root.clone(), config, source).await {
            Ok(state) => Ok(Self { app, state }),
            Err(err) => {
                app.close().await;
                Err(RunError::runtime(err.to_string()))
            }
        }
    }

    pub fn mounted_labels(&self) -> Vec<String> {
        self.state.mounted_labels()
    }

    /// 装载后的未就绪诊断（只读；不改变装载状态）。
    pub fn pending_report(&self) -> Vec<PendingRow> {
        self.state.pending_report()
    }

    /// 声明级依赖环路径（只读）。
    pub fn cycles(&self) -> Vec<Vec<String>> {
        self.state.cycle_paths()
    }

    /// 关闭整棵 App 树，返回关闭前的装载面。
    pub async fn shutdown(self) -> RunReport {
        let Self { app, state } = self;
        let mounted = state.mounted_labels();
        app.close().await;
        drop(state);
        RunReport { mounted }
    }
}

/// 启动会话、等待外部关闭信号、再统一关闭。测试可注入一个立即完成的信号。
pub async fn run_with_source<F>(
    config_path: &Path,
    options: RunOptions,
    source: Arc<dyn PluginSource>,
    shutdown: F,
) -> Result<RunReport, RunError>
where
    F: Future<Output = ()>,
{
    let session = RunSession::start(config_path, options, source).await?;
    shutdown.await;
    Ok(session.shutdown().await)
}

/// 装载后立即抓取诊断并关闭；不等待外部关闭信号。
pub async fn status_with_source(
    config_path: &Path,
    options: RunOptions,
    source: Arc<dyn PluginSource>,
) -> Result<StatusReport, RunError> {
    let session = RunSession::start(config_path, options, source).await?;
    let snapshot = session.state.report();
    let mounted = session.mounted_labels();
    let disabled = string_list(&snapshot["disabled"]);
    let pending = session.pending_report();
    let cycles = session.cycles();
    let failed = string_list(&snapshot["failed"]);
    // 用挂载记录的**原始 artifact** 路径读 sidecar：暂存副本的 `.rN` 文件名旁边
    // 没有发布方写的 manifest。sidecar 缺失时保留 `missing/manifest` 状态。
    let provenance = session
        .state
        .mounted_sources()
        .into_iter()
        .map(|(label, artifact)| RowProvenance {
            label,
            provenance: Provenance::from_artifact(artifact),
        })
        .collect();
    session.shutdown().await;
    Ok(StatusReport {
        mounted,
        disabled,
        pending,
        cycles,
        failed,
        provenance,
    })
}

fn string_list(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 面向 CLI 的 `status`：真宿主磁盘源。
pub async fn status(config_path: &Path, options: RunOptions) -> Result<StatusReport, RunError> {
    status_with_source(config_path, options, disk_source()).await
}

/// 面向 CLI 的运行入口：Enter、Ctrl-C、SIGTERM 或控制台关闭事件触发优雅关闭。
///
/// 不引入 `tokio::signal`：它会在 Windows GNU 工具链上拉入
/// `windows-sys`/`dlltool`，破坏本仓当前的可移植构建。这里用平台最小的
/// 控制台/`signal` FFI，处理器只置一个原子 flag；清理仍走 `App::close`。
pub async fn run(config_path: &Path, options: RunOptions) -> Result<RunReport, RunError> {
    if let Err(err) = shutdown::install() {
        eprintln!("无法安装关闭信号处理器（仍可按 Enter 关闭）: {err}");
    }
    eprintln!("按 Enter 或 Ctrl-C 关闭…");
    let result = run_with_source(config_path, options, disk_source(), shutdown::wait()).await;
    shutdown::complete();
    result
}

/// 让 `main.rs` 也能用同一套 seam（真宿主用默认的磁盘源）。
pub fn disk_source() -> Arc<dyn PluginSource> {
    Arc::new(cordis_host::loader::DiskPluginSource)
}
