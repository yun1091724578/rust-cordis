//! M0b：**暂存文件的所有权与泄漏台账**（G11 / D8 / D9）。
//!
//! G11 的现状是：`stage_plugin` 落下的 `.rN` 副本没有任何人负责删除——
//! 换来的是"热重载永远不用等 FreeLibrary"，代价是每次重载都漏一个文件。
//! M0b 给它三层修法，本文件测前两层（无 DLL，纯记账——D9）：
//!
//! 1. `StagedFile`：认领即销账，drop 即删文件（失败只记日志，绝不 panic）；
//! 2. `staged_leaked()`：进程级台账，`stage_plugin` 入账、认领销账——
//!    报表（`loader.report` 的 `stagedLeaked`）由此暴露泄漏；
//! 3. 真 DLL 的"先卸库、后删文件"顺序（字段声明序）只进 demo/spike 断言。
//!
//! 守卫测试：只要回滚记录（`Mounted.plugin: Arc<dyn Plugin>`）还握着插件，
//! 暂存文件就必须还在——它是回滚的物理载体，提前删除等于把回滚路堵死。

use cordis_host::{stage_plugin, staged_leaked, StagedFile};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

/// 台账是**进程级**单例，两个测试并行会互相踩计数——串行跑。
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 每次调用给一个全局唯一的修订号：台账按路径去重，测试重跑（同进程）时
/// 复用同一路径会让"+1"断言失真。
fn unique_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    std::process::id() as u64 * 1_000_000 + NEXT.fetch_add(1, Ordering::SeqCst)
}

fn fixture_source(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cordis-m0b-staged-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let source = dir.join(format!("{tag}.dll"));
    std::fs::write(&source, b"stub-not-a-real-dll").expect("造假源文件");
    source
}

/// M0b 红测试：泄漏计数必须**被暴露**且成功路径归零。
///
/// 纯记账，无 DLL（D9）：暂存一个假文件 ⇒ 台账 +1；`StagedFile::new` 认领 ⇒
/// 归零；drop ⇒ 文件真的从磁盘消失。现在的实现既没有 `staged_leaked` 也没有
/// `StagedFile`——编译期就红。
#[test]
fn staged_leak_counter_is_exposed_and_reaches_zero_on_success() {
    let _serial = serial();
    let source = fixture_source("ledger");
    let before = staged_leaked();

    // ① 暂存成功但无人认领 ⇒ 台账 +1（这就是 G11 说的"泄漏"）。
    let staged = stage_plugin(&source, unique_revision()).expect("暂存假文件必然成功");
    assert_eq!(
        staged_leaked(),
        before + 1,
        "暂存后未被认领 ⇒ stagedLeaked 必须 +1"
    );

    // ② 认领 ⇒ 销账归零；文件仍在磁盘上（此刻它还被持有者需要着）。
    let adopted = StagedFile::new(&staged);
    assert_eq!(staged_leaked(), before, "被 StagedFile 认领 ⇒ 计数必须归零");
    assert!(
        adopted.path().exists(),
        "认领 ≠ 删除：文件要活到持有者 drop"
    );

    // ③ drop ⇒ 真删除。
    drop(adopted);
    assert!(
        !staged.exists(),
        "drop StagedFile 之后暂存文件必须真的被删掉"
    );

    let _ = std::fs::remove_file(&source);
}

/// 两个装载器首次加载同一 DLL 都会请求 revision 1；暂存路径必须彼此独立，
/// 否则会同时写同一个 `.r1.dll`，`LoadLibrary` 可能读到被覆盖中的字节。
#[test]
fn same_revision_staging_uses_distinct_paths() {
    let _serial = serial();
    let source = fixture_source("same-revision");
    let a = stage_plugin(&source, 1).expect("第一次暂存");
    let b = stage_plugin(&source, 1).expect("第二次暂存");
    assert_ne!(a, b, "相同 revision 的两次暂存不得复用同一个文件");
    assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&source).unwrap());
    assert_eq!(std::fs::read(&b).unwrap(), std::fs::read(&source).unwrap());

    drop(StagedFile::new(&a));
    drop(StagedFile::new(&b));
    let _ = std::fs::remove_file(&source);
}

/// 守卫（本就应绿，单独计数）：回滚记录还握着插件 ⇒ 暂存文件必须还在。
///
/// `Mounted.plugin` 是 `Arc<dyn Plugin>`（内含 `StagedFile`）——共享所有权的
/// 语义就是"最后一个持有者 drop 时才删除"。这里用 `Arc<StagedFile>` 直接
/// 钉死这层语义：回滚记录与运行表各握一份，谁先放手文件都不许消失。
#[test]
fn staged_file_survives_while_the_rollback_record_holds_the_plugin() {
    let _serial = serial();
    let source = fixture_source("guard");
    let staged = stage_plugin(&source, unique_revision()).expect("暂存假文件必然成功");

    // 运行表（强引用）与回滚记录（强引用）各握一份；watcher 只留弱引用。
    let runtime_table: Arc<StagedFile> = Arc::new(StagedFile::new(&staged));
    let rollback_record: Arc<StagedFile> = runtime_table.clone();
    let watcher: Weak<StagedFile> = Arc::downgrade(&runtime_table);

    // 运行表先放手（比如换名重挂）：回滚记录还在 ⇒ 文件必须还在。
    drop(runtime_table);
    assert!(
        rollback_record.path().exists(),
        "回滚记录还握着插件 ⇒ 暂存文件不许被删"
    );

    // 回滚记录也放手：最后一人 drop ⇒ 文件删除、弱引用升不上来。
    drop(rollback_record);
    assert!(watcher.upgrade().is_none(), "此时不该再有任何持有者");
    assert!(!staged.exists(), "最后一个持有者 drop ⇒ 暂存文件必须被删");

    let _ = std::fs::remove_file(&source);
}

/// 多轮暂存 / 认领 / 删除后台账必须回到基线（阶段 0 #6 的暂存部分）。
///
/// 纯记账，无 DLL：只验证 `stage_plugin` 入账、`StagedFile::new` 销账、
/// drop 删文件三条在 64 轮里都不累积。
#[test]
fn staged_ledger_returns_to_baseline_across_many_rounds() {
    let _serial = serial();
    let before = staged_leaked();
    let source = fixture_source("rounds");

    for round in 0..64 {
        let staged = stage_plugin(&source, unique_revision()).expect("暂存假文件必然成功");
        assert_eq!(
            staged_leaked(),
            before + 1,
            "第 {round} 轮暂存后未认领 ⇒ stagedLeaked 必须 +1"
        );

        let adopted = StagedFile::new(&staged);
        assert_eq!(staged_leaked(), before, "第 {round} 轮认领后台账必须归零");
        assert!(adopted.path().exists(), "第 {round} 轮认领 ≠ 删除");
        drop(adopted);
        assert!(!staged.exists(), "第 {round} 轮 drop 后文件必须删除");
    }

    assert_eq!(staged_leaked(), before, "64 轮后台账必须回到基线");
    let _ = std::fs::remove_file(&source);
}
