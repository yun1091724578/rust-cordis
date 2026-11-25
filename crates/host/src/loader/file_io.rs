//! 配置文件的原子写、同目录锁、指纹与稳读工具（从 loader.rs 拆分）。

use super::{ConfigFiles, FileSnapshot};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};
pub(super) fn fingerprint(path: &std::path::Path) -> Option<(u64, u64)> {
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
pub(super) fn config_fingerprint(files: &ConfigFiles) -> Vec<Option<(u64, u64)>> {
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
pub(super) fn write_atomically(path: &Path, text: &str) -> std::result::Result<(), String> {
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
pub(super) fn write_atomically_if_unchanged(
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
pub(super) fn append_atomically(path: &Path, text: &str) -> std::result::Result<(), String> {
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
pub(super) fn read_files(
    files: &ConfigFiles,
    read: &mut (dyn FnMut(&Path) -> std::io::Result<String> + Send),
) -> std::result::Result<FileSnapshot, (PathBuf, std::io::Error)> {
    let main = read(&files.main).map_err(|err| (files.main.clone(), err))?;
    let mut patches = Vec::with_capacity(files.patches.len());
    for path in &files.patches {
        patches.push(read(path).map_err(|err| (path.clone(), err))?);
    }
    Ok(FileSnapshot { main, patches })
}

/// **稳读**：同一组文件读两遍、内容一致才采纳。
///
/// 为什么不是"读一次就采纳"：编辑器保存是"写临时文件 + rename"、也有程序直接截断重写，
/// 中途被读到就是半截内容。两遍之间等 `settle`，不一致就当作"文件正在被写"，
/// 这一轮什么都不做（下一轮再看）。`read` 可注入——测试用它把"正在被写"钉成确定性的一刻。
pub(super) async fn read_settled_with(
    files: &ConfigFiles,
    settle: Duration,
    read: &mut (dyn FnMut(&Path) -> std::io::Result<String> + Send),
) -> std::result::Result<Option<FileSnapshot>, (PathBuf, std::io::Error)> {
    let first = read_files(files, read)?;
    tokio::time::sleep(settle).await;
    let second = read_files(files, read)?;
    Ok((first == second).then_some(first))
}
