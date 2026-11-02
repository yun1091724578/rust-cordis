//! 配置驱动的插件树装载器 + 热重载。
//!
//! 对应 cordis 的 `@cordisjs/plugin-loader` + `@cordisjs/plugin-include`：
//! 读一份 `cordis.yml`，叠若干 `cordis.patch.yml`，把每条配置行变成一棵插件 fiber 树。
//!
//! # 为什么它必须在内核之外
//! 内核只认 `Plugin` 与 `inject`，完全不关心"插件从哪来"。装载器是唯一知道
//! "配置行 → 解析 → 动态库"这条链的地方，换包管理器（npm 风格解析、OCI、内置 registry）
//! 只需要换 [`crate::resolve_plugin_path`] 一个函数。
//!
//! # 热重载的两条硬约束
//! 1. **必须先卸后装。** 内核里"同一隔离域中一个服务名只有一个实现"是不变量，
//!    所以新实例的 `provide` 会在旧实例还活着时直接失败。这不是实现缺陷，
//!    而是 cordis 服务容器的定义性质——热重载因此天然是"卸载 + 重挂"，
//!    不是"热插拔"。能做的补偿是**把预检提前**（见下），而不是取消这条约束。
//! 2. **必须换文件名。** 已加载的动态库在 Windows 上被文件系统锁住，
//!    在 Linux 上覆盖已映射的 `.so` 会让进程进入未定义状态。
//!    所以新库落到 `<name>.r<修订号>.<ext>`，旧句柄继续指向旧文件。

use crate::patch::{apply_entry_patches_tracked, parse_entries, parse_patches, Entry, PatchOwners};
use crate::{mount, resolve_plugin_path, stage_plugin, LoadedPlugin, StagedFile};
use cordis_core::{
    equal_except_volatile, split_path, values_at, BoxFuture, Call, Config, Ctx, Disposable, Dyn,
    ExternValue, Fiber, FnPlugin, Inject, Level, Mode, Plugin, RealmId, Result, Ret, UpdateOutcome,
    ValidationError,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, UNIX_EPOCH};

pub const LOADER_ID: &str = "cordis:include";

/// 装载器配置。
///
/// # 键名风格是兼容性的一部分
/// cordis 的配置字段是 **camelCase**（`baseUrl` / `enableLogs` / `pluginDir`），
/// 因为它们是 TS 接口的字段名直接序列化来的。Rust 侧用默认的 snake_case
/// 会让现有配置文件里的键**静默失效**——`#[serde(default)]` 会把它悄悄变成缺省值，
/// 这是最难查的一类错误。所以这里必须显式对齐 camelCase。
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoaderConfig {
    /// 主配置文件路径（YAML 条目数组）。
    pub path: String,
    /// 依次叠加的 patch 文件路径。
    #[serde(default)]
    pub patches: Vec<String>,
    /// 插件动态库的搜索目录；缺省时用主配置文件所在目录。
    #[serde(default)]
    pub plugin_dir: Option<String>,
    /// 有任一条目装载失败时是否让装载器本身失败。
    ///
    /// 默认 `true`：cordis 的立场是 **fail loud**（harness 的 app-boot 甚至有
    /// `auditStartupEntries` 专门审计必需条目）。静默跳过一行配置，
    /// 会让人在几小时后才发现某个能力根本没装上。
    #[serde(default = "default_strict")]
    pub strict: bool,
    /// 声明级依赖环是否升级为装载失败。
    ///
    /// **默认 `false`**：A6 要求环默认只报告、不拒挂。`strict` 只负责
    /// 逐行装载失败/插值失败；把两个语义混成一个开关会让默认配置在遇到环时
    /// 直接 boot 失败。
    #[serde(default)]
    pub strict_cycles: bool,
    /// 热重载。缺省关闭——生产宿主不该默认轮询文件系统。
    #[serde(default)]
    pub hmr: Option<HmrConfig>,
    /// 宿主注入的 `!!js` 求值引擎（**可选**；缺省用内置子集 [`crate::js::BuiltinJs`]）。
    ///
    /// 不参与序列化：它是**宿主注入的能力**，不是配置文件里的内容。
    /// 注入即获全量语义（覆盖内置子集）；两者都不认的表达式一律 fail loud
    /// （绝不静默降级）——M4-4 起这条纪律的落点是"决策期预检 + `internal/config` 链"。
    #[serde(skip, default)]
    pub js_evaluator: Option<Arc<dyn JsEvaluator>>,
    /// `!!js` 的求值模式（M9，D5）。缺省 `"builtin"`。
    #[serde(default)]
    pub interpolation: InterpolationMode,
}

/// `!!js` 的求值模式（M9，D5，**P2**）。
///
/// 为什么需要它：内置子集是**本仓的兼容产物**（参考实现是 `new Function + with(ctx)`，
/// 读得到完整 ctx）。有些宿主希望"要么用我注入的引擎，要么谁都别求值"——
/// 让表达式在一个**宿主没审阅过**的解释器里跑，比报错危险得多。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InterpolationMode {
    /// **缺省**：宿主注入的引擎优先；没注入就用内置子集（M4-4a 起的行为，逐字不变）。
    #[default]
    Builtin,
    /// 只用宿主注入的引擎；没注入 ⇒ 任何 `!!js` 都 fail loud（不悄悄回落到内置子集）。
    Injected,
}

impl InterpolationMode {
    /// 报告里的字面量（`loader.report.interpolation.mode`）。
    pub fn as_str(self) -> &'static str {
        match self {
            InterpolationMode::Builtin => "builtin",
            InterpolationMode::Injected => "injected",
        }
    }
}

/// 手写 `Debug`：`Arc<dyn JsEvaluator>` 不是 `Debug`，而配置结构可打印是基本礼貌
/// （`#[derive(Debug)]` 会因此编译不过）。
impl std::fmt::Debug for LoaderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoaderConfig")
            .field("path", &self.path)
            .field("patches", &self.patches)
            .field("plugin_dir", &self.plugin_dir)
            .field("strict", &self.strict)
            .field("strict_cycles", &self.strict_cycles)
            .field("hmr", &self.hmr)
            .field(
                "js_evaluator",
                &self.js_evaluator.as_ref().map(|_| "已注入"),
            )
            .field("interpolation", &self.interpolation)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HmrConfig {
    /// 轮询间隔（毫秒）。
    ///
    /// # 为什么是轮询而不是 inotify/FSEvents
    /// 跨平台的文件通知需要额外依赖（`notify`）与各自的边缘情况（编辑器"先写临时文件
    /// 再 rename"、网络盘不触发事件、事件可能丢失）。对一个插件宿主来说，
    /// 500ms 的轮询足够，而"少一个依赖、行为在三平台完全一致"更值钱。
    /// 真要换事件驱动，只需要替换这一个函数。
    #[serde(default = "default_interval")]
    pub interval_ms: u64,
}

fn default_strict() -> bool {
    true
}

fn default_interval() -> u64 {
    500
}

/// `!!js` 求值器：定义搬到了 [`crate::js`]（内置子集与它同处一个文件，
/// 免得"支持清单"和"谁在调用"分家）。这里重新导出，保住 `loader::JsEvaluator` 这条路径。
pub use crate::js::JsEvaluator;

/// volatile 声明的组合器（M4-5）：`declare_volatile(plugin_fn(…), &["data"])`。
///
/// 定义在 [`cordis_core::declare_volatile`]（它是 `Plugin` 的组合器，与 volatile 引用
/// 同处一个概念）；这里重新导出，让**只依赖 host** 的宿主（比如 demo）也能用上。
pub use cordis_core::declare_volatile;

// 真值判断（JS 的 `Boolean(x)`）：`disabled` 位用的就是它。定义在 `crate::js`，
// 这里改名导入是为了让本文件里的调用点读起来就是"JS 真值"这件事。
use crate::js::truthy as js_truthy;

fn validate(raw: Config) -> std::result::Result<Config, ValidationError> {
    let mut issues = Vec::new();
    if raw.get("path").and_then(|v| v.as_str()).is_none() {
        issues.push("path: required (at path)".to_string());
    }
    if issues.is_empty() {
        Ok(raw)
    } else {
        Err(ValidationError { issues })
    }
}

/// 一行**来自配置的哪一层**。
///
/// 配置回写必须落到行真正所在的那一层：主配置里的行写主配置；patch 层插入的行
/// 只能写回 patch 层。把合并树写回主配置会把 patch 插入的行固化进主配置，
/// 下一次读取时 patch 又会插一遍（重复挂载、禁用行复活，P1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowOrigin {
    Main,
    Patch,
}

/// 一份已挂载记录。配置回写与热重载共用它。
#[derive(Clone)]
struct Mounted {
    label: String,
    /// 这一行来自主配置还是 patch 层（配置回写的落点选择）。
    origin: RowOrigin,
    /// 用户提供的插件文件（不会被覆盖，只被读）。
    source: PathBuf,
    /// 实际加载的那个副本（Windows 上不能原地覆盖已加载的库）。
    staged: PathBuf,
    /// `(mtime_nanos, size)`：轮询比对用的指纹。
    ///
    /// `None` = **指纹未知**（假源、非文件来源）：此时这一行**永远不算陈旧**——
    /// 把"未知"当成"永远变了"会让每个轮询周期都去重挂一次（重试风暴）。
    fingerprint: Option<(u64, u64)>,
    config: Config,
    inject: Option<Config>,
    /// 弱引用：记录不该在插件卸载后把 fiber 吊住。
    fiber: Weak<Fiber>,
    /// 这一行的**派生挂载上下文**（M4-1）：隔离域与 intercept 都在它上面。
    ///
    /// 热重载必须用它重挂——用根 ctx 重挂等于悄悄丢掉隔离域，把行搬回默认域。
    scope: Ctx,
    /// 挂载时的**行声明**（M4-3 的 diff 基准）。
    ///
    /// 只比 `name`/`inject`/`isolate`/`intercept`（身份与装配面）；`config` 以
    /// [`Mounted::config`] 为准（插件驱动的更新经提交观察者推进它，raw 声明不会跟着变）。
    declared: Entry,
    /// 挂载时的**派生上下文输入**（链上每层的 `isolate`/`intercept`）：变了就得换实例。
    scope_inputs: Vec<ScopeInput>,
    /// 这一行的插件**声明的 volatile 路径**（M4-5）：`None` = 不参与 volatile。
    ///
    /// 缓存它（而不是每次决策去问插件）有两个理由：① 对账是同步的、纯的；
    /// ② ABI 插件的声明要**跨边界查询**（`OP_VOLATILE_PATHS`），不该出现在每轮 diff 里。
    volatile: Option<Vec<String>>,
    /// 当前版本的插件对象。
    ///
    /// **持有它是为了回滚**：新版本 `apply` 失败时，旧实例已经拆掉了，
    /// 只有手上还留着这个 `Arc` 才能把它原样装回来。
    /// （另一条路是重新 `open()` 旧 `staged` 文件，但那要先等 `FreeLibrary`
    /// 把文件解锁，在 Windows 上是一次不必要的赌博——留着 Arc 就不必赌。）
    plugin: Arc<dyn Plugin>,
}

fn fingerprint(path: &std::path::Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    Some((mtime, metadata.len()))
}

/// 主配置 + patch 层的指纹（每一份各自一个；缺文件是 `None`，创建出来也算变化）。
fn config_fingerprint(files: &ConfigFiles) -> Vec<Option<(u64, u64)>> {
    std::iter::once(&files.main)
        .chain(files.patches.iter())
        .map(|path| fingerprint(path))
        .collect()
}

/// 同目录临时文件 + `rename` 的原子写。
///
/// 为什么不直接 `std::fs::write`：写到一半崩溃/被杀，磁盘上就是半截 YAML——
/// 下一次读取会解析失败（loader 保留旧快照），但**用户的数据已经坏了**。
/// rename 在同一文件系统上是原子的：读到的要么是旧文件、要么是新文件。
///
/// 内容相同直接返回；否则写临时文件并原子替换。Windows 走
/// `MoveFileExW(REPLACE_EXISTING)`，不再有"删目标再 rename"的缺失窗口。
fn write_atomically(path: &Path, text: &str) -> std::result::Result<(), String> {
    // 内容相同不写：避免无意义的 mtime 变化触发其他进程的 HMR/watcher。
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing == text {
            return Ok(());
        }
    }
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cordis.yml".to_string());
    // 每次调用一个独立临时名：提交观察者与对账可能并发写同一份文件。
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let temp = dir.join(format!(
        ".{name}.tmp{}-{}",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let mut file = std::fs::File::create(&temp)
        .map_err(|err| format!("创建临时文件 {} 失败: {err}", temp.display()))?;
    use std::io::Write;
    file.write_all(text.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|err| format!("写入临时文件 {} 失败: {err}", temp.display()))?;
    drop(file);
    replace_file(&temp, path).map_err(|err| {
        let _ = std::fs::remove_file(&temp);
        format!("替换 {} 失败: {err}", path.display())
    })
}

/// 同目录原子替换：Unix 用 `rename`，Windows 用 `MoveFileExW(REPLACE_EXISTING)`。
///
/// 不再使用“删目标 + rename”的回退：那会在目标路径上留下一个可被读者观察到的
/// 缺失窗口。替换失败时保留原目标，只清理临时文件。
#[cfg(unix)]
fn replace_file(temp: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(temp, target)
}

#[cfg(windows)]
fn replace_file(temp: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let wide =
        |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain(Some(0)).collect() };
    let from = wide(temp);
    let to = wide(target);
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
}

#[cfg(not(any(unix, windows)))]
fn replace_file(temp: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(temp, target)
}

/// 同目录 lock 文件：把 `append_atomically` 的 read-modify-write 串行化。
///
/// 这是配置落盘用的**低频**锁，不引入第三方依赖；进程崩溃留下的锁在 30 秒后
/// 视为陈旧并由下一个写入者清理。
struct ConfigLock {
    path: PathBuf,
}

impl ConfigLock {
    fn acquire(target: &Path) -> std::result::Result<Self, String> {
        let dir = target
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let name = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "cordis.yml".to_string());
        let path = dir.join(format!(".{name}.lock"));
        for _ in 0..1000 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    let _ = writeln!(
                        file,
                        "pid={} started={:?}",
                        std::process::id(),
                        std::time::SystemTime::now()
                    );
                    return Ok(Self { path });
                }
                // Windows 上另一个线程正打开/删除同名文件时，`create_new` 可能报
                // `PermissionDenied`（ERROR_ACCESS_DENIED / sharing violation），
                // 而不是 `AlreadyExists`；而且检查 `path.exists()` 时文件可能已经
                // 被对方删掉。竞争窗口内一律按"锁被占用"重试，不能把瞬时竞争
                // 误报成创建失败。
                Err(err)
                    if err.kind() == std::io::ErrorKind::AlreadyExists
                        || err.kind() == std::io::ErrorKind::PermissionDenied =>
                {
                    let stale = std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|time| time.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(30));
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(err) => {
                    return Err(format!("创建配置锁 {} 失败: {err}", path.display()));
                }
            }
        }
        Err(format!(
            "等待配置锁 {} 超时（1s）；请检查是否有进程卡在写入",
            path.display()
        ))
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        // Windows 的删除也可能撞上对方正在 create_new 的窗口（os error 5）。
        // 短重试几次：否则锁文件会留在磁盘上，下一个 acquire 只能等到 30s
        // 陈旧阈值，并发追加会被误判成超时。
        for _ in 0..100 {
            match std::fs::remove_file(&self.path) {
                Ok(()) => return,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }
}

/// 只有目标仍是**装载器上次读到的内容**时才原子替换。
///
/// 防的是“磁盘上已经有一份尚未载入的外部编辑，运行时回写却按内存旧树整份覆盖”。
/// `ConfigLock` 只串行化本进程/本代码路径的写入者，挡不住编辑器；因此读-比-写
/// 仍是尽力而为，但足以消除“编辑已落盘、watcher 还没跑到”的确定性覆盖窗口。
fn write_atomically_if_unchanged(
    path: &Path,
    text: &str,
    expected: &str,
) -> std::result::Result<(), String> {
    let _lock = ConfigLock::acquire(path)?;
    let current = match std::fs::read_to_string(path) {
        Ok(current) => current,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("读取 {} 失败: {err}", path.display())),
    };
    if current != expected {
        return Err(format!(
            "{} 已被外部修改但尚未载入；拒绝回写以免覆盖外部编辑（等待下一次重载）",
            path.display()
        ));
    }
    write_atomically(path, text)
}

/// 在文件末尾追加一段 YAML（内部同样是"整份重写 + 原子 rename"）。
///
/// 读改写全程持有同目录 lock，避免两个进程同时追加时后写者覆盖前写者。
fn append_atomically(path: &Path, text: &str) -> std::result::Result<(), String> {
    let _lock = ConfigLock::acquire(path)?;
    let mut existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("读取 {} 失败: {err}", path.display())),
    };
    if !existing.is_empty() && !existing.ends_with('\n') {
        existing.push('\n');
    }
    existing.push_str(text);
    write_atomically(path, &existing)
}

/// 读一组文件的内容（顺序：主配置 → patch 层）。
///
/// 读卡器要 `Send`：稳读的两遍之间会 `await`（睡 `settle`），future 必须还能跨线程搬运。
fn read_files(
    files: &ConfigFiles,
    read: &mut (dyn FnMut(&Path) -> std::io::Result<String> + Send),
) -> std::io::Result<FileSnapshot> {
    Ok(FileSnapshot {
        main: read(&files.main)?,
        patches: files
            .patches
            .iter()
            .map(|path| read(path))
            .collect::<std::io::Result<Vec<String>>>()?,
    })
}

/// **稳读**：同一组文件读两遍、内容一致才采纳。
///
/// 为什么不是"读一次就采纳"：编辑器保存是"写临时文件 + rename"、也有程序直接截断重写，
/// 中途被读到就是半截内容。两遍之间等 `settle`，不一致就当作"文件正在被写"，
/// 这一轮什么都不做（下一轮再看）。`read` 可注入——测试用它把"正在被写"钉成确定性的一刻。
async fn read_settled_with(
    files: &ConfigFiles,
    settle: Duration,
    read: &mut (dyn FnMut(&Path) -> std::io::Result<String> + Send),
) -> std::io::Result<Option<FileSnapshot>> {
    let first = read_files(files, read)?;
    tokio::time::sleep(settle).await;
    let second = read_files(files, read)?;
    Ok((first == second).then_some(first))
}

/// 一行未就绪（Pending）的诊断。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingRow {
    pub label: String,
    pub plugin: String,
    pub missing: Vec<cordis_core::PendingInject>,
}

/// 一次对账的产物（M4-2 的 `SyncOutcome` 在 M4-3 长成了动作表）。
///
/// 每个字段都是**行标**列表：谁被挂了、谁被卸了、谁被换了实例、谁的配置被推进了、
/// 谁的 volatile 值被原地提交了、谁被监听者短路了、谁的更新被否决了、谁失败了。
/// `report.reconcile`（M4-6）就读它。
#[derive(Debug, Default, Clone)]
pub struct ReconcileOutcome {
    /// 本次**新**挂上的行（按期望态顺序）。
    pub mounted: Vec<String>,
    /// 本次卸下的行（按字典序，便于断言）。
    pub disposed: Vec<String>,
    /// 换实例的行（`name`/`inject`/`isolate`/`intercept` 或祖先隔离域变了）。
    pub replaced: Vec<String>,
    /// 只推进配置、没有重挂的行。
    pub updated: Vec<String>,
    /// **只差 volatile 字段**的行：值原地提交，实例没动（M4-5）。
    pub volatile: Vec<String>,
    /// 动作被 `loader/patch-context` 的监听者短路（尾链没跑到）。
    pub skipped: Vec<String>,
    /// `internal/update` 被否决（`UpdateOutcome::Vetoed`）⇒ 内存里记着、实例上没落地。
    pub lagging: Vec<String>,
    /// 逐行失败消息（`<label>: <原因>`，与装载失败通道同格式）。
    pub failures: Vec<String>,
}

impl ReconcileOutcome {
    /// 真的下手了几件事（日志与 `report.reconcile.applied` 用它）。
    pub fn applied(&self) -> usize {
        self.mounted.len()
            + self.disposed.len()
            + self.replaced.len()
            + self.updated.len()
            + self.volatile.len()
    }

    /// 什么都没发生（相等 ⇒ 空计划）。
    pub fn is_noop(&self) -> bool {
        self.applied() == 0
            && self.skipped.is_empty()
            && self.lagging.is_empty()
            && self.failures.is_empty()
    }
}

/// 对账的**动作表**（M4-3 规格 3）：期望态与现状之差。
///
/// 相等 ⇒ 不产生动作（没有 `Keep`：计划里只列"要动手的事"）。
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// 期望态里有、装载表里没有。
    Mount(String),
    /// 装载表里有、期望态里没有（移除 / 被禁用 / 决策失败）。
    Dispose(String),
    /// 只有 `config` 变了：`fiber.update(config, no_save=true)`，不重挂。
    Update { label: String, config: Config },
    /// **只差 volatile 路径**（M4-5）：把新值原地提交进引用，**不重挂**、
    /// 不发 `loader/partial-dispose`、不走 `loader/patch-context`。
    ///
    /// 之所以是独立动作而不是 `Update` 的一个标志：它对上层是**另一种事**
    /// （实例继续活着），报告与测试都要看得见这个区别。
    Volatile { label: String, config: Config },
    /// 身份或上下文变了（`name`/`inject`/`isolate`/`intercept`）：先卸后装。
    Replace(String),
}

/// `loader/patch-context` 尾链的执行结果（**以尾链是否真的跑过为准**）。
#[derive(Debug, Clone, PartialEq)]
enum ActionOutcome {
    Done,
    /// `internal/update` 否决了这次更新。
    Vetoed,
    Failed(String),
}

/// 一次"文件 → 对账"的结局。
#[derive(Debug, Clone)]
pub enum ReloadOutcome {
    /// 文件正在被写（两遍读不一致）：这一轮不采纳，下一轮再看。
    Unsettled,
    /// 读取或解析失败：**保留上一快照**，只记警告（编辑中的半截文件是常态）。
    Failed(String),
    /// 已并入**在飞**的那一轮对账（只有最新的一份树会被应用）。
    Coalesced,
    /// 本次调用驱动完成的对账。
    Reconciled(ReconcileOutcome),
}

/// 装载器看到的配置文件集合（主配置 + patch 层，均为绝对路径）。
#[derive(Debug, Clone)]
struct ConfigFiles {
    main: PathBuf,
    patches: Vec<PathBuf>,
}

/// 一组配置文件的内容快照（稳读的输入与产物）。
#[derive(Debug, Clone, PartialEq)]
struct FileSnapshot {
    main: String,
    patches: Vec<String>,
}

/// 一行**派生挂载上下文的输入**：链上每层的 label + `isolate`/`intercept` 声明。
///
/// 它就是"挂载上下文"这个函数的自变量：只要它变了，行的 ctx 原型链就变了，
/// 已挂的实例必须换（本仓 `Replace` 的判据之一）。祖先的 `config`/`disabled`
/// **不在**这里——前者是子条目列表（对子行无影响），后者会以 Mount/Dispose 的形式出现在 diff 里。
#[derive(Debug, Clone, PartialEq)]
struct ScopeInput {
    label: String,
    isolate: Option<Config>,
    intercept: Option<Config>,
}

/// 装载器的运行时状态。
///
/// # 为什么它必须是显式类型
/// "运行期启停"要求**装载之后**还能改一行的 `disabled` 并立刻装卸——这需要条目树、
/// 装载表、派生域缓存与修订号落在同一处（M4-2 之前它们是 `load()` 的局部量，
/// 于是"装完就定死"）。热重载与对账状态机（M4-3）共用同一份状态。
pub struct LoaderState {
    /// 装载器自己的上下文：派生行 ctx、写日志、发事件都以它为根。
    ///
    /// 存进状态而不是每次传参：对账的公开签名（`plan`/`reconcile_once`）不该再带一个
    /// **可以传错**的 ctx——M4-2 的 `sync(&ctx)` 就是这种 footgun。
    ctx: Ctx,
    /// 归一化后的条目树（主配置 + patch 合并结果；对账的输入）。
    tree: Mutex<Vec<Entry>>,
    /// **主配置自己的**条目树（patch 叠加之前，配置回写的唯一来源）。
    ///
    /// `tree` 是合并结果，绝不能写回主配置（P1：否则 patch 插入的行被固化、
    /// 下次又插一遍 ⇒ 重复 / 禁用行复活）。热重载时随新主配置刷新。
    main_only: Mutex<Vec<Entry>>,
    /// 装载表：label → 记录。
    mounted: Mutex<HashMap<String, Mounted>>,
    /// 变更序列的**互斥闸门**（对账、同步、启停、单行重载共用）。
    ///
    /// 这些操作都会经过"读现状 → 算动作 → 改实例 → 改记录"的窗口；没有互斥，
    /// 两个窗口交错就会双挂载或让刚卸下的行复活。用 `tokio::sync::Mutex` 而不是
    /// 标准锁：它必须跨 `.await` 持有（mount/dispose 都是异步的）。
    mutation: tokio::sync::Mutex<()>,
    /// 派生域缓存（M4-1）：私有域必须跨 remount 稳定，所以它跟着状态走。
    scopes: Mutex<RowScopes>,
    revision: AtomicU64,
    /// 配置文件的真相来源（主配置 + patch 层）。
    files: ConfigFiles,
    /// 插件动态库的搜索目录。
    search: Vec<PathBuf>,
    /// 稳读的两遍之间等多久（编辑器的"写临时文件再 rename"就靠它消化）。
    settle: Duration,
    /// 主配置 + patch 层的指纹（watcher 只比对它，不必每轮解析）。
    config_fingerprint: Mutex<Vec<Option<(u64, u64)>>>,
    /// `config` / `disabled` 最后一次被哪一层 patch 覆盖（运行期回写落点）。
    owners: Mutex<PatchOwners>,
    /// 待应用的最新一份树（`request_reconcile` 的合并槽：旧的那份会被覆盖）。
    pending: Mutex<Option<Vec<Entry>>>,
    /// 是否有一轮对账在飞（重入保护）。
    reconciling: AtomicBool,
    /// 装载器**有意**卸载中的行：`internal/plugin` 观察者据此把它与"插件自我卸载"区分开。
    disposing: Mutex<HashSet<String>>,
    /// 最近一次真正执行过的对账产物（`report.reconcile` 与测试用）。
    last: Mutex<Option<ReconcileOutcome>>,
    /// 最近一次对账里**休眠**的叶子行标（M4-6：报告要能回答"我配了它，为什么没生效"）。
    disabled: Mutex<Vec<String>>,
    /// `loader.report` 出版过没有：装载期的第一次对账发生在 `provide` **之前**，
    /// 所以刷新要用这个开关挡住（`set` 一个没 provide 过的名字本来就是错误）。
    published: AtomicBool,
    /// 主配置最近一次**成功解析**时的原始文本。
    ///
    /// 运行时回写（插件自我卸载 / 配置提交）不能覆盖一份“磁盘上已改、但还没被
    /// 装载器采纳”的外部编辑；写盘前必须拿它做比较。
    main_baseline: Mutex<String>,
    /// 主配置快照解析与回写共用的一把同步闸门。
    ///
    /// 没有它，`persist` 可能在“dump 内存树”和“比较基线”之间被 `parse_snapshot`
    /// 插进来：基线推进到新文件、写回的却是旧树，外部编辑仍会被覆盖。所有
    /// `main_only` / `main_baseline` 的读写都在这里串行化（纯同步、无 await）。
    config_gate: Mutex<()>,
    source: Arc<dyn PluginSource>,
    /// **生效的**求值器（M4-4）：宿主注入的引擎，或内置子集；外面套着计数代理。
    evaluator: Arc<dyn JsEvaluator>,
    /// `!!js` 求值失败的累计次数（`loader.report.interpolation.failed`）。
    ///
    /// 累计而不是"本次对账"：它是诊断量，问的是"这套配置一共让插值失败过几次"。
    interp_failures: Arc<AtomicU64>,
    /// **生效的** `!!js` 模式（M9，D5）：报告要如实说出"这份配置现在由谁在解释"。
    interpolation_mode: InterpolationMode,
}

/// 对账驱动者的取消安全闸门。
///
/// `reconciling` 是“有一轮在飞”的单飞位；它必须由**持有驱动的那个 future**
/// 在正常结束或**被丢弃**时都复位。否则一次 `timeout` / abort 会把标志永久
/// 留在 `true`，后续 `request_reconcile` 全部返回 `None`（“已合并”），
/// 但再也没有人把 `pending` 里的树应用掉。
struct ReconcileDriver {
    state: Arc<LoaderState>,
    active: bool,
}

impl ReconcileDriver {
    fn new(state: Arc<LoaderState>) -> Self {
        Self {
            state,
            active: true,
        }
    }

    fn finish(&mut self) {
        self.active = false;
    }
}

impl Drop for ReconcileDriver {
    fn drop(&mut self) {
        if self.active {
            // 取消/panic：释放单飞位。`pending` 保留给下一次请求驱动。
            self.state.reconciling.store(false, Ordering::SeqCst);
        }
    }
}

/// 手写 `Debug`：状态里有 `Arc<dyn JsEvaluator>` 与几个锁，`#[derive]` 过不去；
/// 而"装载表现在挂着什么"正是排查时最想看的那一眼。
impl std::fmt::Debug for LoaderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoaderState")
            .field("mounted", &self.mounted_labels())
            .field("revision", &self.revision.load(Ordering::SeqCst))
            .field("config", &self.files.main)
            .field("interpolation_failed", &self.interpolation_failures())
            .finish()
    }
}

/// 计数代理（M4-4）：把"`!!js` 求值失败"这件事收成**一个数字**。
///
/// 为什么是代理而不是在各个调用点各加一次计数：失败点有三处（挂载决策的 `disabled`、
/// 决策期的 `config` 预检、`internal/config` 链上的真求值），将来还会有 M4-5 的 volatile。
/// 代理让"什么算一次插值失败"只有一个定义——`evaluate` 返回 `Err` 就算。
struct CountedEvaluator {
    inner: Arc<dyn JsEvaluator>,
    failures: Arc<AtomicU64>,
}

impl JsEvaluator for CountedEvaluator {
    fn evaluate(&self, expr: &str, ctx: &Ctx) -> std::result::Result<Config, String> {
        let result = self.inner.evaluate(expr, ctx);
        if result.is_err() {
            self.failures.fetch_add(1, Ordering::SeqCst);
        }
        result
    }
}

impl LoaderState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ctx: Ctx,
        tree: Vec<Entry>,
        main_only: Vec<Entry>,
        files: ConfigFiles,
        search: Vec<PathBuf>,
        settle: Duration,
        source: Arc<dyn PluginSource>,
        evaluator: Arc<dyn JsEvaluator>,
        interp_failures: Arc<AtomicU64>,
        interpolation_mode: InterpolationMode,
        owners: PatchOwners,
        main_baseline: String,
    ) -> Self {
        let fingerprint = config_fingerprint(&files);
        Self {
            ctx,
            tree: Mutex::new(tree),
            main_only: Mutex::new(main_only),
            mounted: Mutex::new(HashMap::new()),
            mutation: tokio::sync::Mutex::new(()),
            scopes: Mutex::new(RowScopes::default()),
            revision: AtomicU64::new(0),
            files,
            search,
            settle,
            config_fingerprint: Mutex::new(fingerprint),
            owners: Mutex::new(owners),
            pending: Mutex::new(None),
            reconciling: AtomicBool::new(false),
            disposing: Mutex::new(HashSet::new()),
            last: Mutex::new(None),
            disabled: Mutex::new(Vec::new()),
            published: AtomicBool::new(false),
            main_baseline: Mutex::new(main_baseline),
            config_gate: Mutex::new(()),
            source,
            evaluator,
            interp_failures,
            interpolation_mode,
        }
    }

    /// 当前装载表里的行标（排序）。诊断与断言都在用它——加载器不该只有日志这一种观测面。
    pub fn mounted_labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = self.mounted.lock().unwrap().keys().cloned().collect();
        labels.sort();
        labels
    }

    /// 诊断：装载表里每一行**是否指向活着的实例**。
    ///
    /// 这是对账路径的不变量：**记录先摘、后 dispose；先挂、后记录**——于是任何时刻
    /// 表里的记录都不会指向一个已经拆掉的实例（表不撒谎）。
    pub fn mounted_liveness(&self) -> Vec<(String, bool)> {
        let mut rows: Vec<(String, bool)> = self
            .mounted
            .lock()
            .unwrap()
            .iter()
            .map(|(label, record)| {
                let alive = record
                    .fiber
                    .upgrade()
                    .map(|fiber| !fiber.is_disposed())
                    .unwrap_or(false);
                (label.clone(), alive)
            })
            .collect();
        rows.sort();
        rows
    }

    /// 一行的根 fiber（诊断/测试用；没有实例时为 `None`）。
    pub fn fiber_of(&self, label: &str) -> Option<Arc<Fiber>> {
        self.mounted
            .lock()
            .unwrap()
            .get(label)
            .and_then(|record| record.fiber.upgrade())
    }

    /// 当前**插件文件已变**的行标（HMR 的重载队列，诊断/测试用）。
    ///
    /// `fingerprint` 为 `None` 的行（假源、非文件来源）永远不算陈旧——
    /// 把"未知"当成"永远变了"会让每个轮询周期都重挂一次。
    pub fn stale_labels(&self) -> Vec<String> {
        let table = self.mounted.lock().unwrap();
        table
            .values()
            .filter(|record| {
                record.fingerprint.is_some() && fingerprint(&record.source) != record.fingerprint
            })
            .map(|record| record.label.clone())
            .collect()
    }

    /// 当前条目树的快照（只读诊断；对账的输入之一）。
    pub fn tree_snapshot(&self) -> Vec<Entry> {
        self.tree.lock().unwrap().clone()
    }

    /// 最近一次真正执行过的对账产物（`report.reconcile` 与测试用）。
    pub fn last_outcome(&self) -> Option<ReconcileOutcome> {
        self.last.lock().unwrap().clone()
    }

    /// `!!js` 求值失败的累计次数（M4-4：`loader.report.interpolation.failed`）。
    pub fn interpolation_failures(&self) -> u64 {
        self.interp_failures.load(Ordering::SeqCst)
    }

    /// **声明级依赖环**（M5，A6）：每个强连通分量一条**带环路径**（首尾同名）。
    ///
    /// 图是**声明**构成的：行 A → 行 B 当" A 声明的 inject 名字" ∩ " B 声明的 provision
    /// key" 非空。运行时 `provide()` 出来的东西在挂载前不可知，不参与——
    /// 论文 Thm 73 的 acyclicity 前提也是声明级的（Lemma 77 进入 Thm 80）。
    ///
    /// **默认只报告**（本仓不做裁决）：环里的行照旧挂上，自然停在 PENDING，
    /// 由内核的依赖机制兜着。`strict_cycles` 才是本仓扩展：装载期把环升级成失败
    /// （见 [`load_with_source`]）。
    pub fn cycle_paths(&self) -> Vec<Vec<String>> {
        // 节点：已挂载的行（label、inject 名字集合、provision key 集合）。
        let mut nodes: Vec<(String, Vec<String>, Vec<String>)> = self
            .mounted
            .lock()
            .unwrap()
            .values()
            .map(|record| {
                let mut inject: Vec<String> = record
                    .plugin
                    .inject()
                    .0
                    .iter()
                    .map(|(name, _)| name.to_string())
                    .collect();
                if let Some(declared) = &record.inject {
                    collect_inject_names(declared, &mut inject);
                }
                inject.sort();
                inject.dedup();
                let mut provision = record.plugin.provision();
                provision.sort();
                provision.dedup();
                (record.label.clone(), inject, provision)
            })
            .collect();
        nodes.sort_by(|a, b| a.0.cmp(&b.0));
        let count = nodes.len();
        // 边：A → B（A 要消费的某个名字正好是 B 声明提供的）。自环也算环。
        let mut edges: Vec<Vec<usize>> = vec![Vec::new(); count];
        for (from, (_, inject, _)) in nodes.iter().enumerate() {
            for (to, (_, _, provision)) in nodes.iter().enumerate() {
                if inject
                    .iter()
                    .any(|name| provision.iter().any(|key| key == name))
                {
                    edges[from].push(to);
                }
            }
        }

        // Tarjan 的迭代版：SCC 大小 > 1，或大小 1 且有自环 ⇒ 一个环。
        // 迭代而不是递归：配置里的行数没有上限，栈溢出不该是"配置写大了"的后果。
        let mut index = vec![usize::MAX; count];
        let mut low = vec![0usize; count];
        let mut on_stack = vec![false; count];
        let mut stack: Vec<usize> = Vec::new();
        let mut next = 0usize;
        let mut cycles: Vec<Vec<String>> = Vec::new();
        for root in 0..count {
            if index[root] != usize::MAX {
                continue;
            }
            let mut work: Vec<(usize, usize)> = vec![(root, 0)];
            while let Some((node, edge)) = work.pop() {
                if edge == 0 {
                    index[node] = next;
                    low[node] = next;
                    next += 1;
                    stack.push(node);
                    on_stack[node] = true;
                }
                let mut descended = false;
                for (position, &next_node) in edges[node].iter().enumerate().skip(edge) {
                    if index[next_node] == usize::MAX {
                        work.push((node, position + 1));
                        work.push((next_node, 0));
                        descended = true;
                        break;
                    } else if on_stack[next_node] {
                        low[node] = low[node].min(index[next_node]);
                    }
                }
                if descended {
                    continue;
                }
                if low[node] == index[node] {
                    let mut component = Vec::new();
                    while let Some(top) = stack.pop() {
                        on_stack[top] = false;
                        component.push(top);
                        if top == node {
                            break;
                        }
                    }
                    component.sort_unstable();
                    let self_loop =
                        component.len() == 1 && edges[component[0]].contains(&component[0]);
                    if component.len() > 1 || self_loop {
                        let mut path: Vec<String> =
                            component.iter().map(|i| nodes[*i].0.clone()).collect();
                        path.push(path[0].clone()); // 首尾同名：一眼看出环回到谁
                        cycles.push(path);
                    }
                }
                if let Some((parent, _)) = work.last() {
                    low[*parent] = low[*parent].min(low[node]);
                }
            }
        }
        cycles.sort();
        cycles
    }

    /// **能力清单**（M5，A5 的数据面）：每行一条 inject / provision / volatile / js。
    ///
    /// 顺序 = 装载表顺序（label 升序），四项都可逐字断言。
    /// `js` 是这一行的 `!!js` 表达式**静态**读到的宿主面（M9，D5）：env 名 / 宿主事实 /
    /// `ctx.get` 的服务名——只有引用没有值（凭据绝不进清单）。
    /// 想看**挂载之前**的能力面（含还没挂的行）用
    /// [`crate::probe_capabilities`]——那是 A5 的机制。
    pub fn capabilities(&self) -> Vec<serde_json::Value> {
        // 行的 `!!js` 能力面来自**条目树**（挂载记录里只有解析后的配置，标记已经求过值）。
        let caps_by_label: std::collections::HashMap<String, crate::js::JsCapabilities> =
            crate::patch::collect_js_capabilities(&self.tree.lock().unwrap())
                .into_iter()
                .collect();
        let mut rows: Vec<serde_json::Value> = self
            .mounted
            .lock()
            .unwrap()
            .values()
            .map(|record| {
                let mut inject: Vec<String> = record
                    .plugin
                    .inject()
                    .0
                    .iter()
                    .map(|(name, _)| name.to_string())
                    .collect();
                if let Some(declared) = &record.inject {
                    collect_inject_names(declared, &mut inject);
                }
                inject.sort();
                inject.dedup();
                let mut provision = record.plugin.provision();
                provision.sort();
                provision.dedup();
                // `collect_js_capabilities` 的键是配置里的 `id`，无 id 时是裸 `name`；
                // `record.label` 是内部稳定行标（`name#N`），不能直接当键。
                let cap_key = record
                    .declared
                    .id
                    .clone()
                    .unwrap_or_else(|| record.declared.name.clone());
                let js = caps_by_label
                    .get(&cap_key)
                    .cloned()
                    .unwrap_or_default()
                    .to_json();
                json!({
                    "label": record.label,
                    "plugin": record.plugin.name(),
                    "inject": inject,
                    "provision": provision,
                    "volatile": record.volatile.clone().unwrap_or_default(),
                    "js": js,
                })
            })
            .collect();
        rows.sort_by(|a, b| a["label"].as_str().cmp(&b["label"].as_str()));
        rows
    }

    /// 当前未就绪的行及其精确缺失键（阶段 3 诊断面）。
    ///
    /// 只读、不建 fiber、不分配 realm；`related` 是全局注册表候选，
    /// 不证明在当前行的 isolate 域里可见。
    pub fn pending_report(&self) -> Vec<PendingRow> {
        let rows: Vec<(String, String, Weak<Fiber>)> = self
            .mounted
            .lock()
            .unwrap()
            .values()
            .map(|record| {
                (
                    record.label.clone(),
                    record.plugin.name().to_string(),
                    record.fiber.clone(),
                )
            })
            .collect();
        let mut pending = Vec::new();
        for (label, plugin, fiber) in rows {
            let Some(fiber) = fiber.upgrade() else {
                continue;
            };
            let missing = fiber.pending_injects();
            if missing.is_empty() {
                continue;
            }
            pending.push(PendingRow {
                label,
                plugin,
                missing,
            });
        }
        pending.sort_by(|a, b| a.label.cmp(&b.label));
        pending
    }

    /// **装载诊断面**（M4-6）：`loader.report` 的值，**每次调用都现算**。
    ///
    /// 每个字段回答一个运维问题（而不是"内部状态快照"）：
    /// - `mounted` / `disabled`：现在挂着几行、哪几行在休眠；
    /// - `volatile{declared,supported}`：多少行声明了 volatile、其中多少行现在走得了原地提交；
    /// - `interpolation{failed,mode}`：`!!js` 求值失败累计（M4-4）与**生效模式**（M9，D5）；
    /// - `reconcile{applied,skipped,lagging}`：最近一次对账下手/短路/滞后各几件；
    /// - `realms{default,named}`：默认域表条数与具名域数（两张表都只增不减，P2-14）；
    /// - `stagedLeaked`：已暂存但无人认领的副本文件数（G11，M0b）——装成功归零，
    ///   失败/泄漏 >0；
    /// - `pending`：未就绪行的精确缺失键与原因（阶段 3）；
    /// - `failed`：最近一次对账的**逐行失败消息**（不是"历史累计"——历史在日志里）。
    pub fn report(&self) -> serde_json::Value {
        let outcome = self.last.lock().unwrap().clone().unwrap_or_default();
        let (declared, supported) = self.volatile_summary();
        json!({
            "mounted": self.mounted_labels().len(),
            "disabled": self.disabled.lock().unwrap().clone(),
            "volatile": { "declared": declared, "supported": supported },
            "interpolation": {
                "failed": self.interpolation_failures(),
                "mode": self.interpolation_mode.as_str(),
            },
            "reconcile": {
                "applied": outcome.applied(),
                "skipped": outcome.skipped.len(),
                "lagging": outcome.lagging.len(),
            },
            "realms": {
                "default": self.ctx.store.default_realm_count(),
                "named": self.ctx.store.named_realm_count(),
            },
            "stagedLeaked": crate::staged_leaked(),
            "cycles": self.cycle_paths(),
            "pending": self.pending_report().into_iter().map(|row| json!({
                "label": row.label,
                "plugin": row.plugin,
                "missing": row.missing.into_iter().map(|item| json!({
                    "key": item.key,
                    "reason": item.reason.as_str(),
                    "related": item.related,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "capabilities": self.capabilities(),
            "failed": outcome.failures,
        })
    }

    /// volatile 的两个计数（M4-6）：`declared` = 有声明的**已挂载行数**；
    /// `supported` = 其中**实例还活着**的行数（现在就走得了原地提交的那种）。
    ///
    /// 两者会分开，正是要看的信号：有声明却没有活实例 ⇒ 下次对账会重挂它。
    fn volatile_summary(&self) -> (usize, usize) {
        let table = self.mounted.lock().unwrap();
        let declared = table
            .values()
            .filter(|record| record.volatile.is_some())
            .count();
        let supported = table
            .values()
            .filter(|record| record.volatile.is_some() && record.fiber.upgrade().is_some())
            .count();
        (declared, supported)
    }

    /// 把报告推给 `loader.report`（M4-6）：**每次对账后刷新**，让它是"现在的样子"。
    ///
    /// 装载期的第一次对账发生在 `provide` 之前，用 `published` 挡住；
    /// 之后用 `ctx.set`（M2-2）——`set` 换的是值、不通知依赖者，所以刷新不会引起重载风暴。
    fn publish_report(&self) {
        if !self.published.load(Ordering::SeqCst) {
            return;
        }
        // 收尾期不刷新、也不算失败：`loader.report` 的**提供者就是装载器自己**
        // （`load_with_source` 在 provide 之后才 `mark_report_published`），
        // 拆到一半（Unloading/Disposed）或依赖没齐（Pending）时这一写必然失败——
        // 那是正常时序，不是异常。只有"装载器还 ACTIVE 却写不进自己的报告"才是真问题。
        //
        // 与 `attach_self_dispose_observer` 的第 2 条判定同源：装载器自己不在 Active，
        // 整棵树就在关（或还没开），任何"发布"都没有意义。
        if self.ctx.fiber().state() != cordis_core::FiberState::Active {
            return;
        }
        if let Err(err) = self
            .ctx
            .set("loader.report", Dyn::new(ExternValue(self.report())))
        {
            self.ctx
                .log(Level::Warn, &format!("刷新 loader.report 失败: {err}"));
        }
    }

    /// 标记报告已出版（`load_with_source` 在 `provide` 之后调用）。
    pub fn mark_report_published(&self) {
        self.published.store(true, Ordering::SeqCst);
    }

    fn next_revision(&self) -> u64 {
        self.revision.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn label_of_fiber(&self, fiber: &Arc<Fiber>) -> Option<String> {
        self.mounted
            .lock()
            .unwrap()
            .values()
            .find(|record| {
                record
                    .fiber
                    .upgrade()
                    .map(|own| Arc::ptr_eq(&own, fiber))
                    .unwrap_or(false)
            })
            .map(|record| record.label.clone())
    }

    /// 运行期启停一行：`Some(true)` 禁用、`Some(false)` 显式启用、`None` 清掉选项。
    ///
    /// 语义与上游 `loader.update(entry, { disabled })` 一致
    /// （`loader/tests/group.spec.ts:52-80`）：目标是**组行**时整棵子树一起启停——
    /// 派生 disabled 沿祖先链上溯，不需要逐个子行去改。
    ///
    /// 只改内存条目树、**不落盘**：文件是真相，持久化纪律属于对账（`reconcile_once` 的
    /// 拒绝回滚 + 提交观察者的 `no_save` 分流）。
    pub async fn set_disabled(
        self: &Arc<Self>,
        label: &str,
        value: Option<bool>,
    ) -> std::result::Result<ReconcileOutcome, String> {
        // 整段"改树 → 对账"必须在互斥闸门内：否则并发的对账会中途覆盖树，
        // 刚禁用的行在下一轮里复活（或被挂成第二份）。
        let _guard = self.mutation.lock().await;
        {
            let mut tree = self.tree.lock().unwrap();
            let entry = find_by_label(&mut tree, label)
                .ok_or_else(|| format!("没有 label 为 \"{label}\" 的配置行"))?;
            entry.disabled = value.unwrap_or(false);
            // 静态选项覆盖表达式：上游把选项设成布尔值时，表达式形态被整体替换。
            entry.disabled_expr = None;
        }
        Ok(self.sync_inner().await)
    }

    /// **纯差异**：不下手，只列动作（`plan_reaches_fresh_load_state` 与可视化用它）。
    pub fn plan(&self, entries: &[Entry]) -> Vec<Action> {
        let desired = self.desired(entries);
        self.diff(&desired.rows)
    }

    /// 以**当前条目树**为目标对账一次（运行期启停走它）。
    pub async fn sync(self: &Arc<Self>) -> ReconcileOutcome {
        let _guard = self.mutation.lock().await;
        self.sync_inner().await
    }

    /// [`sync`] 的无锁内核：调用方已经持有 `mutation`。
    async fn sync_inner(self: &Arc<Self>) -> ReconcileOutcome {
        let entries = self.tree_snapshot();
        self.reconcile_once_inner(entries).await
    }

    /// 以**给定的新树**为目标对账一次（文件驱动的唯一驱动器）。
    pub async fn reconcile_once(self: &Arc<Self>, entries: Vec<Entry>) -> ReconcileOutcome {
        let _guard = self.mutation.lock().await;
        self.reconcile_once_inner(entries).await
    }

    /// [`reconcile_once`] 的无锁内核：调用方已经持有 `mutation`。
    async fn reconcile_once_inner(self: &Arc<Self>, entries: Vec<Entry>) -> ReconcileOutcome {
        // 期望态与动作表**先算出来**：这一步会求值 `!!js`、派生上下文（可能分配私有域）。
        let desired = self.desired(&entries);
        let actions = self.diff(&desired.rows);
        // 采纳新树：**意图**先落地（文件是真相）。执行失败不回滚树——下一轮对账会带着
        // 同一份目标重试，把它慢慢推到位；回滚树反而会让"文件说的"和"树说的"长期分叉。
        *self.tree.lock().unwrap() = entries;
        // 休眠行进报告（M4-6）：用户问的"为什么没生效"，答案通常就在这张表里。
        *self.disabled.lock().unwrap() = desired.disabled;
        let mut rows: HashMap<String, FlatRow> = desired
            .rows
            .into_iter()
            .map(|row| (row.label.clone(), row))
            .collect();

        let mut outcome = ReconcileOutcome {
            failures: desired
                .issues
                .into_iter()
                .map(|(label, issue)| format!("{label}: {issue}"))
                .collect(),
            ..ReconcileOutcome::default()
        };
        // 决策失败（没有求值器 / 求值报错 / `config` 位置仍有未求值表达式）要**吵**：
        // 静默跳过一行等于让配置悄悄撒谎（M1-4 立的红线，这里延续到对账路径）。
        for failure in &outcome.failures {
            self.ctx.log(Level::Error, &format!("对账：{failure}"));
        }
        for action in actions {
            match action {
                Action::Dispose(label) => {
                    // 移除路径只有 emit（上游 `EntryGroup.remove` 不走 `patch-context`）。
                    self.emit_partial_dispose(rows.get(&label), &label, false)
                        .await;
                    self.dispose_row(&label).await;
                    self.ctx
                        .log(Level::Info, &format!("{label}: 已卸载（不再期望挂载）"));
                    outcome.disposed.push(label);
                }
                Action::Mount(label) => {
                    let Some(row) = rows.remove(&label) else {
                        continue;
                    };
                    match self.patch_context(&row, PatchWork::Mount).await {
                        Some(ActionOutcome::Done) => {
                            self.ctx.log(Level::Info, &format!("{label}: 已挂载"));
                            outcome.mounted.push(label);
                        }
                        Some(ActionOutcome::Vetoed) | None => outcome.skipped.push(label),
                        Some(ActionOutcome::Failed(err)) => {
                            self.ctx
                                .log(Level::Error, &format!("{label}: 挂载失败 {err}"));
                            outcome.failures.push(format!("{label}: {err}"));
                        }
                    }
                }
                Action::Replace(label) => {
                    let Some(row) = rows.remove(&label) else {
                        continue;
                    };
                    // 先卸后装：同一隔离域里一个服务名只能有一个实现（内核不变量）。
                    self.emit_partial_dispose(Some(&row), &label, true).await;
                    self.dispose_row(&label).await;
                    match self.patch_context(&row, PatchWork::Mount).await {
                        Some(ActionOutcome::Done) => {
                            self.ctx.log(Level::Info, &format!("{label}: 已换实例"));
                            outcome.replaced.push(label);
                        }
                        Some(ActionOutcome::Vetoed) | None => outcome.skipped.push(label),
                        Some(ActionOutcome::Failed(err)) => {
                            self.ctx
                                .log(Level::Error, &format!("{label}: 换实例失败 {err}"));
                            outcome.failures.push(format!("{label}: {err}"));
                        }
                    }
                }
                Action::Update { label, config } => {
                    let Some(row) = rows.remove(&label) else {
                        continue;
                    };
                    // 上游 `entry.ts:130`：更新路径同样先发 partial-dispose（`by_update=true`），
                    // 再走 `patch-context` 的尾链去执行 `fiber.update`。
                    self.emit_partial_dispose(Some(&row), &label, true).await;
                    match self.patch_context(&row, PatchWork::Update(config)).await {
                        Some(ActionOutcome::Done) => outcome.updated.push(label),
                        // 否决：内存记录照旧推进，实例上没落地 ⇒ 记进 lagging（M4-6 报告）。
                        Some(ActionOutcome::Vetoed) => outcome.lagging.push(label),
                        None => outcome.skipped.push(label),
                        Some(ActionOutcome::Failed(err)) => {
                            self.ctx
                                .log(Level::Error, &format!("{label}: 配置更新失败 {err}"));
                            outcome.failures.push(format!("{label}: {err}"));
                        }
                    }
                }
                Action::Volatile { label, config } => {
                    let Some(row) = rows.remove(&label) else {
                        continue;
                    };
                    // M4-5：这条路径**不发** `loader/partial-dispose`、**不走** `patch-context`
                    // （上游 `entry.ts:150-153` 把它整个跳过）——实例不重挂，就没什么"半拆"可言。
                    match self.commit_volatile(&label, &config).await {
                        Ok(Some(changed)) => {
                            self.ctx.log(
                                Level::Info,
                                &format!(
                                    "{label}: volatile 值已原地提交（{}）",
                                    changed.join(", ")
                                ),
                            );
                            outcome.volatile.push(label);
                        }
                        // 运行期前置条件不成立（实例已不在 ACTIVE / 候选期间重载过 /
                        // 普通值也变了）：交回普通更新（上游"`_commitVolatile` 返回 false"）。
                        Ok(None) => {
                            self.ctx.log(
                                Level::Info,
                                &format!("{label}: volatile 前置条件不成立，改走普通更新"),
                            );
                            self.emit_partial_dispose(Some(&row), &label, true).await;
                            match self.patch_context(&row, PatchWork::Update(config)).await {
                                Some(ActionOutcome::Done) => outcome.updated.push(label),
                                Some(ActionOutcome::Vetoed) => outcome.lagging.push(label),
                                None => outcome.skipped.push(label),
                                Some(ActionOutcome::Failed(err)) => {
                                    self.ctx
                                        .log(Level::Error, &format!("{label}: 配置更新失败 {err}"));
                                    outcome.failures.push(format!("{label}: {err}"));
                                }
                            }
                        }
                        // 候选无效 / 推送失败：**保留运行值**，实例落后于配置（上游 warn + 不重挂）。
                        // raw 已经推进（下次激活用它），所以这里不是失败、是 `lagging`。
                        Err(issue) => {
                            self.ctx.log(
                                Level::Warn,
                                &format!("{label}: volatile 提交未生效，保留运行值：{issue}"),
                            );
                            outcome.lagging.push(label);
                        }
                    }
                }
            }
        }
        *self.last.lock().unwrap() = Some(outcome.clone());
        // M4-6：报告是**实时**的——每轮对账后刷新（`reconcile.*`/`failed`/`disabled` 才有意义）。
        self.publish_report();
        outcome
    }

    /// 请求一次对账（watcher 每轮只调它）：**合并 + 重入保护**。
    ///
    /// 已经有一轮在飞时直接返回 `None`——那一轮结束前会顺手把**最新**的那份树带走，
    /// 于是"排队期间来的旧树"根本不会被应用（上游 `_pendingTree` 的同一语义）。
    pub async fn request_reconcile(
        self: &Arc<Self>,
        entries: Vec<Entry>,
    ) -> Option<ReconcileOutcome> {
        *self.pending.lock().unwrap() = Some(entries);
        if self.reconciling.swap(true, Ordering::SeqCst) {
            return None;
        }
        // 单飞位由这个 future 持有；被 timeout/abort 丢弃时必须复位。
        let mut driver = ReconcileDriver::new(self.clone());
        let mut last = None;
        loop {
            // 先把要应用的树取出来（guard 只活在这一句）：`match` 的临时量会活到整个
            // match 结束，那样 guard 就跨过了下面的 `.await`，整个 future 不再是 `Send`。
            let next = self.pending.lock().unwrap().take();
            match next {
                Some(entries) => {
                    // 合并逻辑归 `request_reconcile`，但**落地**必须与其它变更序列互斥：
                    // 否则运行期启停会与这一轮对账交错，把刚卸下的行挂回去。
                    let outcome = {
                        let _guard = self.mutation.lock().await;
                        self.reconcile_once_inner(entries).await
                    };
                    last = Some(outcome);
                }
                None => {
                    self.reconciling.store(false, Ordering::SeqCst);
                    // 关门前再确认一次：刚好在 take 与关门之间放进来的那份不能被落下，
                    // 否则它要等到下一次文件变动才会被应用（幽灵滞后）。
                    if self.pending.lock().unwrap().is_some()
                        && !self.reconciling.swap(true, Ordering::SeqCst)
                    {
                        continue;
                    }
                    break;
                }
            }
        }
        driver.finish();
        last
    }

    /// 一条龙：稳读文件 → 解析（含 patch 层）→ 请求对账。
    ///
    /// 失败路径**一个字节都不动状态**：解析失败保留上一快照（编辑中的半截文件是常态），
    /// 稳读不通过就下一轮再看。
    pub async fn reload(self: &Arc<Self>) -> ReloadOutcome {
        let snapshot = match self.read_settled().await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return ReloadOutcome::Unsettled,
            Err(err) => {
                return ReloadOutcome::Failed(format!(
                    "读取 {} 失败: {err}",
                    self.files.main.display()
                ))
            }
        };
        match self.parse_snapshot(&snapshot) {
            Ok(entries) => match self.request_reconcile(entries).await {
                Some(outcome) => ReloadOutcome::Reconciled(outcome),
                None => ReloadOutcome::Coalesced,
            },
            Err(err) => ReloadOutcome::Failed(err),
        }
    }

    /// 期望态：以 `entries` 为目标、用**这次的**求值器答案算出来的"应当挂着的行"。
    fn desired(&self, entries: &[Entry]) -> Desired {
        let mut scopes = self.scopes.lock().unwrap();
        let decision = RowDecision {
            evaluator: self.evaluator.as_ref(),
        };
        desired_rows(entries, &decision, &self.ctx, &mut scopes)
    }

    /// 现状 vs 期望态 ⇒ 动作表。
    fn diff(&self, desired: &[FlatRow]) -> Vec<Action> {
        let mounted = self.mounted.lock().unwrap();
        let wanted: HashSet<&str> = desired.iter().map(|row| row.label.as_str()).collect();
        let mut removed: Vec<String> = mounted
            .keys()
            .filter(|label| !wanted.contains(label.as_str()))
            .cloned()
            .collect();
        removed.sort();
        let mut actions: Vec<Action> = removed.into_iter().map(Action::Dispose).collect();
        for row in desired {
            let Some(record) = mounted.get(&row.label) else {
                actions.push(Action::Mount(row.label.clone()));
                continue;
            };
            let config = row.entry.config.clone().unwrap_or(Config::Null);
            if record.scope_inputs != row.scope_inputs
                || record.declared.name != row.entry.name
                || record.declared.inject != row.entry.inject
                || record.declared.isolate != row.entry.isolate
                || record.declared.intercept != row.entry.intercept
            {
                // 身份或上下文变了：本仓走 replace（正确性超集；上游不重挂，附录 A）。
                actions.push(Action::Replace(row.label.clone()));
            } else if record.config != config {
                // M4-5：**只差 volatile 路径** ⇒ 原地提交，不重挂。
                //
                // 这里的比较用**原始**配置（对应上游 `equalExceptVolatile(legacy.config,
                // this.options.config, schema)`）：声明是唯一依据——没声明的插件永远走
                // 普通更新。`!!js` 与默认值带来的差别留到执行期的"候选比较"再判。
                let volatile_only = record.volatile.as_ref().is_some_and(|paths| {
                    let segments: Vec<Vec<String>> =
                        paths.iter().map(|path| split_path(path)).collect();
                    equal_except_volatile(&record.config, &config, &segments)
                });
                if volatile_only {
                    actions.push(Action::Volatile {
                        label: row.label.clone(),
                        config,
                    });
                } else {
                    actions.push(Action::Update {
                        label: row.label.clone(),
                        config,
                    });
                }
            }
        }
        actions
    }

    /// 发 `loader/partial-dispose`（`emit`，不阻塞）：监听者用来做可视化/清缓存。
    async fn emit_partial_dispose(&self, row: Option<&FlatRow>, label: &str, by_update: bool) {
        let args = vec![Dyn::new(ExternValue(json!({
            "label": label,
            "by_update": by_update,
            "entry": row.map(|row| row.entry.to_value()),
        })))];
        match row {
            Some(row) => row.scope.emit("loader/partial-dispose", args).await,
            None => self.ctx.emit("loader/partial-dispose", args).await,
        }
    }

    /// 在**行的上下文**上跑 `loader/patch-context` 瀑布，尾链就是要执行的动作。
    ///
    /// 返回 `None` 表示**尾链没被跑到**：监听者短路了整个动作（记账为 `skipped`）。
    /// 这正是 waterfall "tail=动作" 的意思（上游 `entry.ts:102-109` 把
    /// `fiber.update`/`init` 放在 `waterfall('loader/patch-context', …)` 的尾链里）。
    async fn patch_context(
        self: &Arc<Self>,
        row: &FlatRow,
        work: PatchWork,
    ) -> Option<ActionOutcome> {
        let action = match &work {
            PatchWork::Mount => "mount",
            PatchWork::Update(_) => "update",
        };
        let slot: Arc<Mutex<Option<ActionOutcome>>> = Arc::new(Mutex::new(None));
        let tail_slot = slot.clone();
        let state = self.clone();
        let owned = row.clone();
        let tail: BoxFuture<Ret> = Box::pin(async move {
            let outcome = match work {
                PatchWork::Mount => match state.mount_row(&owned).await {
                    Ok(record) => {
                        state
                            .mounted
                            .lock()
                            .unwrap()
                            .insert(owned.label.clone(), record);
                        ActionOutcome::Done
                    }
                    Err(err) => ActionOutcome::Failed(err),
                },
                PatchWork::Update(config) => match state.fiber_of(&owned.label) {
                    Some(fiber) => match fiber.update_with(config, true).await {
                        Ok(UpdateOutcome::Restarted) => {
                            // 记录跟着走：`no_save=true` 的提交已经由观察者推进过内存记录，
                            // 这里再对齐一次，免得 diff 下一轮又看到"配置变了"。
                            if let Some(record) =
                                state.mounted.lock().unwrap().get_mut(&owned.label)
                            {
                                record.config = owned.entry.config.clone().unwrap_or(Config::Null);
                            }
                            ActionOutcome::Done
                        }
                        Ok(UpdateOutcome::Vetoed | UpdateOutcome::Superseded) => {
                            ActionOutcome::Vetoed
                        }
                        Err(err) => ActionOutcome::Failed(err.to_string()),
                    },
                    None => ActionOutcome::Failed("装载表里没有可用实例".into()),
                },
            };
            *tail_slot.lock().unwrap() = Some(outcome);
            Some(Dyn::new(ExternValue(
                json!({ "label": owned.label, "action": action }),
            )))
        });
        let args = vec![Dyn::new(ExternValue(json!({
            "label": row.label,
            "action": action,
            "entry": row.entry.to_value(),
        })))];
        row.scope
            .waterfall("loader/patch-context", args, Some(tail), None)
            .await;
        let outcome = slot.lock().unwrap().take();
        outcome
    }

    /// 一次 **volatile 原地提交**（M4-5）。三态：
    ///
    /// - `Ok(Some(changed))`：提交完成（`changed` = 值真的变了的路径）；
    /// - `Ok(None)`：**前置条件不成立** ⇒ 调用方走普通更新；
    /// - `Err(issue)`：**候选无效**（解析失败）⇒ 保留运行值，实例落后于配置。
    ///
    /// 顺序逐条对齐上游 `entry.ts:149-153` + `_commitVolatile`（`:164-197`）：
    /// ① raw 先推进（"下次激活用新的"，即使候选无效也推进）；② 候选 = 在行的 ctx 上
    /// 重新解析（`!!js` 会跟着当前上下文再算一次）；③ 实例必须还活着（ACTIVE）且
    /// **解析期间没有重载过**（单飞 + 陈旧判定：候选算完才发现实例换了代，
    /// 那就不是"原地提交"而是"给旧实例写值"）；④ 普通值必须没变；⑤ 推给 ABI 侧
    /// （原生插件是空操作）⇒ ⑥ 写引用 ⇒ ⑦ 发 `loader/volatile-update`（owner-filter）。
    async fn commit_volatile(
        self: &Arc<Self>,
        label: &str,
        config: &Config,
    ) -> std::result::Result<Option<Vec<String>>, String> {
        let Some(fiber) = self.fiber_of(label) else {
            return Ok(None);
        };
        // 声明与插件对象一起取（一次锁）：两者都是这次提交要用的。
        let Some((paths, plugin)) = self.mounted.lock().unwrap().get(label).and_then(|record| {
            record
                .volatile
                .clone()
                .map(|paths| (paths, record.plugin.clone()))
        }) else {
            return Ok(None);
        };
        if fiber.state() != cordis_core::FiberState::Active {
            return Ok(None);
        }
        // ① raw 推进 + 记录推进：文件是真相，下一次激活拿新的。
        fiber.set_raw_config(config.clone());
        if let Some(record) = self.mounted.lock().unwrap().get_mut(label) {
            record.config = config.clone();
        }

        // ② 候选（会走 `internal/config` 链 + 插件的校验器）。
        let epoch_before = fiber.running_epoch();
        let candidate = fiber
            .resolve_raw(config)
            .await
            .map_err(|err| format!("候选解析失败: {err}"))?;

        // ③ 陈旧判定：解析期间实例换过代（依赖变了 / 被更新了）⇒ 这次候选不适用于"原地提交"。
        if fiber.state() != cordis_core::FiberState::Active || fiber.running_epoch() != epoch_before
        {
            return Ok(None);
        }

        // ④ 普通值必须没变（对应 `deepEqual(fiber.config, candidate, true)` 里
        //    "volatile 引用恒相等"那条规则：我们按声明路径把两边都抹平再比）。
        let segments: Vec<Vec<String>> = paths.iter().map(|path| split_path(path)).collect();
        if !equal_except_volatile(&fiber.resolved(), &candidate, &segments) {
            return Ok(None);
        }

        // ⑤ 先推给插件（ABI 侧的真值在插件里：推不动就**什么也不提交**）。
        let values = values_at(&candidate, &paths);
        plugin
            .apply_volatile(&fiber.ctx(), &values)
            .map_err(|err| format!("推送失败: {err}"))?;

        // ⑥ 写引用（身份不变、值变）。⑦ 有变化才发事件。
        let changed = fiber.commit_volatile(candidate);
        if !changed.is_empty() {
            self.emit_volatile_update(&fiber, &changed).await;
        }
        Ok(Some(changed))
    }

    /// 发 `loader/volatile-update`（M4-5）：载荷 = **变化的路径**，
    /// 且只到这一行的 fiber 子树（上游 `entry.ts:188-191` 的动态 filter）。
    async fn emit_volatile_update(self: &Arc<Self>, fiber: &Arc<Fiber>, changed: &[String]) {
        let args = vec![Dyn::new(ExternValue(json!(changed)))];
        let filter = self.ctx.store.fiber_filter(fiber);
        self.ctx
            .store
            .dispatch(
                Mode::Emit,
                "loader/volatile-update",
                &fiber.ctx(),
                args,
                None,
                filter.as_ref(),
            )
            .await;
    }

    /// 有意卸载一行：**先摘记录**（表不撒谎），再 dispose（清理逻辑可能回调宿主）。
    ///
    /// `disposing` 是给 `internal/plugin` 观察者看的：有意的卸载不该被误判成
    /// "插件自我卸载"。上游靠 `fiber.entry.disabled` 区分，这里用显式标记——
    /// 更稳，因为"禁用"只是有意卸载的一种原因（还有移除与换实例）。
    async fn dispose_row(&self, label: &str) {
        self.disposing.lock().unwrap().insert(label.to_string());
        let record = self.mounted.lock().unwrap().remove(label);
        if let Some(record) = record {
            if let Some(fiber) = record.fiber.upgrade() {
                fiber.dispose().await;
            }
        }
        self.disposing.lock().unwrap().remove(label);
    }

    /// 插件**自己**把这一行卸载了（上游 `loader/src/index.ts:137-145` 的 case 7）。
    ///
    /// 处理：从装载表摘掉 + 树侧标 `disabled: true` + **落盘**。不落盘的话，
    /// 下一次读取配置时这一行会悄悄回来——用户看到的现象是"我明明卸掉了它，重启后又活了"。
    ///
    /// # 落盘必须落到行**真正所在的那一层**（P1）
    /// `self.tree` 是主配置 + patch 的合并结果。把它写回主配置会把 patch 插入的行
    /// 固化进主配置（下次读取时 patch 再插一遍 ⇒ 重复），还会把 patch 的改动
    /// 写进错误的文件。所以：主配置里的行走 [`persist`]；patch 插入的行走
    /// [`persist_overlay`]（追加一条 overlay 到最后一个 patch 文件）。
    fn mark_self_disposed(&self, label: &str) {
        let record = self.mounted.lock().unwrap().remove(label);
        // 合并树（内存）先标上：下一轮对账不会把它再挂回来。
        let marked = {
            let mut tree = self.tree.lock().unwrap();
            match find_by_label(&mut tree, label) {
                Some(entry) => {
                    entry.disabled = true;
                    entry.disabled_expr = None;
                    entry.clone()
                }
                None => return,
            }
        };
        let origin = record
            .map(|record| record.origin)
            .unwrap_or(RowOrigin::Main);
        // 字段级归属优先：主配置行也可能被 patch 的 `disabled` 覆盖。
        let owner_layer = marked
            .id
            .as_deref()
            .and_then(|id| self.owners.lock().unwrap().disabled_layer(id));
        let result = match owner_layer {
            Some(layer) => self.persist_overlay(layer, label, &marked, None, Some(true)),
            None => match origin {
                RowOrigin::Main => {
                    let mut main = self.main_only.lock().unwrap();
                    match find_by_label(&mut main, label) {
                        Some(entry) => {
                            entry.disabled = true;
                            entry.disabled_expr = None;
                            drop(main);
                            self.persist()
                        }
                        // 记录说它是主配置行、主树里却找不到（label 漂移等）：
                        // 退到 patch 层，**绝不**把合并树写进主配置。
                        None => {
                            drop(main);
                            self.persist_overlay_last(label, &marked, None, Some(true))
                        }
                    }
                }
                RowOrigin::Patch => self.persist_overlay_last(label, &marked, None, Some(true)),
            },
        };
        match result {
            Ok(()) => self.ctx.log(
                Level::Info,
                &format!("{label}: 插件自我卸载，已在配置里标为 disabled 并回写"),
            ),
            Err(err) => self.ctx.log(
                Level::Error,
                &format!("{label}: 插件自我卸载，但配置回写失败: {err}"),
            ),
        }
    }

    /// 这一行来自主配置还是 patch 层（按"主树里有没有同一个 label"判定）。
    ///
    /// label 只由结构决定（[`stable_label`]），而 patch 的 `insert` 只会**追加**
    /// （根末尾 / 组末尾），不影响既有行的编号——所以主树上的同一 label
    /// 就是"这一行来自主配置"的可靠判据。
    fn origin_of(&self, label: &str) -> RowOrigin {
        let mut main = self.main_only.lock().unwrap();
        match find_by_label(&mut main, label) {
            Some(_) => RowOrigin::Main,
            None => RowOrigin::Patch,
        }
    }

    /// 把**主配置树**落盘（配置回写统一走这里）。
    ///
    /// **绝不**写 `self.tree`：它是主配置 + patch 合并后的结果（P1 的修复点）。
    fn persist(&self) -> std::result::Result<(), String> {
        // 与 parse_snapshot 串行：dump 的树和比较用的基线必须来自同一代。
        let _gate = self.config_gate.lock().unwrap();
        let text = {
            let main = self.main_only.lock().unwrap();
            crate::patch::dump_entries(&main)?
        };
        let baseline = self.main_baseline.lock().unwrap().clone();
        write_atomically_if_unchanged(&self.files.main, &text, &baseline)?;
        // 只有写成功才推进基线；失败时磁盘上的外部编辑原样保留。
        *self.main_baseline.lock().unwrap() = text;
        self.refresh_config_fingerprint();
        Ok(())
    }

    /// 把一条 overlay 追加到**指定的 patch 层**。
    ///
    /// - 有 `id`：追加 `- id: <id>` + `disabled`/`config`，下次解析时由该层生效；
    /// - 无 `id`：无从定位，**拒绝写盘**并报错（调用方记 Error 日志）。
    fn persist_overlay(
        &self,
        layer: usize,
        label: &str,
        row: &Entry,
        config: Option<&Config>,
        disabled: Option<bool>,
    ) -> std::result::Result<(), String> {
        let Some(id) = row.id.as_deref() else {
            return Err(format!(
                "行 \"{label}\" 来自 patch 层且没有 id，无法定位回写目标（拒绝写盘）"
            ));
        };
        let Some(patch) = self.files.patches.get(layer) else {
            return Err(format!(
                "行 \"{label}\" 的 patch 归属层 {layer} 不存在（配置层数可能在运行期变化，拒绝写盘）"
            ));
        };
        let text = crate::patch::dump_overlay(id, config, disabled)?;
        append_atomically(patch, &text)?;
        self.refresh_config_fingerprint();
        Ok(())
    }

    /// 兼容没有字段归属信息的旧路径：写到最后一个 patch 层。
    fn persist_overlay_last(
        &self,
        label: &str,
        row: &Entry,
        config: Option<&Config>,
        disabled: Option<bool>,
    ) -> std::result::Result<(), String> {
        let Some(layer) = self.files.patches.len().checked_sub(1) else {
            return Err(format!(
                "行 \"{label}\" 不在主配置里，且没有 patch 层可写（拒绝写盘）"
            ));
        };
        self.persist_overlay(layer, label, row, config, disabled)
    }

    /// 写盘之后同步文件指纹：否则 watcher 会把**我们自己写的内容**当成外部变更，
    /// 下一轮再触发一次对账。
    fn refresh_config_fingerprint(&self) {
        *self.config_fingerprint.lock().unwrap() = config_fingerprint(&self.files);
    }

    /// **稳读**：同一组文件读两遍、内容一致才采纳（编辑中的半截文件不采纳）。
    async fn read_settled(&self) -> std::io::Result<Option<FileSnapshot>> {
        let mut read = |path: &Path| std::fs::read_to_string(path);
        read_settled_with(&self.files, self.settle, &mut read).await
    }

    /// 从快照解析条目树：主配置 + 依次叠 patch 层（语义与装载时**完全同一条**路径）。
    ///
    /// 顺手刷新 [`LoaderState::main_only`]——它是配置回写的唯一来源。刷新放在
    /// **全部解析成功之后**：半截文件解析失败时，上一份主树必须原样保留。
    fn parse_snapshot(&self, snapshot: &FileSnapshot) -> std::result::Result<Vec<Entry>, String> {
        let main = parse_entries(&snapshot.main)?;
        let mut entries = main.clone();
        let mut owners = PatchOwners::default();
        for (index, text) in snapshot.patches.iter().enumerate() {
            let patches = parse_patches(text).map_err(|err| {
                format!(
                    "解析 patch {} 失败: {err}",
                    self.files.patches[index].display()
                )
            })?;
            let ctx = self.ctx.clone();
            let mut warn = |message: &str| ctx.log(Level::Warn, message);
            entries = apply_entry_patches_tracked(entries, &patches, index, &mut owners, &mut warn);
        }
        // 只有**全部解析成功**才进入提交段。解析本身可以在闸门外做；这里必须与
        // `persist` 串行，避免回写看到“树已换、基线未换”（或反之）的半新半旧状态。
        let _gate = self.config_gate.lock().unwrap();
        *self.main_only.lock().unwrap() = main;
        *self.owners.lock().unwrap() = owners;
        *self.main_baseline.lock().unwrap() = snapshot.main.clone();
        Ok(entries)
    }

    /// 挂载一条展平后的行：加载（经 [`PluginSource`]）→ 挂载 → 收敛。
    ///
    /// `row.scope` 是这一行的**派生上下文**（M4-1：隔离域与 intercept 已按祖先链叠加）；
    /// 用根 ctx 挂等于把行悄悄搬回默认域。
    async fn mount_row(&self, row: &FlatRow) -> std::result::Result<Mounted, String> {
        let loaded = self
            .source
            .load(&row.entry, &self.search, self.next_revision())?;
        let fiber = mount(
            &row.scope,
            loaded.plugin.clone(),
            row.entry.config.clone().unwrap_or(Config::Null),
            row.entry.inject.clone(),
        );
        fiber.settle().await;
        if let Some(error) = fiber.error() {
            // `apply` 失败留下的 fiber 仍然占着 runtime 表（`fibers_by_plugin_name` 看得见），
            // 它的 effect 也没跑过清理。不在这里拆掉，每次失败挂载都会漏一条半成品 fiber。
            fiber.dispose().await;
            return Err(error);
        }
        // volatile 声明在**装载成功之后**取（对应 §8 M4-5 的"paths 在挂载成功后查询"）：
        // 对 ABI 插件这是一次跨边界查询，早于 apply 问也可能得到"还没准备好"的答案。
        let volatile = volatile_of(fiber.volatile_paths());
        let origin = self.origin_of(&row.label);
        Ok(Mounted {
            label: row.label.clone(),
            origin,
            source: loaded.source,
            staged: loaded.staged,
            fingerprint: loaded.fingerprint,
            config: row.entry.config.clone().unwrap_or(Config::Null),
            inject: row.entry.inject.clone(),
            fiber: Arc::downgrade(&fiber),
            plugin: loaded.plugin,
            scope: row.scope.clone(),
            declared: row.entry.clone(),
            scope_inputs: row.scope_inputs.clone(),
            volatile,
        })
    }
}

/// `loader/patch-context` 尾链要干的事（M4-3 规格 4）。
enum PatchWork {
    Mount,
    Update(Config),
}

/// 一次挂载决策（M4-2）：一行的"该不该挂" = **求值过的**派生 disabled。
///
/// 上游 `Entry.disabled` 是 getter：沿祖先链上溯、每次读都重新求值
/// （`entry.ts:70-85`）。这里保留同样的形状——**没有缓存**，
/// 于是上下文变了，下一次决策就会给出新的答案。
///
/// M4-4 起求值器**恒存在**（宿主引擎或内置子集，外面套着计数代理）：
/// "没有求值器"这个状态没有了，"表达式算不出来"成了唯一的失败态。
struct RowDecision<'a> {
    evaluator: &'a dyn JsEvaluator,
}

impl RowDecision<'_> {
    /// 派生 disabled：自身或**任一祖先**为真即禁用。
    ///
    /// 链上的 `!!js` 表达式用**这一行**的派生 ctx 求值（上游 `this.evaluate(...)` 同理）：
    /// `disabled: !!js "!ctx.get('profileContext')"` 读的是行自己的上下文。
    /// 求值失败 ⇒ `Err`（fail loud，绝不静默当成 `false`）。
    fn disabled(
        &self,
        chain: &[(String, Entry)],
        scope: &Ctx,
    ) -> std::result::Result<bool, String> {
        for (label, entry) in chain {
            if entry.disabled {
                return Ok(true);
            }
            if let Some(expr) = &entry.disabled_expr {
                let source = expr
                    .expression()
                    .map_err(|err| format!("{err} (at {label}.disabled)"))?;
                let value = self.evaluator.evaluate(&source, scope).map_err(|err| {
                    format!("!!js 求值失败: {source} (at {label}.disabled): {err}")
                })?;
                if js_truthy(&value) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

/// 创建装载器插件。
pub fn loader() -> Arc<dyn Plugin> {
    Arc::new(FnPlugin {
        name: LOADER_ID,
        key: cordis_core::unique_key(LOADER_ID),
        inject: Inject::default(),
        validator: Some(Arc::new(validate)),
        f: Arc::new(|ctx: Ctx, config: Config| {
            Box::pin(async move {
                let config: LoaderConfig =
                    serde_json::from_value(config).map_err(|err| err.to_string())?;
                load(ctx, config).await
            })
        }),
    })
}

/// 一次成功装载的产物（[`PluginSource`] 的返回）。
pub struct LoadedFile {
    pub plugin: Arc<dyn Plugin>,
    /// 用户提供的插件文件（HMR 指纹用；假实现可以给一个假路径）。
    pub source: PathBuf,
    /// 实际加载的副本（staged `.rN`；假实现同 `source`）。
    pub staged: PathBuf,
    pub fingerprint: Option<(u64, u64)>,
}

/// "一行配置 → 一个可用插件实例"这条链的抽象（M4-1 的 seam）。
///
/// 真实实现是磁盘上的动态库（resolve → stage → open → ping，见 [`DiskPluginSource`]）；
/// 测试用假实现直接给 `Arc<dyn Plugin>`——于是隔离域、intercept、label 稳定化
/// 这些**装载层语义**都能在没有动态库的情况下端到端测试。
pub trait PluginSource: Send + Sync {
    fn load(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        revision: u64,
    ) -> std::result::Result<LoadedFile, String>;
}

/// 真实实现：磁盘上的动态库。预检（ABI 版本、结构尺寸、入口可调用性）全在
/// 动到正在跑的实例之前完成。
pub struct DiskPluginSource;

impl PluginSource for DiskPluginSource {
    fn load(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        revision: u64,
    ) -> std::result::Result<LoadedFile, String> {
        let path = resolve_plugin_path(&entry.name, search).map_err(|err| err.to_string())?;
        let source_fingerprint = fingerprint(&path);
        let staged = stage_plugin(&path, revision).map_err(|err| err.to_string())?;
        let plugin = LoadedPlugin::open_staged(&staged).map_err(|err| err.to_string())?;
        // 预检：ABI 版本、结构尺寸、插件入口可调用性，全在动到正在跑的实例之前完成。
        // （探活失败时 `plugin` 在此 drop：先 FreeLibrary、后删暂存文件——不会泄漏。）
        if !plugin.ping() {
            return Err("ABI 探活失败（版本不匹配，或入口无法调用）".into());
        }
        Ok(LoadedFile {
            plugin: Arc::new(plugin),
            source: path,
            staged,
            fingerprint: source_fingerprint,
        })
    }
}

async fn load(ctx: Ctx, config: LoaderConfig) -> Result<()> {
    load_with_source(ctx, config, Arc::new(DiskPluginSource))
        .await
        .map(|_| ())
}

/// [`load`] 的可注入版本：测试用假 [`PluginSource`] 顶替动态库加载。
///
/// 返回 [`LoaderState`]：装载器不是"装完就定死"——运行期启停（[`LoaderState::set_disabled`]）
/// 与对账（[`LoaderState::reconcile_once`] / [`LoaderState::reload`]）都要在这个状态上继续做。
pub async fn load_with_source(
    ctx: Ctx,
    config: LoaderConfig,
    source: Arc<dyn PluginSource>,
) -> Result<Arc<LoaderState>> {
    let main_path = PathBuf::from(&config.path);
    let base_dir = main_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    // 1) 读主配置。主树单独留一份：配置回写只能写它（patch 合并结果写回主配置会把
    //    patch 插入的行固化下来，见 P1）。
    let text = std::fs::read_to_string(&main_path)
        .map_err(|err| format!("读取 {} 失败: {err}", main_path.display()))?;
    let main_entries = parse_entries(&text)?;
    let mut entries = main_entries.clone();

    // 2) 依次叠加 patch 层。语义与 `applyEntryPatches` 完全一致，纯函数、可单测。
    //    路径在这里就解析成**绝对路径**：watcher 与对账之后每一轮都要按同一份清单读文件。
    let patches: Vec<PathBuf> = config
        .patches
        .iter()
        .map(|patch| {
            if PathBuf::from(patch).is_absolute() {
                PathBuf::from(patch)
            } else {
                base_dir.join(patch)
            }
        })
        .collect();
    let mut owners = PatchOwners::default();
    for (layer, patch_path) in patches.iter().enumerate() {
        let text = std::fs::read_to_string(patch_path)
            .map_err(|err| format!("读取 patch {} 失败: {err}", patch_path.display()))?;
        let patches = parse_patches(&text)?;
        let mut warn = |message: &str| ctx.log(Level::Warn, message);
        entries = apply_entry_patches_tracked(entries, &patches, layer, &mut owners, &mut warn);
    }

    // 3) 运行时状态。插件默认从主配置所在目录解析，可被 pluginDir 覆盖。
    let search: Vec<PathBuf> = match &config.plugin_dir {
        Some(dir) => vec![PathBuf::from(dir)],
        None => vec![base_dir.clone()],
    };
    // 稳读窗口取轮询周期的四分之一（下限 10ms）：编辑器的"写临时文件再 rename"
    // 通常比这快得多，于是既不会误采纳半截文件，也不会让人等一个完整周期。
    let settle = config
        .hmr
        .as_ref()
        .map(|hmr| Duration::from_millis((hmr.interval_ms / 4).max(10)))
        .unwrap_or_else(|| Duration::from_millis(50));
    // `!!js` 的生效引擎（M4-4 + M9）：先看模式，再看宿主注入了什么。
    // - `builtin`（缺省）：注入的引擎**覆盖**内置子集；没注入就用内置子集。
    // - `injected`：只用注入的引擎；没注入 ⇒ `RejectInjected`（每个表达式 fail loud）。
    // 外面统一套一层计数代理：三次失败点（决策期 `disabled`、决策期 `config` 预检、
    // 链上真求值）都走同一个数字出口。
    let interp_failures = Arc::new(AtomicU64::new(0));
    let injected = config.js_evaluator.clone();
    let inner: Arc<dyn JsEvaluator> = match (config.interpolation, injected) {
        (InterpolationMode::Builtin, Some(evaluator))
        | (InterpolationMode::Injected, Some(evaluator)) => evaluator,
        (InterpolationMode::Builtin, None) => Arc::new(crate::js::BuiltinJs),
        (InterpolationMode::Injected, None) => Arc::new(crate::js::RejectInjected),
    };
    let evaluator: Arc<dyn JsEvaluator> = Arc::new(CountedEvaluator {
        inner,
        failures: interp_failures.clone(),
    });
    let interpolation_mode = config.interpolation;
    let state = Arc::new(LoaderState::new(
        ctx.clone(),
        entries,
        main_entries,
        ConfigFiles {
            main: main_path,
            patches,
        },
        search,
        settle,
        source,
        evaluator,
        interp_failures,
        interpolation_mode,
        owners,
        text,
    ));

    // 4) `!!js` 的插值挂上 `internal/config` 链（M4-4b）——**必须在第一次对账之前**：
    //    首挂那一行的配置解析就要走它，否则原始标记会流进插件。
    attach_interpolation(&ctx, state.clone());

    // 5) 挂载 = **对账一次**：初次装载与热重载/运行期启停走同一条码路，不存在两套语义。
    //    `!!js` 的处置也归这里：`disabled` 与 `config` 两个位置都**现算**（内置子集或
    //    宿主注入的引擎）；算不出来的行不挂，并逐行报出（fail loud）。
    //    静默把表达式当默认值用，等于让配置悄悄撒谎——这是 M1-4 起的红线。
    let outcome = state.sync().await;
    let failures: Vec<String> = outcome.failures;

    // 6) 三条链路挂在同一份状态上：配置回写、自我卸载识别、配置/插件文件监听。
    attach_commit_writer(&ctx, state.clone());
    attach_self_dispose_observer(&ctx, state.clone());
    if let Some(hmr) = &config.hmr {
        attach_watcher(&ctx, state.clone(), hmr.interval_ms);
    }

    // 7) 暴露装载结果，便于上层做启动审计（对应 harness 的 `auditStartupEntries`）。
    //    报告的形状与刷新纪律见 [`LoaderState::report`]（M4-6）：装载时出版一次，
    //    之后**每轮对账**都用 `ctx.set` 刷新——读它的人永远看到"现在的样子"。
    ctx.provide("loader.report", Dyn::new(ExternValue(state.report())))?;
    state.mark_report_published();

    // fail loud：静默跳过一行配置，会让人在几小时后才发现某个能力根本没装上。
    // 错误信息必须带上**每一行的具体失败原因**，否则"fail loud"就退化成了一句口号。
    //
    // `strict=false` 的降级不是"闭嘴"：每一行都要有一条警告日志（§8 M4-4 的失败策略），
    // 报告里的 `failed` 也照旧列着——只是不再把启动整个拖下去。
    if !failures.is_empty() {
        if config.strict {
            return Err(format!(
                "{} 个条目装载失败（strict 模式；把 strict 设为 false 可降级为警告）:\n  - {}",
                failures.len(),
                failures.join("\n  - ")
            )
            .into());
        }
        for failure in &failures {
            ctx.log(Level::Warn, &format!("降级（strict=false）：{failure}"));
        }
    }

    // 声明级依赖环（M5）：**默认只报告**（环里的行照旧挂上，自然停在 PENDING）；
    // `strict_cycles` 才是本仓扩展——把环升级成装载失败，错误里带**环路径**。
    // 注意这条与上面的"逐行失败"是两件事：行都挂上了，是**声明**互相咬住了。
    let cycles = state.cycle_paths();
    if config.strict_cycles && !cycles.is_empty() {
        let paths: Vec<String> = cycles.iter().map(|path| path.join(" → ")).collect();
        return Err(format!(
            "声明级依赖环（strictCycles 模式；把 strictCycles 设为 false 可降级为报告）:\n  - {}",
            paths.join("\n  - ")
        )
        .into());
    }
    for path in &cycles {
        ctx.log(
            Level::Warn,
            &format!("声明级依赖环（只报告，不拒挂）：{}", path.join(" → ")),
        );
    }
    Ok(state)
}

/// 从行级 `inject` 声明里收集**必需依赖名**。
///
/// 可选依赖不参与声明级依赖环：它们不阻塞装载，也不该把一条软依赖变成
/// 装载期的硬环。形状解析与 [`crate::patch::parse_inject`] 共用一份，避免
/// `{required, optional}` 被当成两个服务名。
fn collect_inject_names(declared: &Config, out: &mut Vec<String>) {
    let Ok(parsed) = crate::patch::parse_inject(declared) else {
        return;
    };
    for (name, _) in parsed.required {
        out.push(name);
    }
}

/// 装载层的隔离域记账（M4-1）。
///
/// - **行私有域**（`isolate: {svc: true}`）：按**声明行的稳定 label** 分配一次并缓存，
///   跨 remount 稳定（上游把它挂在条目身份上，`isolate.ts:49-57` 的 LocalRealm）；
/// - **具名共享域**（`isolate: {svc: "<label>"}`）：委托 [`Store::named_realm`]
///   ——跨行共享、只增不减；
/// - realm 只存在这里与 `Mounted`，**绝不写进 `Entry`**：否则 `to_value` 会把内部编号
///   泄进 yml，`PartialEq` 也会被它触发成"配置变了"。
#[derive(Default)]
pub struct RowScopes {
    private: HashMap<(String, String), RealmId>,
}

impl RowScopes {
    /// 为一行的**祖先链（含自身，根在前）**派生挂载上下文：
    /// 从根往叶子逐层叠加 `isolate` 与 `intercept`；后代的同名条目覆盖祖先。
    pub fn scope_ctx(&mut self, parent: &Ctx, chain: &[(String, Entry)]) -> Ctx {
        let mut ctx = parent.clone();
        for (label, entry) in chain {
            if let Some(Config::Object(isolate)) = &entry.isolate {
                for (name, value) in isolate {
                    let realm = match value {
                        // `true` = 行私有域（按声明行缓存，跨 remount 稳定）。
                        Config::Bool(true) => self.private_realm(&ctx, label, name),
                        // 字符串 = 具名共享域（跨行共享，不回收）。
                        Config::String(shared) => ctx.store.named_realm(name, shared),
                        // 其余形态不是合法 isolate 声明（规格只有 `true` 与字符串标签）。
                        _ => continue,
                    };
                    ctx = ctx.isolate(name, Some(realm));
                }
            }
            if let Some(Config::Object(intercept)) = &entry.intercept {
                for (name, config) in intercept {
                    ctx = ctx.intercept(name, config.clone());
                }
            }
        }
        ctx
    }

    fn private_realm(&mut self, ctx: &Ctx, label: &str, name: &str) -> RealmId {
        let key = (label.to_string(), name.to_string());
        if let Some(realm) = self.private.get(&key) {
            return *realm;
        }
        // 用 `alloc_realm` 而不是 `named_realm`：私有域的键是**行身份**，
        // 且不应与具名域共享命名空间。
        let realm = ctx.store.alloc_realm();
        self.private.insert(key, realm);
        realm
    }
}

/// 递归插值：把配置树里的 `!!js` 标记换成**真实值**（上游 `config/utils.ts:12-22`）。
///
/// 两处调用（同一个函数，判据因此只有一套）：
/// - **决策期预检**（[`DesiredWalk::visit`]）：只为"能不能算"与"错误里带路径"，
///   结果不落地——真正的值由下一处给出；
/// - **`internal/config` 链**（[`attach_interpolation`]）：每行 fiber 解析配置时现算，
///   算出来的才是插件真正收到的值。于是"上下文变了，表达式就重新答一次"这条性质
///   与 `disabled` 位完全一致。
///
/// `path` 是**给人看的路径**（`a.config.b[2].k`）：配置里出错的地方和出错的原因一样重要。
pub(crate) fn interpolate_value(
    value: &Config,
    path: &str,
    ctx: &Ctx,
    evaluator: &dyn JsEvaluator,
) -> std::result::Result<Config, String> {
    if let Some(expr) = crate::patch::JsExpr::from_value(value) {
        let source = expr
            .expression()
            .map_err(|err| format!("{err} (at {path})"))?;
        return evaluator
            .evaluate(&source, ctx)
            .map_err(|err| format!("!!js 求值失败: {source} (at {path}): {err}"));
    }
    match value {
        Config::Array(list) => {
            let mut out = Vec::with_capacity(list.len());
            for (index, item) in list.iter().enumerate() {
                out.push(interpolate_value(
                    item,
                    &format!("{path}[{index}]"),
                    ctx,
                    evaluator,
                )?);
            }
            Ok(Config::Array(out))
        }
        Config::Object(object) => {
            let mut out = serde_json::Map::with_capacity(object.len());
            for (key, item) in object {
                out.insert(
                    key.clone(),
                    interpolate_value(item, &format!("{path}.{key}"), ctx, evaluator)?,
                );
            }
            Ok(Config::Object(out))
        }
        other => Ok(other.clone()),
    }
}

/// 这次配置解析是不是**我的一条配置行**（M4-4b：插值只对行生效）。
///
/// 上游的条件是 `this.entry && this.parent.fiber?.entry === this.entry`
/// （`loader/src/index.ts:104-113`）：行 fiber 才插值，行**里面**再挂出来的子插件
/// 保留调用方的配置身份（harness 的 `app-boot`/`cmdline` 就是靠这一条把自己的
/// 配置块原样传给子插件的）。
///
/// 本仓没有"挂在 fiber 上的 Entry"，用的是 ctx 的**结构**判据：
/// 从这条 fiber 的父 ctx 上溯，**跳过没有 fiber 的派生 ctx**（行 ctx 的隔离域/intercept
/// 链都是派生出来的、不带 fiber），第一个带 fiber 的祖先必须是装载器自己的 ctx。
///
/// 为什么不用"挂载时登记 fiber → label 表"：fiber 的首次配置解析跑在一个被 spawn 的
/// 任务里（`Store::plugin`），多线程运行时上它和 `mount_row` 的下一行没有先后关系——
/// 登记晚一步，插值就被跳过，配置里会**留下原始标记**。结构判据没有这个窗口：
/// 父链在 fiber 诞生的那一刻就是确定的。
fn mounted_on_loader(loader_ctx: &Ctx, fiber: &Arc<Fiber>) -> bool {
    let Some(loader_fiber) = loader_ctx.fiber_opt() else {
        return false;
    };
    let mut cursor = fiber.parent_ctx();
    while let Some(ctx) = cursor {
        if let Some(owner) = ctx.fiber_opt() {
            return Arc::ptr_eq(&owner, &loader_fiber);
        }
        cursor = ctx.parent.clone();
    }
    false
}

/// `internal/config` 链上的插值（M4-4b）：**每行 fiber 解析配置时现算**。
///
/// 顺序与上游一致（`loader/src/index.ts:104-113`）：先让内层（含 tail = raw）跑完，
/// 再对**结果**插值——这样别的 `internal/config` 监听者改过的值也在插值范围内。
///
/// 求值失败 ⇒ 返回 `None`：`Store::resolve_config` 把"瀑布被中断"当**错误**
/// （M4-4 同批收紧；原来会静默回落 raw，让原始标记流进插件）。失败细节（路径 + 原文）
/// 已经写进错误日志，并计入 `loader.report.interpolation.failed`。
fn attach_interpolation(ctx: &Ctx, state: Arc<LoaderState>) {
    let loader_ctx = ctx.clone();
    ctx.on_with(
        "internal/config",
        move |call: Call| {
            let state = state.clone();
            let loader_ctx = loader_ctx.clone();
            Box::pin(async move {
                let value = call.next.call(call.args.clone()).await;
                let Some(config) = value
                    .as_ref()
                    .and_then(|value| value.downcast::<Config>())
                    .map(|config| (*config).clone())
                else {
                    return value;
                };
                let Some(fiber) = call.args.first().and_then(|arg| arg.downcast::<Fiber>()) else {
                    return Some(Dyn::new(config));
                };
                if !mounted_on_loader(&loader_ctx, &fiber) {
                    return Some(Dyn::new(config));
                }
                // 路径用**行标**（已在装载表里时）；首挂那一刻记录还没建，退回"这一行的配置"。
                let root = state
                    .label_of_fiber(&fiber)
                    .map(|label| format!("{label}.config"))
                    .unwrap_or_else(|| "config".to_string());
                match interpolate_value(&config, &root, &call.ctx, state.evaluator.as_ref()) {
                    Ok(interpolated) => Some(Dyn::new(interpolated)),
                    Err(issue) => {
                        state
                            .ctx
                            .log(Level::Error, &format!("{}: {issue}", fiber.name()));
                        None
                    }
                }
            })
        },
        false,
        true,
    );
}

/// 配置回写：插件调用 `fiber.update(config)` 时把新配置落回文件。
///
/// 这条链路就是文档里"配置即稳定 ABI"的落点——用户改的是 **patch 层**，
/// 所以升级宿主时用户的改动天然存活。
///
/// M3-1：`internal/commit` **总是**发出，`no_save` 作为载荷到达——
/// 无论哪种情况都先推进内存记录（`mounted` 表 + 条目树），
/// 只有 `no_save == false` 才落盘（上游是干脆不发 commit，见附录 A）。
fn attach_commit_writer(ctx: &Ctx, state: Arc<LoaderState>) {
    ctx.store
        .clone()
        .on_commit(ctx, move |fiber, config, no_save| {
            let found = {
                let table = state.mounted.lock().unwrap();
                table
                    .values()
                    .find(|record| record.fiber.upgrade().map(|f| f.uid()) == Some(fiber.uid()))
                    .map(|record| (record.label.clone(), record.origin))
            };
            let Some((label, origin)) = found else { return };
            if let Some(record) = state.mounted.lock().unwrap().get_mut(&label) {
                record.config = config.clone();
            }
            let tree_entry = {
                let mut tree = state.tree.lock().unwrap();
                match find_by_label(&mut tree, &label) {
                    Some(entry) => {
                        entry.config = Some(config.clone());
                        Some(entry.clone())
                    }
                    None => None,
                }
            };
            if no_save {
                // 只推进记录，不落盘（文件驱动的更新是这条路：文件已经是真相）。
                return;
            }
            let Some(tree_entry) = tree_entry else { return };
            // 字段级归属优先：主配置行若被 patch 覆盖 config，必须写回那一层，
            // 否则下一次 reload 会被 patch 再覆盖回去。
            let owner_layer = tree_entry
                .id
                .as_deref()
                .and_then(|id| state.owners.lock().unwrap().config_layer(id));
            let result = match owner_layer {
                Some(layer) => {
                    state.persist_overlay(layer, &label, &tree_entry, Some(config), None)
                }
                None => match origin {
                    RowOrigin::Main => {
                        let mut main = state.main_only.lock().unwrap();
                        match find_by_label(&mut main, &label) {
                            Some(entry) => {
                                entry.config = Some(config.clone());
                                drop(main);
                                state.persist()
                            }
                            None => {
                                drop(main);
                                state.persist_overlay_last(&label, &tree_entry, Some(config), None)
                            }
                        }
                    }
                    RowOrigin::Patch => {
                        state.persist_overlay_last(&label, &tree_entry, Some(config), None)
                    }
                },
            };
            if let Err(err) = result {
                state
                    .ctx
                    .log(Level::Error, &format!("{label}: 配置回写失败: {err}"));
            }
        });
}

/// 观察 fiber 的诞生与处置：识别"插件把自己卸载了"（M4-3 规格 7）。
///
/// # 判定链（每一环都在挡一类误报）
/// 1. `!fiber.is_disposed()` ⇒ 诞生事件，不看；
/// 2. 装载器自己的 fiber 已处置 ⇒ 整棵树正在关，不看（否则每行都会被标 disabled）；
/// 3. 这个 fiber 必须是**某条已挂载配置行的根 fiber**（子插件、别人的 fiber 直接放过）；
/// 4. 这一行正在被装载器**有意**卸载（`disposing`：禁用/移除/换实例/HMR）⇒ 不看。
///
/// # 为什么必须"有意/无意"分得这么细
/// 误判的代价是不对称的：漏判 ⇒ 用户手动卸载的插件重启后又活了（可容忍）；
/// 误判 ⇒ **一次正常的配置更新把插件永久标成 disabled 并写进文件**（灾难，且自我强化：
/// 下次读取时它还是禁用的）。
fn attach_self_dispose_observer(ctx: &Ctx, state: Arc<LoaderState>) {
    let loader_ctx = ctx.clone();
    ctx.store.clone().on_plugin(ctx, move |fiber| {
        if !fiber.is_disposed() {
            return;
        }
        if loader_ctx.fiber().is_disposed() {
            return;
        }
        let Some(label) = state.label_of_fiber(fiber) else {
            return;
        };
        if state.disposing.lock().unwrap().contains(&label) {
            return;
        }
        state.mark_self_disposed(&label);
    });
}

/// 热重载监听：一轮里看两件事——**配置文件**变了没有、**插件文件**变了没有。
///
/// 配置这一侧：指纹变了才去稳读（`reload` 自己会再读两遍）→ 解析失败**保留上一快照**
/// （编辑中的半截文件是常态，只有警告）→ 成功则请求一次对账（`request_reconcile`
/// 自带合并与重入保护，所以这里可能会 Coalesced）。
///
/// 插件这一侧：比对每个已挂载行的 `source` 指纹 → 变了就"换名重挂"（M1-2 的事务语义）。
///
/// 用 `Weak<Context>` 而不是 `Arc<Context>`，卸载后任务会在下一轮自然退出，
/// 不会把整棵上下文树吊住（内核靠清空 `Fiber.ctx` 打破引用环，外部持强引用会抵消它）。
fn attach_watcher(ctx: &Ctx, state: Arc<LoaderState>, interval_ms: u64) {
    ctx.effect(move |ctx| {
        let weak = ctx.downgrade();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let state = state.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(interval_ms.max(50))).await;
                    if stop_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Some(ctx) = weak.upgrade() else { break };
                    if ctx.fiber().is_disposed() {
                        break;
                    }

                    // ① 配置文件变了？
                    let current = config_fingerprint(&state.files);
                    let changed = current != *state.config_fingerprint.lock().unwrap();
                    if changed {
                        match state.reload().await {
                            // 文件正在被写：**不推进指纹**，下一轮再看同一份文件。
                            ReloadOutcome::Unsettled => {}
                            ReloadOutcome::Failed(err) => {
                                // 解析失败保留上一快照；推进指纹避免每轮刷同一条警告
                                // （文件不变就不会再解析，文件一变指纹就又会不同）。
                                *state.config_fingerprint.lock().unwrap() = current;
                                ctx.log(Level::Warn, &format!("配置文件未采纳（保留上一快照）: {err}"));
                            }
                            ReloadOutcome::Coalesced => {
                                *state.config_fingerprint.lock().unwrap() = current;
                            }
                            ReloadOutcome::Reconciled(outcome) => {
                                *state.config_fingerprint.lock().unwrap() = current;
                                if !outcome.is_noop() {
                                    ctx.log(
                                        Level::Info,
                                        &format!(
                                            "配置对账：应用 {} 项（挂 {} 卸 {} 换 {} 更新 {}）跳过 {} 滞后 {} 失败 {}",
                                            outcome.applied(),
                                            outcome.mounted.len(),
                                            outcome.disposed.len(),
                                            outcome.replaced.len(),
                                            outcome.updated.len(),
                                            outcome.skipped.len(),
                                            outcome.lagging.len(),
                                            outcome.failures.len(),
                                        ),
                                    );
                                }
                            }
                        }
                    }

                    // ② 插件文件变了？
                    let stale = state.stale_labels();
                    for label in stale {
                        if let Err(err) = reload_one(&ctx, &state, &label).await {
                            ctx.log(Level::Error, &format!("热重载 {label} 失败: {err}"));
                        }
                    }
                }
            });
        }
        Disposable::sync(move || stop.store(true, Ordering::SeqCst))
    });
}

/// 一次"换名重挂"的结果（M1-2）。
///
/// 三态让"非法状态不可表达"：回滚成功与"新旧都失败"必须被调用方区分对待。
enum SwapOutcome {
    /// 新实例已上线。
    Ok {
        fiber: Arc<Fiber>,
        plugin: Arc<dyn Plugin>,
    },
    /// 新版本 apply 失败，已回滚到旧实例（`restored` 即当前在服务的 fiber）。
    RolledBack { error: String, restored: Arc<Fiber> },
    /// 回滚也失败：该条目当前**没有**可用实例，需要人工介入。
    Lost {
        error: String,
        rollback_error: String,
    },
}

impl SwapOutcome {
    /// 诊断用短名（日志与测试消息）。
    #[cfg_attr(not(test), allow(dead_code))]
    fn describe(&self) -> &'static str {
        match self {
            SwapOutcome::Ok { .. } => "Ok(新实例已上线)",
            SwapOutcome::RolledBack { .. } => "RolledBack(已回滚到旧实例)",
            SwapOutcome::Lost { .. } => "Lost(新旧实例均不可用)",
        }
    }

    /// 需要向调用方/日志报告的错误（成功时为 None）。
    fn error_message(&self) -> Option<&str> {
        match self {
            SwapOutcome::Ok { .. } => None,
            SwapOutcome::RolledBack { error, .. } => Some(error),
            SwapOutcome::Lost { error, .. } => Some(error),
        }
    }
}

impl Mounted {
    /// 把一次换入的结果落进记录；返回 `true` 表示该条目应当从装载表移除（`Lost`）。
    ///
    /// 不变量（M1-2）：记录必须**始终指向当前在服务的实例**：
    /// - `Ok`：推进到新实例（`staged` / `plugin` / `fiber` 三件套）；
    /// - `RolledBack`：把 `fiber` 指回 `restored`——**这就是修复本身**：
    ///   否则记录仍指着已 dispose 的旧实例，下一次重载会与幽灵实例冲突（服务名撞车
    ///   ⇒ 新实例 provide 失败 ⇒ 回滚也失败）；
    /// - `Lost`：条目没有可用实例，调用方应把它移出装载表并告警。
    fn commit_outcome(&mut self, outcome: SwapOutcome, staged: Option<PathBuf>) -> bool {
        self.fingerprint = fingerprint(&self.source);
        match outcome {
            SwapOutcome::Ok { fiber, plugin } => {
                self.fiber = Arc::downgrade(&fiber);
                // M4-5：volatile 声明是**插件版本**的一部分——新插件可能声明了另一组路径。
                // 缓存不跟着换，下一次 diff 会拿旧路径去判"只差 volatile"，把一次
                // 需要重挂的更新误判成原地提交（或反过来）。
                self.volatile = volatile_of(plugin.volatile_paths());
                self.plugin = plugin;
                if let Some(staged) = staged {
                    self.staged = staged;
                }
                false
            }
            SwapOutcome::RolledBack { restored, .. } => {
                self.fiber = Arc::downgrade(&restored);
                // 回滚后线上的是**旧**插件：声明缓存必须跟着退回去。
                self.volatile = volatile_of(restored.volatile_paths());
                false
            }
            SwapOutcome::Lost { .. } => true,
        }
    }
}

/// 空声明 = 不参与 volatile（与挂载路径的口径一致）。
fn volatile_of(paths: Vec<String>) -> Option<Vec<String>> {
    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

/// 就地把 `record` 指向的实例换成 `new_plugin`；**换失败则回滚到 `record.plugin`**。
///
/// # 为什么这里必须有事务语义
/// 预检（`ping`）只挡得住"新库加载不起来"。`apply` 阶段的失败——配置校验不过、
/// 依赖服务不存在、插件自己返回 `Err`——只有走到这里才发现，而那时旧实例**已经拆掉了**。
/// 不回滚就等于"一次失败的自我修改让能力凭空消失"。
///
/// 这正是论文 §5.2.2 Algorithm 10 在装载器层的对应物：
/// *"The transaction guarantees that the system never enters a half-reloaded state."*
/// 论文那版要 `backup ← invalidate_caches(...)` / `restore_caches(backup)`，
/// 是因为 JS 的模块系统有缓存要搬运；Rust 这边动态库没有那一层，
/// 手上留着的 `record.plugin` 就是"备份"本身，回滚只是把它重新 `mount` 一次。
///
/// 返回 [`SwapOutcome`]：回滚成功时，`restored` 才是"当前在服务的实例"——
/// 调用方必须把它写回记录（M1-2）。
async fn swap_entry(record: &Mounted, new_plugin: Arc<dyn Plugin>) -> SwapOutcome {
    // 卸载旧实例。这一步无法与安装合并：同一隔离域里一个服务名只能有一个实现，
    // 旧实例还在时新实例的 `provide` 必然冲突。
    if let Some(fiber) = record.fiber.upgrade() {
        fiber.dispose().await;
    }

    // 挂新实例。**用记录里的派生上下文**（M4-1）：行级 isolate/intercept 挂在它上面，
    // 用根 ctx 重挂等于把行悄悄搬回默认域。
    let scope = &record.scope;
    let fiber = mount(
        scope,
        new_plugin.clone(),
        record.config.clone(),
        record.inject.clone(),
    );
    fiber.settle().await;
    let Some(error) = fiber.error() else {
        return SwapOutcome::Ok {
            fiber,
            plugin: new_plugin,
        };
    };

    // 新实例失败：先把它拆干净——半途注册的副作用必须回收，
    // 否则它们会跟着回滚后的旧实例一起留在线上。
    fiber.dispose().await;

    // 用旧插件把旧实例装回来。
    let restored = mount(
        scope,
        record.plugin.clone(),
        record.config.clone(),
        record.inject.clone(),
    );
    restored.settle().await;
    match restored.error() {
        // 回滚成功：报新版本的失败，并说明已经回到旧实例。
        None => SwapOutcome::RolledBack {
            error: format!("{error}（已回滚到旧实例）"),
            restored,
        },
        // 回滚也失败：必须把两件事都报出来。此时线上**确实**是空的，
        // 静默才是最坏的结果——调用方需要知道要人工介入。
        Some(rollback_error) => SwapOutcome::Lost {
            error: format!(
                "{error}；且回滚失败：{rollback_error}（该条目当前没有可用实例，需要人工介入）"
            ),
            rollback_error,
        },
    }
}

/// **手动**重挂一行：预检新库（落新文件名 + 打开 + ABI 探活）→ 换名重挂 → 更新记录。
///
/// HMR 监听器在"插件文件指纹变了"时调它；**外部的确定性驱动**（acceptance spike、
/// 集成测试）也用它——于是"重载"这件事不必等墙钟轮询，而是"触碰文件 + 调一次"。
pub async fn reload_one(
    ctx: &Ctx,
    state: &Arc<LoaderState>,
    label: &str,
) -> std::result::Result<(), String> {
    // 预检 → 换实例 → 改记录 → 发布报告：整个变更序列与对账/启停互斥。
    // 不互斥时，一次在飞的对账会把这里刚卸下的行按旧目标重新挂回去（或挂成两份）。
    let guard = state.mutation.lock().await;
    let Some(mut record) = state.mounted.lock().unwrap().get(label).cloned() else {
        return Ok(());
    };

    // ① 预检：新库先落到新文件名、打开、ABI 探活。**全部通过才动正在跑的实例**
    //    ——这是比"先卸后装"更好的地方：坏构建不会让你先失去能力再发现它坏了。
    let staged = match stage_plugin(&record.source, state.next_revision()) {
        Ok(staged) => staged,
        Err(err) => {
            // 复制错误多半是瞬时的（文件正在被写、被杀毒软件短暂锁住）：只记录，
            // **不推进指纹**——否则这一行要等文件再变一次才会被重试。
            ctx.log(
                Level::Error,
                &format!("热重载 {label}: 暂存新库失败（保留当前实例，下轮重试）: {err}"),
            );
            state.publish_report();
            return Err(err.to_string());
        }
    };
    let plugin = match LoadedPlugin::open_staged(&staged) {
        Ok(plugin) => plugin,
        Err(err) => {
            // 打不开的副本永远不会被认领：立刻销账并删除，否则每个坏构建都会
            // 在泄漏台账里留下一个 `.rN`（`stagedLeaked` 只增不减）。
            drop(StagedFile::new(&staged));
            // 预检失败也要推进指纹：HMR 否则会每 500ms 重试同一个坏库（重试风暴）。
            advance_fingerprint(state, label);
            state.publish_report();
            ctx.log(
                Level::Error,
                &format!("热重载 {label}: 新库无法打开（保留当前实例）: {err}"),
            );
            return Err(err.to_string());
        }
    };
    if !plugin.ping() {
        // `plugin` 在此 drop：先卸库、后删暂存副本（`LoadedPlugin` 的字段声明序）。
        drop(plugin);
        advance_fingerprint(state, label);
        state.publish_report();
        ctx.log(
            Level::Error,
            &format!("热重载 {label}: ABI 探活失败（保留当前实例）"),
        );
        return Err("ABI 探活失败".into());
    }

    // ② 换名重挂。成功则新实例上线，失败则**回滚到旧实例**（回滚在 swap_entry 内完成）。
    //
    //    先把记录**摘出装载表**再动手（M4-3）：`swap_entry` 会 dispose 旧实例，而
    //    "表里的记录指向已 dispose 的实例"正是要消灭的状态。摘掉之后：
    //    - 观测者看到的是"这一行暂时不在服务"（真实情况）；
    //    - `disposing` 标记让 `internal/plugin` 观察者不把这次有意卸载误判成插件自我卸载。
    let new_plugin: Arc<dyn Plugin> = Arc::new(plugin);
    state.disposing.lock().unwrap().insert(label.to_string());
    state.mounted.lock().unwrap().remove(label);
    let outcome = swap_entry(&record, new_plugin.clone()).await;
    state.disposing.lock().unwrap().remove(label);

    // ③ 更新记录（M1-2：记录必须**始终指向当前在服务的实例**）。
    //
    // 失败分支同样要推进指纹——不推进它，轮询会每 500ms 重试同一个坏版本（重试风暴）；
    // `staged` / `plugin` / `fiber` 的修正交给 `commit_outcome` 单点负责。
    // `Lost`（新旧实例都不可用）时不放回表里，避免后续 diff/重载基于幽灵记录。
    let error = outcome.error_message().map(|err| err.to_string());
    let lost_reason = match &outcome {
        SwapOutcome::Lost { rollback_error, .. } => Some(rollback_error.clone()),
        _ => None,
    };
    let remove_entry = record.commit_outcome(outcome, Some(staged));
    if remove_entry {
        ctx.log(
            Level::Error,
            &format!(
                "热重载 {label}: 新旧实例均不可用（回滚失败原因：{}），条目已从装载表移除，需要人工介入",
                lost_reason.as_deref().unwrap_or("未知")
            ),
        );
    } else {
        state
            .mounted
            .lock()
            .unwrap()
            .insert(label.to_string(), record.clone());
    }

    // `loader.report` 是实时诊断面：重载改了实例/记录/失败，报告必须跟上。
    // （`publish_report` 自己会挡住"装载器不在 ACTIVE"的收尾期时序。）
    state.publish_report();
    drop(guard);

    // ⑤ 广播。监听者用它做可视化、指标或"重载后重放状态"。
    ctx.emit(
        "loader/reload",
        vec![Dyn::new(ExternValue(json!({
            "label": label,
            "source": record.source.to_string_lossy(),
            "error": error,
        })))],
    )
    .await;
    ctx.log(
        match &error {
            Some(_) => Level::Error,
            None => Level::Info,
        },
        &format!(
            "热重载 {label}: {}",
            error.clone().unwrap_or_else(|| "成功".into())
        ),
    );
    Ok(())
}

/// 预检失败时也要把指纹推进到装载表：不然 HMR 每 500ms 重试同一个坏库（重试风暴）。
fn advance_fingerprint(state: &Arc<LoaderState>, label: &str) {
    if let Some(record) = state.mounted.lock().unwrap().get_mut(label) {
        record.fingerprint = fingerprint(&record.source);
    }
}

fn find_by_label<'a>(entries: &'a mut [Entry], label: &str) -> Option<&'a mut Entry> {
    let mut ordinals = HashMap::new();
    find_by_label_into(entries, label, &mut ordinals)
}

/// 与 [`desired_rows`] **同一走法**的查找：同一个计数器语义、同样的遍历规则
/// （见 [`stable_label`]）。两处若漂移，回写就会落到错误的行上。
fn find_by_label_into<'a>(
    entries: &'a mut [Entry],
    label: &str,
    ordinals: &mut HashMap<String, usize>,
) -> Option<&'a mut Entry> {
    for entry in entries.iter_mut() {
        if stable_label(entry, ordinals) == label {
            return Some(entry);
        }
        if let Some(found) = find_by_label_into(&mut entry.children, label, ordinals) {
            return Some(found);
        }
    }
    None
}

/// 现在**应当**挂着的一行：叶子条目 + 稳定行标 + 派生挂载上下文 + 上下文输入。
///
/// group 行自己不挂载（只是容器），所以这里只有叶子行。
#[derive(Clone)]
struct FlatRow {
    label: String,
    entry: Entry,
    /// 这一行的派生上下文（M4-1）：隔离域与 intercept 都已按祖先链叠加。
    scope: Ctx,
    /// 派生上下文的自变量（M4-3 的 diff 用它判断"上下文变没变"）。
    scope_inputs: Vec<ScopeInput>,
}

/// 派生上下文的自变量：链上每层的 label + `isolate`/`intercept` 声明。
fn scope_inputs(chain: &[(String, Entry)]) -> Vec<ScopeInput> {
    chain
        .iter()
        .map(|(label, entry)| ScopeInput {
            label: label.clone(),
            isolate: entry.isolate.clone(),
            intercept: entry.intercept.clone(),
        })
        .collect()
}

/// 期望态计算的产物（M4-6 起把"休眠"也记下来：报告要回答"为什么没生效"）。
struct Desired {
    rows: Vec<FlatRow>,
    /// 逐行的问题（`<行标>: <原因>`）：不是"禁用"，而是"我们**无法判断**该不该挂"。
    issues: Vec<(String, String)>,
    /// 当前**休眠**的叶子行标（自身或祖先的 `disabled` 求值为真）。组行不是"能挂的行"，不计。
    disabled: Vec<String>,
}

/// 现在**应当**挂着的一组行——也就是"期望态"（M4-2；M4-3 的 `plan` 拿它做差异）。
///
/// 三条纪律：
/// 1. **整棵树都要走到**：`stable_label` 的序号是全局的，跳过（无论因为 disabled 还是
///    表达式）都会让编号与 [`find_by_label`] 漂移，回写就落到错误的行上；
/// 2. **label 只由结构决定**（P2-15）：`!!js`、`disabled` 都不参与编号；
/// 3. **disabled 现算**：每个叶子行用**自己的**派生 ctx 求值祖先链上的表达式
///    （上游 `entry.disabled` 的 getter 同理），于是上下文一变，下一次决策就变。
fn desired_rows(
    entries: &[Entry],
    decision: &RowDecision<'_>,
    parent: &Ctx,
    scopes: &mut RowScopes,
) -> Desired {
    let mut walk = DesiredWalk {
        decision,
        parent,
        scopes,
        ordinals: HashMap::new(),
        seen: HashSet::new(),
        rows: Vec::new(),
        issues: Vec::new(),
        disabled: Vec::new(),
    };
    walk.visit(entries, &[]);
    Desired {
        rows: walk.rows,
        issues: walk.issues,
        disabled: walk.disabled,
    }
}

struct DesiredWalk<'a, 'b> {
    decision: &'a RowDecision<'b>,
    parent: &'a Ctx,
    scopes: &'a mut RowScopes,
    ordinals: HashMap<String, usize>,
    /// 已经出现过的行标：重复 `id` 会撞名（M4-3 之前会静默覆盖装载表记录）。
    seen: HashSet<String>,
    rows: Vec<FlatRow>,
    issues: Vec<(String, String)>,
    disabled: Vec<String>,
}

impl DesiredWalk<'_, '_> {
    fn visit(&mut self, entries: &[Entry], ancestors: &[(String, Entry)]) {
        for entry in entries {
            let label = stable_label(entry, &mut self.ordinals);
            // 重复 `id`（或 `id` 撞上生成的行标）必须**报出来**：装载表是 label → 记录，
            // 静默覆盖会让其中一行的实例变成无人认领的幽灵。重复出现的行**不挂载**，
            // 语义是"第一行获胜"，且问题进 `failures`/日志（strict 下整体失败）。
            if !self.seen.insert(label.clone()) {
                self.issues.push((
                    label,
                    "重复 label（两行解析成同一个行标识；请给它们不同的 id），已跳过该行"
                        .to_string(),
                ));
                continue;
            }
            let mut chain: Vec<(String, Entry)> = ancestors.to_vec();
            chain.push((label.clone(), entry.clone()));

            // 组行自己不挂载：它的 `disabled` 会通过链传到每个后代（这是"组禁用"
            // 的全部机制——不需要逐个子行去改）。
            if entry.group {
                self.visit(&entry.children, &chain);
                continue;
            }

            // 派生上下文**先派生再决策**：链上的 `!!js` 表达式读的就是这一行的上下文
            // （上游 `this.evaluate(...)` 同理），所以决策不能没有它。
            let scope = self.scopes.scope_ctx(self.parent, &chain);

            // 派生 disabled：自己或任一祖先为真 ⇒ 这一行不挂。这是**正常状态**，不报错——
            // 但 M4-6 起要**记下来**（报告里点名休眠行：用户问的往往是"我配了它，为什么没生效"）。
            match self.decision.disabled(&chain, &scope) {
                Ok(true) => {
                    self.disabled.push(label);
                    continue;
                }
                Ok(false) => {}
                // 算不出来（子集外 / 语法错）⇒ 不挂载并逐行报出（fail loud 的落点）。
                Err(issue) => {
                    self.issues.push((label, issue));
                    continue;
                }
            }

            // `config` 位置的 `!!js`：**先算一遍**（M4-4b 的决策期预检）。
            //
            // 真正的求值发生在每行 fiber 自己的 `internal/config` 解析里（值要跟着上下文
            // 走、每次解析现算），这里算的这一遍只为了两件事：
            // ① 错误带**配置路径**（链上只有"这一行的配置"这一层信息）；
            // ② 算不出来的行**不挂**——挂了就是把原始标记喂给插件（正是 M1-4 消除的静默错误）。
            if let Some(config) = &entry.config {
                let path = format!("{label}.config");
                if let Err(issue) =
                    interpolate_value(config, &path, &scope, self.decision.evaluator)
                {
                    self.issues.push((label, issue));
                    continue;
                }
            }
            self.rows.push(FlatRow {
                label,
                entry: entry.clone(),
                scope,
                scope_inputs: scope_inputs(&chain),
            });
        }
    }
}

/// 行的**稳定 label**：有 `id` 用它，否则 `name#<序号>`（同名行的出现序）。
///
/// 这是 P2-15 的修复：原来的 `id ?? name` 会让**同名无 id 的多行互相覆盖**，
/// 装载表、回写、对账（plan/diff/stale_labels）随之全错。
///
/// **label 只由结构决定**（M4-2 修订）：早先"含未求值 `!!js` 的行不消耗序号"会让编号
/// 依赖求值结果（表达式行、禁用组的子树都会把它改掉），而 [`find_by_label`] 的走法
/// 一旦与之漂移，回写就落到错误的行上。现在两处都是"整棵树都走、按出现序编号"。
fn stable_label(entry: &Entry, ordinals: &mut HashMap<String, usize>) -> String {
    if let Some(id) = &entry.id {
        return id.clone();
    }
    let counter = ordinals.entry(entry.name.clone()).or_insert(0);
    let label = format!("{}#{}", entry.name, *counter);
    *counter += 1;
    label
}

/// 与装载器同一套结构行标的一行（`id` 或 `name#N`）。
#[derive(Clone, Debug)]
pub struct LabeledEntry {
    pub label: String,
    pub entry: Entry,
}

/// 按装载器同一套走法生成叶子行标（组行本身不返回，但会消耗序号）。
///
/// `inspect` / `validate` 用它，避免“预览显示 `hello`、`status` 显示 `hello#0`”
/// 这种无法对应的漂移。重复 `id` 仍会得到相同 label；是否拒绝由解析/装载阶段裁决。
pub fn labeled_entries(entries: &[Entry]) -> Vec<LabeledEntry> {
    fn visit(
        entries: &[Entry],
        ordinals: &mut HashMap<String, usize>,
        out: &mut Vec<LabeledEntry>,
    ) {
        for entry in entries {
            let label = stable_label(entry, ordinals);
            if entry.group {
                visit(&entry.children, ordinals, out);
            } else {
                out.push(LabeledEntry {
                    label,
                    entry: entry.clone(),
                });
            }
        }
    }
    let mut ordinals = HashMap::new();
    let mut rows = Vec::new();
    visit(entries, &mut ordinals, &mut rows);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
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
            issues[0].1.contains("at inner-bad.config.k"),
            "错误必须带路径：{issues:?}"
        );
        assert!(issues[0].1.contains("1 +"), "错误必须带原文：{issues:?}");
        assert!(
            !issues.iter().any(|(label, _)| label.contains("alpha#1")),
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
            hmr: None,
            js_evaluator: None,
            interpolation: InterpolationMode::Builtin,
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
        );
        state
            .mounted
            .lock()
            .unwrap()
            .insert("entry".into(), record.clone());
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
}
