//! 插件文件暂存（[`StagedFile`] / [`stage_plugin`]）、路径解析与挂载辅助（O11 拆分）。

use cordis_core::{Config, Ctx, Error, Plugin, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

// ------------------------------------------------------------ 插件解析

/// 进程级"已暂存、未认领"台账（G11 / D8）：`stage_plugin` 成功即入账，
/// [`StagedFile::new`] 认领即销账——销不掉账的就是泄漏，
/// [`staged_leaked`] / `loader.report` 的 `stagedLeaked` 直接暴露它。
static STAGED_UNADOPTED: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();

fn staged_unadopted() -> &'static Mutex<std::collections::HashSet<PathBuf>> {
    STAGED_UNADOPTED.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// 当前**已暂存但无人认领**的文件数（G11 的泄漏计数）。
///
/// 装载全部成功 ⇒ 每个暂存副本都被各自的 [`StagedFile`] 认领 ⇒ 0；
/// `stage_plugin` 成功而 `open` 失败（坏 DLL、探活失败前就报错等）⇒ >0。
pub fn staged_leaked() -> usize {
    staged_unadopted().lock().unwrap().len()
}

/// 一个**已被认领**的暂存文件：drop 时删除自己（M0b，G11 的所有权半边）。
///
/// - 认领（[`StagedFile::new`]）同时从泄漏台账销账；
/// - drop 删除失败只记日志、绝不 panic——文件可能仍被加载中的库锁住，
///   真正可靠的删除发生在 [`LoadedPlugin`] 的字段声明序保证的
///   "先 FreeLibrary、后 remove_file"之后；
/// - `Clone`/`Copy` 故意不存在：所有权唯一，共享请用 `Arc<StagedFile>`
///   （回滚记录就是这么持有它的）。
pub struct StagedFile(PathBuf);

impl StagedFile {
    /// 认领一个暂存文件（从泄漏台账销账）。文件此刻**不会**被删——
    /// 它要活到本对象 drop。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        staged_unadopted().lock().unwrap().remove(&path);
        StagedFile(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.0) {
            // NotFound 是良性（别的路径已经删过）；其余只记日志：
            // 删除失败不能把宿主进程拖崩。
            if err.kind() != std::io::ErrorKind::NotFound {
                eprintln!("[cordis] 删除暂存文件 {} 失败: {err}", self.0.display());
            }
        }
    }
}

/// 插件名 → 动态库路径，带**版本化的临时副本**。
///
/// # 为什么要有副本
/// Windows 上已加载的 DLL 被文件系统锁住，无法原地覆盖；Linux 上覆盖已映射的
/// `.so` 也会让进程处于未定义状态。热重载因此必须把新库落到一个**新文件名**，
/// 让旧句柄继续指向旧文件、新句柄指向新文件，等旧 fiber 全部卸载后旧文件才可回收。
/// 这不是技巧，是动态加载的硬约束——任何支持热重载的插件系统都会撞上它。
pub fn stage_plugin(path: &Path, revision: u64) -> Result<PathBuf> {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "plugin".into());
    let extension = path.extension().map(|s| s.to_string_lossy().to_string());
    // 修订号只在**单个 LoaderState** 内单调。两个装载器首次加载同一 DLL 时
    // 都会请求 revision 1；若文件名只有 `.r1`，两边会同时写同一个副本，
    // 一边的 `LoadLibrary` 可能读到另一边正在覆盖的字节。把进程号 + 全局序号
    // 放进 `.rN` 之前的 stem，既保持按 `.r<digits>` 识别的既有清理逻辑，
    // 又让每次暂存拥有独立路径。跨进程同时写同一源文件也不会撞名。
    static STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let unique = format!(
        "s{}-{}",
        std::process::id(),
        STAGE_SEQUENCE.fetch_add(1, Ordering::SeqCst)
    );
    let file_name = match &extension {
        Some(extension) => format!("{stem}.{unique}.r{revision}.{extension}"),
        None => format!("{stem}.{unique}.r{revision}"),
    };
    let target = path.with_file_name(file_name);
    std::fs::copy(path, &target)
        .map_err(|err| format!("复制插件到 {} 失败: {err}", target.display()))?;
    // 副本落地即入泄漏台账（G11）：在它被 `StagedFile` 认领之前，
    // 它就是一笔潜在的泄漏——认领发生在 `LoadedPlugin::open_staged`。
    staged_unadopted().lock().unwrap().insert(target.clone());
    Ok(target)
}

/// 插件解析：`name` → 动态库路径。
///
/// 这里是"任意插件安装"真正落地的地方。本实现只做最朴素的文件解析；
/// 换成一个包管理器（npm 风格的 `node_modules` 解析、OCI、内置 registry）
/// 只需要替换这一个函数——内核与 ABI 都不需要改。
pub fn resolve_plugin_path(name: &str, search: &[PathBuf]) -> Result<PathBuf> {
    let candidates = |stem: &str| {
        let mut files = vec![PathBuf::from(stem)];
        if cfg!(windows) {
            files.push(PathBuf::from(format!("{stem}.dll")));
        } else if cfg!(target_os = "macos") {
            files.push(PathBuf::from(format!("{stem}.dylib")));
            // Cargo 的 cdylib 产物在 macOS 上也带 `lib` 前缀。
            files.push(PathBuf::from(format!("lib{stem}.dylib")));
        } else {
            files.push(PathBuf::from(format!("{stem}.so")));
            // 也接受 `lib<name>.so` 的约定。
            files.push(PathBuf::from(format!("lib{stem}.so")));
        }
        files
    };
    for candidate in candidates(name) {
        let path = candidate;
        if path.is_file() {
            return Ok(path);
        }
    }
    for dir in search {
        for candidate in candidates(name) {
            let path = dir.join(candidate);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    Err(format!("找不到插件 \"{name}\"；搜索目录: {search:?}").into())
}

/// 把加载好的插件挂到上下文上，并把行级 `inject` 合并进 fiber。
///
/// **行级 inject 必须在依赖解析之前合并**（`loader/src/index.ts:129-135`）。
/// 这里用"构造 fiber 时直接传入合并后的 inject"实现，比 JS 的
/// `internal/plugin` 观察者路径更直接；观察者路径在内核里也保留着，
/// 供不自己创建 fiber 的外部扩展使用。
///
/// # Panics
/// `row_inject` 形状非法时 panic。装载器路径已在 [`crate::patch::Entry`] 解析期
/// 校验；直接调用本函数应传入同样合法的形状（数组、平铺对象或
/// `{required, optional}`）。
pub fn mount(
    ctx: &Ctx,
    plugin: Arc<dyn Plugin>,
    config: Config,
    row_inject: Option<Config>,
) -> Arc<cordis_core::Fiber> {
    mount_inner(ctx, plugin, config, row_inject, None)
}

/// 装载器内部挂载入口：在 `Fiber` 创建时绑定不可变行标，确保 `apply` 首跳就能获得稳定的行身份。
pub(crate) fn mount_with_label(
    ctx: &Ctx,
    plugin: Arc<dyn Plugin>,
    config: Config,
    row_inject: Option<Config>,
    label: &str,
) -> Arc<cordis_core::Fiber> {
    mount_inner(ctx, plugin, config, row_inject, Some(label))
}

fn mount_inner(
    ctx: &Ctx,
    plugin: Arc<dyn Plugin>,
    config: Config,
    row_inject: Option<Config>,
    host_label: Option<&str>,
) -> Arc<cordis_core::Fiber> {
    let mut inject = plugin.inject();
    let parsed = row_inject
        .as_ref()
        .map(|row| {
            crate::patch::parse_inject(row)
                .unwrap_or_else(|err| panic!("entry-level inject 已在解析期校验: {err}"))
        })
        .unwrap_or_default();
    for (name, value) in parsed.required {
        if parsed.merge_config {
            inject = inject.with(&name, value);
        } else if !inject.contains(&name) {
            inject.0.push((name.into(), Config::Null));
        }
    }
    ctx.store
        .plugin_with_host_label(ctx, plugin, config, inject, &parsed.optional, host_label)
}

/// 便利函数：把一个 `Error` 转成 JSON 友好的字符串。
pub fn describe(error: &Error) -> String {
    error.to_string()
}
