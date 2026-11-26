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
    fn CreateFileW(
        file_name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *mut std::ffi::c_void,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template_file: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
}

#[cfg(unix)]
unsafe extern "C" {
    fn flock(fd: std::ffi::c_int, operation: std::ffi::c_int) -> std::ffi::c_int;
}

#[cfg(not(any(unix, windows)))]
fn replace_file(temp: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(temp, target)
}

#[cfg(windows)]
fn is_lock_contention(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
    )
}

#[cfg(not(windows))]
fn is_lock_contention(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::AlreadyExists
}

/// 跨进程/跨线程互斥的陈旧锁回收门禁：
/// - Windows：`CreateFileW` 以 `dwShareMode = 0`（独占）+ `FILE_FLAG_DELETE_ON_CLOSE` 打开 `.reclaim`；
///   进程崩溃后内核立即释放句柄，即使磁盘残留 `.reclaim` 文件也不会死锁，且绝无双持窗口。
/// - Unix：`O_CREAT | O_RDWR` + `flock(LOCK_EX | LOCK_NB)` + 核验 `fstat.ino == stat.ino`，
///   崩溃自动释放，同样不存在陈旧 `.reclaim` 误删窗口。
#[cfg(windows)]
struct ReclaimMutex {
    handle: *mut std::ffi::c_void,
}

#[cfg(windows)]
impl ReclaimMutex {
    fn try_acquire(path: &Path) -> Option<Self> {
        use std::os::windows::ffi::OsStrExt;
        const GENERIC_READ: u32 = 0x8000_0000;
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const DELETE: u32 = 0x0001_0000;
        const OPEN_ALWAYS: u32 = 4;
        const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
        const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
        let invalid = (-1isize) as *mut std::ffi::c_void;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE | DELETE,
                0,
                std::ptr::null_mut(),
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_DELETE_ON_CLOSE,
                std::ptr::null_mut(),
            )
        };
        if handle == invalid || handle.is_null() {
            None
        } else {
            Some(Self { handle })
        }
    }
}

#[cfg(windows)]
impl Drop for ReclaimMutex {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(unix)]
struct ReclaimMutex {
    path: PathBuf,
    _file: std::fs::File,
}

#[cfg(unix)]
impl ReclaimMutex {
    fn try_acquire(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::io::AsRawFd;
        const LOCK_EX: std::ffi::c_int = 2;
        const LOCK_NB: std::ffi::c_int = 4;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .ok()?;
        let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
        if rc != 0 {
            return None;
        }
        let fd_meta = file.metadata().ok()?;
        let path_meta = std::fs::metadata(path).ok()?;
        if fd_meta.dev() != path_meta.dev() || fd_meta.ino() != path_meta.ino() {
            return None;
        }
        Some(Self {
            path: path.to_path_buf(),
            _file: file,
        })
    }
}

#[cfg(unix)]
impl Drop for ReclaimMutex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(not(any(unix, windows)))]
struct ReclaimMutex {
    path: PathBuf,
}

#[cfg(not(any(unix, windows)))]
impl ReclaimMutex {
    fn try_acquire(path: &Path) -> Option<Self> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .ok()?;
        drop(file);
        Some(Self {
            path: path.to_path_buf(),
        })
    }
}

#[cfg(not(any(unix, windows)))]
impl Drop for ReclaimMutex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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
                // 而不是 `AlreadyExists`；非 Windows 平台仅对 `AlreadyExists` 重试，
                // 真实目录权限错误立即返回，不再空转 1 秒。
                Err(err) if is_lock_contention(&err) => {
                    let is_stale = |p: &Path, max_age: Duration| {
                        std::fs::metadata(p)
                            .and_then(|meta| meta.modified())
                            .ok()
                            .and_then(|time| time.elapsed().ok())
                            .is_some_and(|age| age > max_age)
                    };
                    if is_stale(&path, Duration::from_secs(30)) {
                        // 用内核级独占 `.reclaim` 互斥锁串行化陈旧锁回收，并在持锁后二次确认
                        // mtime 仍已陈旧，防止并发等待者在另一方刚创建新锁后误删新锁。
                        let reclaim_path = dir.join(format!(".{name}.lock.reclaim"));
                        if let Some(_reclaim) = ReclaimMutex::try_acquire(&reclaim_path) {
                            if is_stale(&path, Duration::from_secs(30))
                                && std::fs::remove_file(&path).is_ok()
                            {
                                drop(_reclaim);
                                continue;
                            }
                        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    #[test]
    fn r2_p2_config_lock_stale_reclaim_preserves_mutual_exclusion() {
        let dir =
            std::env::temp_dir().join(format!("cordis-config-lock-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("cordis.yml");
        let lock_path = dir.join(".cordis.yml.lock");

        for round in 0..25 {
            let _ = std::fs::remove_file(&lock_path);
            let f = std::fs::File::create(&lock_path).unwrap();
            let old = std::time::SystemTime::now() - Duration::from_secs(120);
            f.set_modified(old).unwrap();
            drop(f);

            let active = Arc::new(AtomicUsize::new(0));
            let violations = Arc::new(AtomicUsize::new(0));
            let barrier = Arc::new(Barrier::new(6));
            let mut handles = Vec::new();
            for _ in 0..6 {
                let target = target.clone();
                let active = active.clone();
                let violations = violations.clone();
                let barrier = barrier.clone();
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    let _guard = ConfigLock::acquire(&target).expect("acquire lock");
                    let prev = active.fetch_add(1, Ordering::SeqCst);
                    if prev != 0 {
                        violations.fetch_add(1, Ordering::SeqCst);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                    active.fetch_sub(1, Ordering::SeqCst);
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(
                violations.load(Ordering::SeqCst),
                0,
                "round {round}: 并发回收陈旧锁时破坏了互斥性（多个线程同时持有 ConfigLock）"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn r3_p2_config_lock_stale_reclaim_file_also_preserves_mutual_exclusion() {
        let dir = std::env::temp_dir().join(format!(
            "cordis-config-lock-reclaim-race-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("cordis.yml");
        let lock_path = dir.join(".cordis.yml.lock");
        let reclaim_path = dir.join(".cordis.yml.lock.reclaim");

        for round in 0..20 {
            let _ = std::fs::remove_file(&lock_path);
            let _ = std::fs::remove_file(&reclaim_path);
            let old = std::time::SystemTime::now() - Duration::from_secs(120);
            for p in [&lock_path, &reclaim_path] {
                let f = std::fs::File::create(p).unwrap();
                f.set_modified(old).unwrap();
            }

            let active = Arc::new(AtomicUsize::new(0));
            let violations = Arc::new(AtomicUsize::new(0));
            let barrier = Arc::new(Barrier::new(6));
            let mut handles = Vec::new();
            for _ in 0..6 {
                let target = target.clone();
                let active = active.clone();
                let violations = violations.clone();
                let barrier = barrier.clone();
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    let _guard = ConfigLock::acquire(&target).expect("acquire lock");
                    let prev = active.fetch_add(1, Ordering::SeqCst);
                    if prev != 0 {
                        violations.fetch_add(1, Ordering::SeqCst);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                    active.fetch_sub(1, Ordering::SeqCst);
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(
                violations.load(Ordering::SeqCst),
                0,
                "round {round}: 并发回收陈旧 .reclaim 锁时破坏了互斥性"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
