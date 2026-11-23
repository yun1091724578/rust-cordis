//! `cargo xtask package-check`：用**打包产物**（不是仓库源码）构建一个隔离消费者，
//! 并跑通一次真实的跨 ABI 服务调用。
//!
//! 为什么需要它：`cargo xtask consumer` 用的是**仓库源码的 path 依赖**，所以
//! "crates.io 上那份包到底能不能用"从来没有被验证过——打包会把 workspace 继承字段
//! 展开、把 path 依赖改写成 registry 依赖，`include` 写错、忘了提交新模块这类问题
//! 都只会在发布之后才暴露。
//!
//! 判据有两条，缺一不可：
//! 1. 消费者能构建并跑通一次服务调用（打包产物真的可用）；
//! 2. 消费者的依赖图里**没有任何**指向仓库源码的 `path`——否则"产物可用"可能只是
//!    被源码兜住了。第 2 条既是生成时的文本判据，也是 `cargo metadata` 的溯源判据。
//!
//! 另有两条"证据可信度"纪律，缺了判据会假绿：
//! - **隔离判定必须规范化**：`--dir` 含 `..` 一律拒绝；其余情况对"已存在的最深祖先"
//!   与仓库根都 canonicalize 后再比。纯字符串前缀比较下，`C:\repo\target\..\..\x`
//!   这样的写法（以及盘符大小写、8.3 短名）都能把消费者塞进仓库，而它会继承仓库的
//!   `.cargo/config.toml`；
//! - **`.crate` 必须是本次写的**：`CARGO_TARGET_DIR/package/*.crate` 跨运行保留，
//!   打包前先删掉这四个目标产物，`collect` 再断言 mtime ≥ 本次开始时间——否则
//!   "只改仓库源码"也可能对着上一次的产物判绿，整个检查就失去意义。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

/// 包清单：**唯一来源**。清残留、判新鲜度、解包扫描、打包顺序全部从这里派生。
///
/// 顺序有约束：core/abi/macros 在前、host 最后（host 依赖 core/abi 的 registry 形式，
/// 必须最后打包）。下面几个常量按位置切分这张表，所以新增 crate 时只改这里——
/// 任何一处手抄的副本都会在下次新增 crate 时静默漏掉，让新鲜度检查退化。
const ALL_PACKAGES: [&str; 5] = [
    "rust-cordis-core",
    "rust-cordis-abi",
    "rust-cordis-macros",
    "rust-cordis-host",
    "rust-cordis-sdk",
];
/// 打包顺序：无内部依赖的底层包（core/abi/macros）在前。
const HEAD_PACKAGES: &[&str] = &[ALL_PACKAGES[0], ALL_PACKAGES[1], ALL_PACKAGES[2]];
/// 依赖内部包的尾部包（host/sdk），通过临时 patch 后打包。
const TAIL_PACKAGES: &[&str] = &[ALL_PACKAGES[3], ALL_PACKAGES[4]];
const CORE_PACKAGE: &str = ALL_PACKAGES[0];
const ABI_PACKAGE: &str = ALL_PACKAGES[1];
const MACROS_PACKAGE: &str = ALL_PACKAGES[2];
const HOST_PACKAGE: &str = ALL_PACKAGES[3];
const SDK_PACKAGE: &str = ALL_PACKAGES[4];
/// 消费者直接/经宏展开依赖的全部 5 个发布包。
const CONSUMER_PACKAGES: &[&str] = &[
    CORE_PACKAGE,
    ABI_PACKAGE,
    MACROS_PACKAGE,
    HOST_PACKAGE,
    SDK_PACKAGE,
];

/// 消费者模板：`include_str!` 而不是 `format!`（模板里满是 `{}`）。
const CONSUMER_MANIFEST: &str = include_str!("package_check_assets/consumer_cargo.toml.in");
const CONSUMER_MAIN: &str = include_str!("package_check_assets/consumer_main.rs.in");

/// 消费者必须打印的判据行，与 `consumer_main.rs.in` 里那两条 `println!` 一一对应。
///
/// 只认退出码是不够的：把消费者换成一个立刻 `exit(0)` 的空壳同样会绿。两行都在，
/// 才说明真的跑完了服务调用并验过关闭后的实例数。
const REQUIRED_MARKERS: [&str; 2] = ["service call ok", "instances after close: 0"];

/// 临时目录的归属标记：只有带它的目录才会被清空（防止 `--dir` 打错就递归删别人的目录）。
const ROOT_MARKER: &str = "package-check-root.marker";

/// `.crate` 新鲜度判据允许的时钟/文件系统误差。
///
/// 主线保证是"打包前先删掉目标 `.crate`"（见 [`Check::drop_stale_crate`]），mtime 只是
/// 第二道：有些文件系统（FAT/exFAT、网络盘）的 mtime 粒度到 2s，卡死等号会误红。
const FRESHNESS_SLACK: Duration = Duration::from_secs(2);

pub fn command(args: Vec<String>) -> i32 {
    let options = match parse(&args) {
        Ok(Parsed::Help) => {
            print!("{}", crate::USAGE);
            return 0;
        }
        Ok(Parsed::Run(options)) => options,
        Err(message) => {
            eprintln!("{message}\n\n{}", crate::USAGE);
            return 2;
        }
    };
    let mut check = Check::new(options);
    match check.execute() {
        Ok(summary) => {
            check.print_summary(&summary);
            // 措辞与事实一一对应：谁让它留的（开关还是环境变量）、还是根本不该删。
            if check.options.reuse_dir {
                println!(
                    "[package-check] --reuse-dir：复用目录原样保留（不清理）：{}",
                    summary.root.display()
                );
            } else if let Some(source) = check.options.keep {
                println!(
                    "[package-check] 保留临时目录（{}）：{}",
                    source.describe(),
                    summary.root.display()
                );
            } else {
                check.cleanup();
            }
            0
        }
        Err(failure) => {
            eprintln!(
                "[package-check] 失败于阶段 \"{}\"：{}",
                failure.stage, failure.message
            );
            let root = &check.options.root;
            match retention(&check.options, check.prepare_state) {
                Retention::Reused => eprintln!(
                    "[package-check] --reuse-dir：复用目录保持原样（本命令不清理它）：{}",
                    root.display()
                ),
                Retention::OwnedMarked => eprintln!(
                    "[package-check] 临时目录已保留（产物、解包目录与消费者工程都在里面）：{}",
                    root.display()
                ),
                Retention::OwnedUnmarked => eprintln!(
                    "[package-check] 本命令已建目录、但写标记 {ROOT_MARKER} 失败；目录仍是本次\
                     运行的，原样保留：{}（空目录下次执行会自行清掉；有残留内容需手工删除）",
                    root.display()
                ),
                // prepare 在外来目录/非 ASCII/仓库内这几条上失败时，目录不是本次建的。
                Retention::Foreign => eprintln!(
                    "[package-check] 失败发生在本命令建目录之前；该目录不是本次运行的：\
                     原样保留 {}（缺 {ROOT_MARKER}）",
                    root.display()
                ),
                Retention::Absent => eprintln!(
                    "[package-check] 失败发生在本命令建目录之前，没有需要保留的目录：{}",
                    root.display()
                ),
            }
            1
        }
    }
}

// ------------------------------------------------------------------ 参数

#[derive(Debug)]
struct Options {
    root: PathBuf,
    /// `--keep` 的来源。打印"为什么保留"时必须准确：显式开关与环境变量不是一回事。
    keep: Option<KeepSource>,
    /// 实际生效的离线状态：默认离线，只有 `--online` 会打开网络。
    /// `--offline` 解析后仍是 `true`——它只是**显式**重申默认（见 [`parse`]）。
    offline: bool,
    /// 仅调试：跳过 prepare/package/unpack/生成消费者，复用现有 root。
    reuse_dir: bool,
}

#[derive(Clone, Copy, Debug)]
enum KeepSource {
    Flag,
    Env,
}

impl KeepSource {
    fn describe(self) -> &'static str {
        match self {
            Self::Flag => "--keep",
            Self::Env => "CORDIS_PACKAGE_KEEP",
        }
    }
}

enum Parsed {
    Help,
    Run(Options),
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut root: Option<PathBuf> = None;
    let mut keep = keep_from_env().then_some(KeepSource::Env);
    // 默认离线：这个检查的价值之一是"不联网也能跑"，联网只用于补缓存。
    let mut offline = true;
    let mut reuse_dir = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        index += 1;
        match arg {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--keep" => keep = Some(KeepSource::Flag),
            "--reuse-dir" => reuse_dir = true,
            // 空操作（默认已离线）：保留它是为了显式钉住离线——将来若把默认改成联网，
            // 脚本里写好 `--offline` 的地方不必跟着改。
            "--offline" => offline = true,
            "--online" => offline = false,
            "--dir" => {
                let value = args
                    .get(index)
                    .ok_or_else(|| "--dir 需要一个绝对路径".to_string())?;
                index += 1;
                if root.replace(PathBuf::from(value)).is_some() {
                    return Err("--dir 只能给一次".to_string());
                }
            }
            unknown if unknown.starts_with('-') => {
                return Err(format!("package-check 未知参数 {unknown:?}"));
            }
            positional => {
                return Err(format!(
                    "package-check 不接受位置参数，实际 {positional:?}（目录用 --dir）"
                ));
            }
        }
    }
    let root = match root {
        Some(path) => path,
        None => default_root(),
    };
    if !root.is_absolute() {
        return Err(format!(
            "临时目录必须是绝对路径（实际 {}）：ASCII 目录语义依赖盘符，不跟随当前目录",
            root.display()
        ));
    }
    // 驱动器根没有父目录。`remove_owned_dir` 允许删"空目录"，所以空盘符根会被当成
    // 无主空目录整盘 `remove_dir_all`——这个形状永远不是临时目录，直接拒。
    if root.parent().is_none() {
        return Err(format!(
            "临时目录不能是驱动器根（{}）：本命令会递归清空自己的临时目录，盘符根一旦被当成\
             无主目录就会删掉整盘；请给一个子目录，例如 {}",
            root.display(),
            root.join("cordis-package-consumer").display()
        ));
    }
    // `..` 必须在拉起任何东西之前拒绝：canonicalize 只能解析已存在的路径段，
    // 靠它兜底等于把隔离判据交给运气（`C:\repo\target\..\..\x` 这类写法正是漏洞所在）。
    if root
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!(
            "临时目录不能含 `..`（无法静态判断它是否落在仓库内）：{}",
            root.display()
        ));
    }
    Ok(Parsed::Run(Options {
        root,
        keep,
        offline,
        reuse_dir,
    }))
}

/// 默认目录：`C:\cordis-package-consumer`（Windows）或系统临时目录下的同名子目录。
///
/// 必须是 ASCII：仓库根含中文，GNU ld 处理不了非 ASCII 路径，而消费者要真的链接。
fn default_root() -> PathBuf {
    if let Some(value) = std::env::var_os("CORDIS_PACKAGE_CONSUMER_DIR") {
        return PathBuf::from(value);
    }
    if cfg!(windows) {
        PathBuf::from(r"C:\cordis-package-consumer")
    } else {
        std::env::temp_dir().join("cordis-package-consumer")
    }
}

fn keep_from_env() -> bool {
    match std::env::var("CORDIS_PACKAGE_KEEP") {
        Ok(raw) => {
            let raw = raw.trim();
            !raw.is_empty() && raw != "0" && !raw.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

// ------------------------------------------------------------------ 主流程

struct Check {
    options: Options,
    repo: PathBuf,
    timings: Vec<(&'static str, Duration)>,
    /// prepare 的实际进度。失败时的措辞按这两个状态位选，而不是靠"目录里有没有
    /// marker"事后反推——反推分不清"本命令刚建目录但写 marker 失败"与"外来目录"。
    prepare_state: PrepareState,
}

/// prepare 走到的状态：目录是否已经由本命令建出来、marker 是否已经写成功。
#[derive(Clone, Copy, Debug, Default)]
struct PrepareState {
    created: bool,
    marked: bool,
}

/// 失败后"目录为什么留在那儿"的判定结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Retention {
    /// `--reuse-dir`：目录本来就不是本次建的，也不该清理。
    Reused,
    /// 本命令建了目录，也写好了 marker。
    OwnedMarked,
    /// 本命令建了目录，但 marker 没写成功。
    OwnedUnmarked,
    /// 本命令还没建目录，路径上却已经有东西（外来目录/上次失败的残留）。
    Foreign,
    /// 本命令还没建目录，路径上什么都没有。
    Absent,
}

/// 按 [`PrepareState`] 与目录是否存在判定保留措辞。
///
/// **不看** marker 文件：`root.join(ROOT_MARKER).exists()` 会把"本命令刚建目录、写
/// marker 失败"误报成外来目录，于是真正的残留被说成"不是本次运行的"。
fn retention(options: &Options, state: PrepareState) -> Retention {
    if options.reuse_dir {
        Retention::Reused
    } else if state.created {
        if state.marked {
            Retention::OwnedMarked
        } else {
            Retention::OwnedUnmarked
        }
    } else if options.root.exists() {
        Retention::Foreign
    } else {
        Retention::Absent
    }
}

struct Failure {
    stage: &'static str,
    message: String,
}

/// 一个打包出来的 `.crate`（复制到临时目录后的副本）。
struct Packaged {
    package: &'static str,
    file: PathBuf,
    bytes: u64,
}

/// 一个解包目录。
struct Unpacked {
    package: &'static str,
    dir: PathBuf,
}

struct Consumer {
    dir: PathBuf,
    manifest: PathBuf,
    exe: PathBuf,
}

/// `prepare` 的产物：版本，以及**本次打包开始的时间**（`.crate` 新鲜度的基准）。
struct Prepared {
    version: String,
    started: SystemTime,
}

/// `--reuse-dir` 的产物：现有 root 里已解包的目录与消费者工程。
struct Reused {
    unpacked: Vec<Unpacked>,
    consumer: Consumer,
}

struct Summary {
    root: PathBuf,
    packaged: Vec<Packaged>,
    unpacked: Vec<Unpacked>,
    probe: String,
    total: Duration,
    reused: bool,
}

impl Check {
    fn new(options: Options) -> Self {
        Self {
            options,
            repo: crate::workspace_root(),
            timings: Vec::new(),
            prepare_state: PrepareState::default(),
        }
    }

    fn root(&self) -> PathBuf {
        self.options.root.clone()
    }

    fn cargo(&self) -> String {
        std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
    }

    fn execute(&mut self) -> Result<Summary, Failure> {
        let started = Instant::now();
        if self.options.reuse_dir {
            let reused = self.stage("reuse", |check| check.reuse())?;
            self.stage("resolve", |check| check.resolve(&reused.consumer))?;
            self.stage("build", |check| check.build(&reused.consumer))?;
            let probe = self.stage("run", |check| check.probe(&reused.consumer))?;
            return Ok(Summary {
                root: self.root(),
                packaged: Vec::new(),
                unpacked: reused.unpacked,
                probe,
                total: started.elapsed(),
                reused: true,
            });
        }
        let prepared = self.stage("prepare", |check| check.prepare())?;
        let packaged = self.stage("package", |check| check.package(&prepared))?;
        let unpacked = self.stage("unpack", |check| check.unpack(&packaged))?;
        let consumer = self.stage("consumer", |check| check.write_consumer(&unpacked))?;
        self.stage("resolve", |check| check.resolve(&consumer))?;
        self.stage("build", |check| check.build(&consumer))?;
        let probe = self.stage("run", |check| check.probe(&consumer))?;
        Ok(Summary {
            root: self.root(),
            packaged,
            unpacked,
            probe,
            total: started.elapsed(),
            reused: false,
        })
    }

    /// 把一次阶段执行包起来：成功/失败都打印阶段名与耗时，失败带上阶段名向上抛。
    ///
    /// 阶段体可以 `&mut self`（prepare 要把自己的进度写进 [`Check::prepare_state`]，
    /// 失败措辞才有状态可依）。
    fn stage<T>(
        &mut self,
        name: &'static str,
        run: impl FnOnce(&mut Self) -> Result<T, String>,
    ) -> Result<T, Failure> {
        println!("[package-check] --- {name}");
        let started = Instant::now();
        let result = run(&mut *self);
        let elapsed = started.elapsed();
        self.timings.push((name, elapsed));
        match result {
            Ok(value) => {
                println!(
                    "[package-check] --- {name} ok（{:.1}s）",
                    elapsed.as_secs_f64()
                );
                Ok(value)
            }
            Err(message) => Err(Failure {
                stage: name,
                message,
            }),
        }
    }

    // -------------------------------------------------------------- 阶段体

    fn prepare(&mut self) -> Result<Prepared, String> {
        // 时间基准取在**任何**打包动作之前：`collect` 用它证明产物是本次写的。
        let started = SystemTime::now();
        let root = self.options.root.clone();
        let text = root.to_string_lossy().into_owned();
        // 先判隔离再判 ASCII：两条都违反时，"消费者在仓库里"是更根本的错
        // （本机仓库路径本身就含中文，两条会同时成立）。
        assert_outside_repo(&root, &self.repo)?;
        if !text.is_ascii() {
            return Err(format!(
                "临时目录必须是纯 ASCII 路径（GNU ld 处理不了非 ASCII 路径）：{text}；\
                 用 --dir 或 CORDIS_PACKAGE_CONSUMER_DIR 指向例如 C:\\cordis-package-consumer"
            ));
        }
        // 幂等：上次的残留整体清掉，不靠"目录是否为空"这种赌。但**不能**清空
        // 别人的目录（`--dir` 打错一个字符就是递归删除）：归属判定与末尾的
        // cleanup 共用 `remove_owned_dir`，两处口径不允许分叉。
        match remove_owned_dir(&root)? {
            RemoveOutcome::Removed => {
                println!("[package-check] 清理上次的临时目录 {}", root.display());
            }
            RemoveOutcome::Absent => {}
        }
        // 顺序有讲究：root → marker → 子目录。marker 必须紧跟 root，否则中途失败会
        // 留下"非空但没有 marker"的目录，下一次执行按外来目录永久拒绝。
        fs::create_dir_all(&root)
            .map_err(|err| format!("创建目录 {} 失败：{err}", root.display()))?;
        self.prepare_state.created = true;
        write_marker(&root)?;
        self.prepare_state.marked = true;
        for dir in [
            root.join("packaged"),
            root.join("unpacked"),
            root.join("consumer"),
        ] {
            fs::create_dir_all(&dir)
                .map_err(|err| format!("创建目录 {} 失败：{err}", dir.display()))?;
        }
        let version = crate::seal::workspace_version(&self.repo)?;
        println!(
            "[package-check] 版本 {version}；仓库 {}；临时目录 {}；offline={}",
            self.repo.display(),
            root.display(),
            self.options.offline
        );
        Ok(Prepared { version, started })
    }

    /// 仅调试：复用现有 root（跳过 prepare/package/unpack/消费者生成），只做
    /// resolve/build/run。
    ///
    /// 用途：给"改坏解包产物必须变红"提供可重复证据——打包链路被整体跳过，于是
    /// 之后的红只可能来自产物本身，而不是"源码没被重新打包"这类解释。
    fn reuse(&self) -> Result<Reused, String> {
        let root = self.root();
        if !root.exists() {
            return Err(format!(
                "--reuse-dir 要求目录已存在（先用 `package-check --keep` 生成一次）：{}",
                root.display()
            ));
        }
        if !root.join(ROOT_MARKER).exists() {
            return Err(format!(
                "--reuse-dir 只复用 package-check 自己的目录（缺 {ROOT_MARKER}）：{}；\
                 换一个 --dir，或先跑一次 `package-check --keep --dir {}`",
                root.display(),
                root.display()
            ));
        }
        assert_outside_repo(&root, &self.repo)?;
        let dir = root.join("consumer");
        let manifest = dir.join("Cargo.toml");
        if !manifest.exists() {
            return Err(format!(
                "--reuse-dir 没有找到上一次生成的消费者清单：{}（先跑一次 `package-check --keep`）",
                manifest.display()
            ));
        }
        let unpacked = self.scan_unpacked()?;
        println!(
            "[package-check] 复用目录 {}（跳过打包/解包/生成消费者；只做 resolve/build/run）",
            root.display()
        );
        Ok(Reused {
            unpacked,
            consumer: Consumer {
                exe: consumer_exe(&root),
                dir,
                manifest,
            },
        })
    }

    /// 列出 `root/unpacked` 下四个包的解包目录（目录名是 `<包名>-<版本>`，版本未知）。
    fn scan_unpacked(&self) -> Result<Vec<Unpacked>, String> {
        let base = self.root().join("unpacked");
        let entries = fs::read_dir(&base)
            .map_err(|err| format!("读取解包目录 {} 失败：{err}", base.display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        let mut found = Vec::new();
        for package in ALL_PACKAGES {
            let prefix = format!("{package}-");
            let dir = entries.iter().find(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().starts_with(&prefix))
                    .unwrap_or(false)
            });
            match dir {
                Some(dir) => found.push(Unpacked {
                    package,
                    dir: dir.clone(),
                }),
                None => {
                    return Err(format!(
                        "--reuse-dir 在 {} 里找不到 {package} 的解包目录",
                        base.display()
                    ))
                }
            }
        }
        Ok(found)
    }

    fn package(&self, prepared: &Prepared) -> Result<Vec<Packaged>, String> {
        let version = prepared.version.as_str();
        // 先删掉本次要产出的四个 .crate。`target/package` 跨运行保留，而 cargo 在
        // 版本未变、有人预置、或外层短路时可能不重写它们——对着上一次的产物判绿，
        // 会让"只改仓库源码"这条判据彻底失效。
        for package in ALL_PACKAGES {
            self.drop_stale_crate(package, version)?;
        }
        let mut args = vec!["package".to_string()];
        for package in HEAD_PACKAGES {
            args.push("-p".to_string());
            args.push((*package).to_string());
        }
        args.push("--allow-dirty".to_string());
        args.push("--locked".to_string());
        // 不带 --no-verify：这三个包的 .crate 会被**真实构建**一遍，
        // "包里的文件够不够构建"在这一步就被证明。
        self.with_network(&mut args);
        self.step_status(&self.cargo(), &args, &self.repo, &[])?;
        let mut packaged = Vec::with_capacity(ALL_PACKAGES.len());
        for package in HEAD_PACKAGES {
            packaged.push(self.collect(package, version, prepared.started)?);
        }
        packaged.extend(self.package_tail(prepared)?);
        Ok(packaged)
    }

    /// 删掉指定包的 `.crate`（不存在不算错）。
    fn drop_stale_crate(&self, package: &str, version: &str) -> Result<(), String> {
        let path = self.crate_path(package, version);
        match fs::remove_file(&path) {
            Ok(()) => {
                println!(
                    "[package-check] 删除上次残留的 .crate {}（保证本次产物是新的）",
                    path.display()
                );
                Ok(())
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(format!(
                "删除残留的 {} 失败（旧产物会让检查失去意义）：{err}",
                path.display()
            )),
        }
    }

    fn crate_path(&self, package: &str, version: &str) -> PathBuf {
        self.cargo_target_dir()
            .join("package")
            .join(format!("{package}-{version}.crate"))
    }

    /// 把 `.crate` 复制到临时目录（后续解包只读副本，检查过程不依赖 target 里别的残留）。
    fn collect(
        &self,
        package: &'static str,
        version: &str,
        not_before: SystemTime,
    ) -> Result<Packaged, String> {
        let source = self.crate_path(package, version);
        if !source.exists() {
            return Err(format!(
                "打包成功后找不到 {}（预期 cargo package 写到目标目录的 package/ 下）",
                source.display()
            ));
        }
        // 第二道判据：mtime 必须晚于本次打包开始时间。删残留是主线保证，这条负责
        // 揭穿"cargo 复用了旧产物"（例如外层短路、或有人把旧 .crate 又放了回来）。
        let modified = fs::metadata(&source)
            .and_then(|metadata| metadata.modified())
            .map_err(|err| {
                format!(
                    "读取 {} 的修改时间失败：{err}（无法确认它是本次打包的产物）",
                    source.display()
                )
            })?;
        if !is_fresh(modified, not_before) {
            return Err(format!(
                "{} 是上一次留下的产物（mtime 早于本次打包开始时间），拒绝使用：\
                 否则仓库源码怎么改都可能对着旧产物判绿",
                source.display()
            ));
        }
        let file = self
            .root()
            .join("packaged")
            .join(source.file_name().unwrap_or_default());
        fs::copy(&source, &file)
            .map_err(|err| format!("复制 {} → {} 失败：{err}", source.display(), file.display()))?;
        let bytes = fs::metadata(&file)
            .map_err(|err| format!("读取 {} 元数据失败：{err}", file.display()))?
            .len();
        println!(
            "[package-check] 打包产物 {}（{bytes} 字节，本次新写）",
            file.display()
        );
        Ok(Packaged {
            package,
            file,
            bytes,
        })
    }

    /// `rust-cordis-host` 的打包：**必须 `--no-verify` + 一份 `[patch.crates-io]`**。
    ///
    /// 三条实测约束（缺一条就红，别试图去掉）：
    /// 1. 打包会把 host 的 path 依赖改写成 registry 依赖
    ///    `rust-cordis-core = "^0.1.2"` / `rust-cordis-abi = "^0.1.2"`，而 crates.io 上
    ///    目前只有 0.1.0 / 0.1.1：`--verify` 会在 `target/package/` 里真实构建，必然
    ///    拉不到 0.1.2 → 必须 `--no-verify`；
    /// 2. 连**生成 .crate** 这一步也要重新解析依赖图（否则报
    ///    `failed to prepare local package for uploading`）→ 必须给一份 patch；
    /// 3. 这份 patch **只能指向 workspace 里的 path 包**：指向解包副本会引入一个新的
    ///    package id，需要改写仓库的 `Cargo.lock`，`--locked` 会直接拒（实测：
    ///    `cannot update the lock file ... because --locked was passed`，而改动这个
    ///    被跟踪的文件本身就不允许）。
    ///
    /// 所以"仓库源码不参与消费者构建"由另外两处保证，而不是靠这一步的 patch：
    /// 打包清单里不会留下 path（`unpack_into` 里逐条断言），消费者侧的依赖图只认
    /// 解包目录（`resolve` 阶段的 `cargo metadata` 判据）。
    fn package_tail(&self, prepared: &Prepared) -> Result<Vec<Packaged>, String> {
        let version = prepared.version.as_str();
        let patch = self.root().join("package-patch.toml");
        let mut text = format!(
            "# 只在这一次 `cargo package` 里生效：让 host/sdk 被打包时改写出来的 registry\n\
             # 依赖（^{version}）能被解析。它不进产物、不影响消费者。\n\
             #\n\
             # 为什么指 workspace 里的 path 包而不是解包副本：解包副本是一个新的\n\
             # package id，会要求改写仓库 Cargo.lock，而这一步必须带 --locked。\n\
             [patch.crates-io]\n"
        );
        for (package, relative) in [
            (CORE_PACKAGE, "crates/core"),
            (ABI_PACKAGE, "crates/abi"),
            (MACROS_PACKAGE, "crates/macros"),
        ] {
            let dir = self.repo.join(relative);
            if !dir.join("Cargo.toml").exists() {
                return Err(format!("{package} 的源码目录不存在：{}", dir.display()));
            }
            text.push_str(&format!(
                "{package} = {{ path = \"{}\" }}\n",
                toml_path(&dir)
            ));
        }
        fs::write(&patch, text).map_err(|err| format!("写 {} 失败：{err}", patch.display()))?;

        let mut args = vec!["package".to_string()];
        for package in TAIL_PACKAGES {
            args.push("-p".to_string());
            args.push((*package).to_string());
        }
        args.extend([
            "--no-verify".to_string(),
            "--allow-dirty".to_string(),
            "--locked".to_string(),
            "--config".to_string(),
            patch.to_string_lossy().into_owned(),
        ]);
        self.with_network(&mut args);
        self.step_status(&self.cargo(), &args, &self.repo, &[])?;
        let mut out = Vec::with_capacity(TAIL_PACKAGES.len());
        for package in TAIL_PACKAGES {
            out.push(self.collect(package, version, prepared.started)?);
        }
        Ok(out)
    }

    fn unpack(&self, crates: &[Packaged]) -> Result<Vec<Unpacked>, String> {
        crates.iter().map(|entry| self.unpack_into(entry)).collect()
    }

    fn unpack_into(&self, entry: &Packaged) -> Result<Unpacked, String> {
        let unpacked_root = self.root().join("unpacked");
        let folder = entry
            .file
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .ok_or_else(|| format!("无法从 {} 取出目录名", entry.file.display()))?;
        let dir = unpacked_root.join(&folder);
        let args = vec![
            "-xzf".to_string(),
            entry.file.to_string_lossy().into_owned(),
            "-C".to_string(),
            unpacked_root.to_string_lossy().into_owned(),
        ];
        self.step_status("tar", &args, &self.root(), &[])?;
        let manifest = dir.join("Cargo.toml");
        if !manifest.exists() {
            return Err(format!(
                "解包后缺少 {}（tar 把内容解到别处了？）",
                manifest.display()
            ));
        }
        // 判据③：**打包清单本身**不得残留源码 path（cargo 会把 path 依赖改写成
        // registry 依赖，一旦改写失灵，这里先红，而不是在消费者那边以诡异方式失败）。
        let text = fs::read_to_string(&manifest)
            .map_err(|err| format!("读取 {} 失败：{err}", manifest.display()))?;
        assert_artifact_only(
            &format!("{} 的打包清单", entry.package),
            &text,
            &dir,
            &unpacked_root,
            &self.repo,
        )?;
        println!("[package-check] 解包 {} → {}", entry.package, dir.display());
        Ok(Unpacked {
            package: entry.package,
            dir,
        })
    }

    fn write_consumer(&self, unpacked: &[Unpacked]) -> Result<Consumer, String> {
        let root = self.root();
        let unpacked_root = root.join("unpacked");
        let host = self.directory_of(unpacked, HOST_PACKAGE)?;
        let core = self.directory_of(unpacked, CORE_PACKAGE)?;
        let abi = self.directory_of(unpacked, ABI_PACKAGE)?;
        let macros = self.directory_of(unpacked, MACROS_PACKAGE)?;
        let sdk = self.directory_of(unpacked, SDK_PACKAGE)?;
        let version = unpacked_version(&host)?;

        let manifest_text = CONSUMER_MANIFEST
            .replace("{{VERSION}}", &version)
            .replace("{{HOST}}", &toml_path(&host))
            .replace("{{CORE}}", &toml_path(&core))
            .replace("{{ABI}}", &toml_path(&abi))
            .replace("{{MACROS}}", &toml_path(&macros))
            .replace("{{SDK}}", &toml_path(&sdk));
        let dir = root.join("consumer");
        let manifest = dir.join("Cargo.toml");
        fs::write(&manifest, &manifest_text)
            .map_err(|err| format!("写 {} 失败：{err}", manifest.display()))?;
        fs::create_dir_all(dir.join("src"))
            .map_err(|err| format!("创建 {} 失败：{err}", dir.join("src").display()))?;
        fs::write(dir.join("src").join("main.rs"), CONSUMER_MAIN)
            .map_err(|err| format!("写消费者 main.rs 失败：{err}"))?;

        // 判据①②的反后门部分：消费者清单里任何 path 都只能落在解包目录内，
        // 且全文不得出现仓库根或 "crates/" 这类源码相对写法。
        assert_artifact_only(
            "消费者 Cargo.toml",
            &manifest_text,
            &dir,
            &unpacked_root,
            &self.repo,
        )?;
        for package in CONSUMER_PACKAGES {
            let dir = self.directory_of(unpacked, package)?;
            if !manifest_text.contains(&toml_path(&dir)) {
                return Err(format!(
                    "消费者 Cargo.toml 没有引用 {package} 的解包目录 {}",
                    dir.display()
                ));
            }
        }
        println!(
            "[package-check] 生成隔离消费者 {}（反后门判据：无仓库源码 path）",
            manifest.display()
        );

        Ok(Consumer {
            exe: consumer_exe(&root),
            dir,
            manifest,
        })
    }

    fn resolve(&self, consumer: &Consumer) -> Result<(), String> {
        // 首跑没有 Cargo.lock，`--locked` 会直接报错，所以生成时不给它；
        // 之后的 build/metadata 都用 `--locked`，保证解析结果不会被悄悄改动。
        let mut args = vec![
            "generate-lockfile".to_string(),
            "--manifest-path".to_string(),
            consumer.manifest.to_string_lossy().into_owned(),
        ];
        self.with_network(&mut args);
        let captured = self.step_capture(&self.cargo(), &args, &consumer.dir, &[])?;
        if !captured.success {
            return Err(self.explain(
                "生成消费者 Cargo.lock",
                &captured,
                "离线缓存缺依赖：先联网执行 `cargo fetch` 补齐缓存，或给本命令加 --online",
            ));
        }

        let mut args = vec![
            "metadata".to_string(),
            "--format-version".to_string(),
            "1".to_string(),
            "--locked".to_string(),
            "--manifest-path".to_string(),
            consumer.manifest.to_string_lossy().into_owned(),
        ];
        self.with_network(&mut args);
        let captured = self.step_capture(&self.cargo(), &args, &consumer.dir, &[])?;
        if !captured.success {
            return Err(self.explain("读取消费者依赖图", &captured, ""));
        }
        let metadata: Value = serde_json::from_str(&captured.stdout)
            .map_err(|err| format!("cargo metadata 输出不是合法 JSON：{err}"))?;
        let packages = metadata
            .get("packages")
            .and_then(Value::as_array)
            .ok_or_else(|| "cargo metadata 缺少 packages 字段".to_string())?;

        // 判据②的路径比较必须两侧都 canonicalize：`cargo metadata` 给的是盘上真实
        // 路径，而仓库根与解包根来自参数/模板——大小写、8.3 短名、尾部分隔符都可能
        // 不一致，纯 `starts_with` 会漏（漏的方向恰好是"把源码当成产物"）。
        let repo = canonical(&self.repo);
        let unpacked_root = canonical(&self.root().join("unpacked"));
        let mut seen: Vec<&str> = Vec::new();
        for package in packages {
            let name = package.get("name").and_then(Value::as_str).unwrap_or("");
            let manifest = package
                .get("manifest_path")
                .and_then(Value::as_str)
                .unwrap_or("");
            if manifest.is_empty() {
                return Err(format!("cargo metadata 里 {name} 缺少 manifest_path"));
            }
            let manifest_path = canonical(Path::new(manifest));
            // 判据②：依赖图里任何一个包都不能来自仓库源码。
            if manifest_path.starts_with(&repo) {
                return Err(format!(
                    "依赖 {name} 解析到了仓库源码：{manifest}（消费者必须只用打包产物）"
                ));
            }
            if CONSUMER_PACKAGES.contains(&name) {
                if !manifest_path.starts_with(&unpacked_root) {
                    return Err(format!(
                        "依赖 {name} 不在解包目录内：{manifest}（registry 版本没被 patch 掉？）"
                    ));
                }
                seen.push(name);
            }
        }
        for package in CONSUMER_PACKAGES {
            if !seen.contains(package) {
                return Err(format!(
                    "消费者依赖图里缺少 {package}（cargo metadata 没解析到它，检查可能无从谈起）"
                ));
            }
        }
        println!("[package-check] 溯源通过：core/abi/macros/host/sdk 全部来自解包目录");
        Ok(())
    }

    fn build(&self, consumer: &Consumer) -> Result<(), String> {
        let mut args = vec![
            "build".to_string(),
            "--manifest-path".to_string(),
            consumer.manifest.to_string_lossy().into_owned(),
            "--locked".to_string(),
        ];
        self.with_network(&mut args);
        // 消费者独占的 target 目录：既不污染仓库 target，也保证产物确实来自本次构建。
        let target = self.root().join("target").to_string_lossy().into_owned();
        self.step_status(
            &self.cargo(),
            &args,
            &consumer.dir,
            &[("CARGO_TARGET_DIR", target)],
        )
    }

    fn probe(&self, consumer: &Consumer) -> Result<String, String> {
        if !consumer.exe.exists() {
            return Err(format!(
                "构建产物不存在：{}（build 阶段没有产出可执行文件？）",
                consumer.exe.display()
            ));
        }
        let path = consumer.exe.to_string_lossy().into_owned();
        let captured = self.step_capture(&path, &[], &consumer.dir, &[])?;
        if !captured.success {
            return Err(format!(
                "消费者退出码 {:?}（服务调用或关闭后归零的断言失败，细节见上方输出）",
                captured.code
            ));
        }
        for marker in REQUIRED_MARKERS {
            if !captured.stdout.contains(marker) {
                return Err(format!(
                    "消费者输出缺少判据行 {marker:?}（退出码 0 但没跑到断言，不能算通过）"
                ));
            }
        }
        Ok(captured.stdout)
    }

    // -------------------------------------------------------------- 小工具

    fn with_network(&self, args: &mut Vec<String>) {
        if self.options.offline {
            args.push("--offline".to_string());
        }
    }

    fn directory_of(&self, unpacked: &[Unpacked], package: &str) -> Result<PathBuf, String> {
        unpacked
            .iter()
            .find(|entry| entry.package == package)
            .map(|entry| entry.dir.clone())
            .ok_or_else(|| format!("{package} 还没有解包目录"))
    }

    fn cargo_target_dir(&self) -> PathBuf {
        match std::env::var_os("CARGO_TARGET_DIR") {
            Some(value) => {
                let path = PathBuf::from(value);
                if path.is_absolute() {
                    path
                } else {
                    self.repo.join(path)
                }
            }
            None => self.repo.join("target"),
        }
    }

    fn command_line(&self, program: &str, args: &[String]) -> String {
        if args.is_empty() {
            program.to_string()
        } else {
            format!("{program} {}", args.join(" "))
        }
    }

    /// 打印命令行并执行，输出直通（长构建要能看见进度）。
    fn step_status(
        &self,
        program: &str,
        args: &[String],
        dir: &Path,
        env: &[(&str, String)],
    ) -> Result<(), String> {
        println!("+ {}", self.command_line(program, args));
        let mut command = Command::new(program);
        command.args(args).current_dir(dir);
        for (key, value) in env {
            command.env(key, value);
        }
        match command.status() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!(
                "{} 退出码 {:?}",
                self.command_line(program, args),
                status.code()
            )),
            Err(err) => Err(format!("启动 {program} 失败：{err}")),
        }
    }

    /// 打印命令行并执行，**捕获**输出（需要读 stdout / 判定退出码的场景）。
    fn step_capture(
        &self,
        program: &str,
        args: &[String],
        dir: &Path,
        env: &[(&str, String)],
    ) -> Result<Captured, String> {
        println!("+ {}", self.command_line(program, args));
        let mut command = Command::new(program);
        command.args(args).current_dir(dir);
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command
            .output()
            .map_err(|err| format!("启动 {program} 失败：{err}"))?;
        let captured = Captured {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        print!("{}", captured.stdout);
        eprint!("{}", captured.stderr);
        Ok(captured)
    }

    fn explain(&self, what: &str, captured: &Captured, hint: &str) -> String {
        let tail = tail_lines(&captured.stderr, 20);
        let mut message = format!("{what} 失败（exit {:?}）", captured.code);
        if !tail.trim().is_empty() {
            message.push('\n');
            message.push_str(&tail);
        }
        if !hint.is_empty() {
            message.push('\n');
            message.push_str(hint);
        }
        message
    }

    fn cleanup(&self) {
        // 与 prepare 同一个归属判定：只删带自己 marker（或空）的目录。
        let root = &self.options.root;
        for attempt in 0..3 {
            match remove_owned_dir(root) {
                Ok(_) => {
                    println!("[package-check] 临时目录已清理：{}", root.display());
                    return;
                }
                Err(message) => {
                    if attempt < 2 {
                        std::thread::sleep(Duration::from_millis(200));
                    } else {
                        println!(
                            "[package-check] 警告：临时目录未能删除（下次执行会先识别归属再清空）：{message}"
                        );
                    }
                }
            }
        }
    }

    fn print_summary(&self, summary: &Summary) {
        println!();
        println!("{}", headline(summary.reused, summary.packaged.len()));
        println!("  临时目录：{}", summary.root.display());
        for entry in &summary.packaged {
            println!("  .crate：{}（{} 字节）", entry.file.display(), entry.bytes);
        }
        for entry in &summary.unpacked {
            println!("  解包：{}", entry.dir.display());
        }
        let timings = self
            .timings
            .iter()
            .map(|(name, elapsed)| format!("{name} {:.1}s", elapsed.as_secs_f64()))
            .collect::<Vec<_>>()
            .join(" / ");
        println!("  阶段耗时：{timings}");
        println!("  总耗时：{:.1}s", summary.total.as_secs_f64());
        for line in summary.probe.trim().lines() {
            println!("  服务调用：{line}");
        }
        println!("  判据①：消费者 Cargo.toml 的 path 全部落在解包目录内（无仓库源码 path）");
        println!("  判据②：cargo metadata 里 core/abi/macros/host/sdk 全部来自解包目录，依赖图无仓库源码");
        println!(
            "  未上传 registry：只执行 cargo package（不执行 publish）；CARGO_HOME = {}",
            cargo_home_note()
        );
    }
}

struct Captured {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// 摘要首行。
///
/// `--reuse-dir` 是调试复跑，**不是发布证据**：这一行不得出现"通过"，否则按
/// "通过"字样判绿的自动化会把它当成门禁结果。
fn headline(reused: bool, packaged: usize) -> String {
    if reused {
        "package-check 调试复跑完成（--reuse-dir，不作为发布证据）：复用现有解包产物与消费者 → \
         构建 → 跨 ABI 服务调用"
            .to_string()
    } else {
        format!(
            "package-check 通过：{packaged} 个 crate 打包 → 解包 → 隔离消费者构建 → 跨 ABI 服务调用"
        )
    }
}

/// 消费者可执行文件：`<root>/target/debug/cordis-package-consumer<EXE_SUFFIX>`。
///
/// 生成消费者与 `--reuse-dir` 复用目录两处都要它，拼法只留一份（`EXE_SUFFIX` 也一样）。
fn consumer_exe(root: &Path) -> PathBuf {
    root.join("target").join("debug").join(format!(
        "cordis-package-consumer{}",
        std::env::consts::EXE_SUFFIX
    ))
}

/// `CARGO_HOME` 的一句话说明（顺带确认没有凭据文件被写出来）。
fn cargo_home_note() -> String {
    match std::env::var_os("CARGO_HOME") {
        Some(value) => {
            let home = PathBuf::from(value);
            let credentials = ["credentials", "credentials.toml"]
                .iter()
                .any(|name| home.join(name).exists());
            format!(
                "{}（凭据文件：{}）",
                home.display(),
                if credentials {
                    "存在，本检查未读取也未写入"
                } else {
                    "不存在"
                }
            )
        }
        None => "(未设置)".to_string(),
    }
}

fn tail_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(count);
    lines[start..].join("\n")
}

/// TOML 里写路径：统一正斜杠（Windows 的反斜杠在 TOML 里是转义符）。
fn toml_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// 从解包目录的清单里读版本（`[package] version = "…"`）。
fn unpacked_version(dir: &Path) -> Result<String, String> {
    let manifest = dir.join("Cargo.toml");
    let text = fs::read_to_string(&manifest)
        .map_err(|err| format!("读取 {} 失败：{err}", manifest.display()))?;
    text.lines()
        .find_map(|line| {
            let line = line.trim();
            (line.starts_with("version = \"") && line.ends_with('"'))
                .then(|| {
                    line.trim_start_matches("version = \"")
                        .trim_end_matches('"')
                })
                .map(str::to_string)
        })
        .ok_or_else(|| format!("{} 缺少 version 字段", manifest.display()))
}

// ------------------------------------------------------------------ 归一化与归属

/// canonicalize，失败时原样返回。
///
/// 用途是**比较**路径：Windows 的 canonicalize 会给出盘上真实的大小写与长名
/// （`\\?\C:\…` 形式），于是盘符大小写、8.3 短名、重复分隔符的差异都消失。
/// 目标不存在时不制造错误（调用方要的是"能比就比"）。
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// 从 `path` 往上找到第一个存在的祖先（`path` 自己存在就是它自己）。
fn deepest_existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path;
    loop {
        if current.exists() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return path.to_path_buf(),
        }
    }
}

/// 隔离判据：临时目录不得落在仓库内。
///
/// 纯 `starts_with` 不够——`..`、盘符大小写、8.3 短名都能让"仓库里的目录"看起来
/// 在仓库外。这里拒绝 `..`，并把"已存在的最深祖先"与仓库根都 canonicalize 后再比：
/// 目录一旦落在仓库里，消费者会继承仓库的 `.cargo/config.toml`。
fn assert_outside_repo(root: &Path, repo: &Path) -> Result<(), String> {
    if root
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!(
            "临时目录不能含 `..`（无法静态判断它是否落在仓库内）：{}",
            root.display()
        ));
    }
    let resolved = canonical(&deepest_existing_ancestor(root));
    let repo = canonical(repo);
    if resolved.starts_with(&repo) {
        return Err(format!(
            "临时目录不能位于仓库内（消费者必须与源码隔离，否则源码里的 Cargo 配置会漏进来）：{}（规范化后 {}）",
            root.display(),
            resolved.display()
        ));
    }
    Ok(())
}

#[derive(Debug)]
enum RemoveOutcome {
    /// 目录本来就不在。
    Absent,
    /// 删掉了（本次运行自己建的，或上次留下的空目录）。
    Removed,
}

/// 删除**只属于 package-check 的**临时目录。
///
/// 归属只有两种：带 [`ROOT_MARKER`] 的目录，或空目录。其余一律拒绝——`--dir` 打错
/// 一个字符就是递归删除。prepare 与 cleanup 共用它，口径不允许分叉（cleanup 曾经
/// 无条件 `remove_dir_all`，配上未规范化的 `--dir` 就能删掉仓库里的目录）。
///
/// 错误信息区分两类："我们建的目录删不掉"（多半被占用）与"外来目录被拒绝"。
fn remove_owned_dir(root: &Path) -> Result<RemoveOutcome, String> {
    if !root.exists() {
        return Ok(RemoveOutcome::Absent);
    }
    if !root.join(ROOT_MARKER).exists() {
        let empty = fs::read_dir(root)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if !empty {
            return Err(format!(
                "目录已存在、非空，而且不是 package-check 的临时目录（缺 {ROOT_MARKER}）：\
                 这是外来目录，拒绝清空 {}（换一个 --dir，或确认内容后手工删除）",
                root.display()
            ));
        }
    }
    fs::remove_dir_all(root).map_err(|err| {
        format!(
            "删除 package-check 自己的临时目录 {} 失败（可能还有进程占用）：{err}",
            root.display()
        )
    })?;
    Ok(RemoveOutcome::Removed)
}

fn write_marker(root: &Path) -> Result<(), String> {
    fs::write(
        root.join(ROOT_MARKER),
        b"cargo xtask package-check temp root\n",
    )
    .map_err(|err| format!("写标记文件 {ROOT_MARKER} 失败（{}）：{err}", root.display()))
}

/// `.crate` 的 mtime 是否晚于本次打包开始时间（留出 [`FRESHNESS_SLACK`]）。
fn is_fresh(modified: SystemTime, started: SystemTime) -> bool {
    match modified.checked_add(FRESHNESS_SLACK) {
        Some(deadline) => deadline >= started,
        // 时间溢出（极端时钟）：宁可信其新——主线保证是"打包前删残留"。
        None => true,
    }
}

// ------------------------------------------------------------------ 反后门判据

/// 断言一份清单里没有任何指向仓库源码的依赖路径。
///
/// 规则（三条都要满足）：
/// 1. 全文不得出现仓库根路径；
/// 2. 绝对 `path` 必须落在 `allowed` 之内（消费者只能是解包目录），且真实存在；
/// 3. 相对 `path` 不得含 `..`，且必须真实存在（打包清单里的 `src/lib.rs` 这类）。
///
/// 不再单独找 `crates/` 这类字面 needle：真的指向源码树时，仓库根 needle 或
/// "相对 path 必须存在"必然会命中，多一条只会制造假红（例如某个包的文档提到它）。
fn assert_artifact_only(
    label: &str,
    text: &str,
    base: &Path,
    allowed: &Path,
    repo: &Path,
) -> Result<(), String> {
    let repo_text = repo.to_string_lossy().into_owned();
    let normalized = text.replace('\\', "/");
    if !repo_text.is_empty() && normalized.contains(&repo_text.replace('\\', "/")) {
        return Err(format!(
            "{label} 出现了仓库根路径 {repo_text}（消费者必须只用打包产物）"
        ));
    }
    let allowed = canonical(allowed);
    for value in path_values(text) {
        let path = Path::new(&value);
        if path.is_absolute() {
            if !canonical(path).starts_with(&allowed) {
                return Err(format!(
                    "{label} 出现指向解包目录之外的 path：{value}（只允许 {}）",
                    allowed.display()
                ));
            }
            if !path.exists() {
                return Err(format!("{label} 的 path 不存在：{value}"));
            }
            continue;
        }
        if value.contains("..") {
            return Err(format!(
                "{label} 出现越出包目录的相对 path：{value}（不能用 .. 退回源码树）"
            ));
        }
        let resolved = base.join(path);
        if !resolved.exists() {
            return Err(format!(
                "{label} 的相对 path 不存在：{value}（{}）",
                resolved.display()
            ));
        }
    }
    Ok(())
}

/// 抽出清单里所有 `path = "…"` 的值。
fn path_values(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut rest = text;
    while let Some(index) = rest.find("path") {
        rest = &rest[index + "path".len()..];
        let trimmed = rest.trim_start();
        let Some(after) = trimmed.strip_prefix('=') else {
            continue;
        };
        let after = after.trim_start();
        let Some(after) = after.strip_prefix('"') else {
            continue;
        };
        let Some(end) = after.find('"') else {
            continue;
        };
        values.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一棵最小的目录树：`repo/`（仓库根）、`unpacked/host/`（解包产物）、`consumer/src/`。
    struct Tree {
        root: PathBuf,
        repo: PathBuf,
        unpacked: PathBuf,
        consumer: PathBuf,
    }

    impl Tree {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "cordis-package-check-{label}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            let repo = root.join("repo");
            let unpacked = root.join("unpacked");
            let consumer = root.join("consumer");
            fs::create_dir_all(repo.join("crates").join("core")).unwrap();
            fs::create_dir_all(unpacked.join("host")).unwrap();
            fs::create_dir_all(consumer.join("src")).unwrap();
            fs::write(consumer.join("src").join("main.rs"), b"fn main() {}\n").unwrap();
            Self {
                root,
                repo,
                unpacked,
                consumer,
            }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn path_values_reads_every_path_key() {
        let values = path_values(
            "name = \"x\"\n[lib]\npath = \"src/lib.rs\"\n[dependencies.a]\npath = \"C:/x/y\"\n",
        );
        assert_eq!(values, vec!["src/lib.rs".to_string(), "C:/x/y".to_string()]);
    }

    #[test]
    fn a_manifest_pointing_at_the_unpacked_artifacts_passes() {
        let tree = Tree::new("ok");
        let text = format!(
            "[dependencies]\nhost = {{ path = \"{}\" }}\n[lib]\npath = \"src/main.rs\"\n",
            toml_path(&tree.unpacked.join("host"))
        );
        assert!(
            assert_artifact_only("消费者", &text, &tree.consumer, &tree.unpacked, &tree.repo)
                .is_ok()
        );
    }

    #[test]
    fn a_manifest_pointing_at_the_repo_root_fails() {
        let tree = Tree::new("repo-path");
        let text = format!(
            "[dependencies]\nhost = {{ path = \"{}\" }}\n",
            toml_path(&tree.repo.join("crates").join("core"))
        );
        let error =
            assert_artifact_only("消费者", &text, &tree.consumer, &tree.unpacked, &tree.repo)
                .expect_err("指向仓库根的 path 必须判红");
        assert!(error.contains("仓库根路径"), "{error}");
    }

    #[test]
    fn a_manifest_pointing_outside_the_unpacked_root_fails() {
        let tree = Tree::new("outside");
        let text = format!(
            "[dependencies]\nhost = {{ path = \"{}\" }}\n",
            toml_path(&tree.consumer.join("src"))
        );
        let error =
            assert_artifact_only("消费者", &text, &tree.consumer, &tree.unpacked, &tree.repo)
                .expect_err("解包目录之外的绝对 path 必须判红");
        assert!(error.contains("解包目录之外"), "{error}");
    }

    #[test]
    fn a_relative_path_escaping_the_package_fails() {
        let tree = Tree::new("escape");
        let error = assert_artifact_only(
            "消费者",
            "[dependencies]\nhost = { path = \"../host\" }\n",
            &tree.consumer,
            &tree.unpacked,
            &tree.repo,
        )
        .expect_err("`..` 必须判红");
        assert!(error.contains("越出包目录"), "{error}");
    }

    #[test]
    fn a_missing_relative_path_fails() {
        let tree = Tree::new("missing");
        let error = assert_artifact_only(
            "消费者",
            "[[test]]\nname = \"x\"\npath = \"tests/nope.rs\"\n",
            &tree.consumer,
            &tree.unpacked,
            &tree.repo,
        )
        .expect_err("不存在的相对 path 必须判红");
        assert!(error.contains("不存在"), "{error}");
    }

    #[test]
    fn a_relative_source_path_fails_because_it_does_not_exist() {
        // 删掉 `crates/` 字面 needle 之后，源码相对的写法仍然红——因为该相对 path
        // 在消费者目录里根本不存在，而"相对 path 必须存在"是保留的规则。
        let tree = Tree::new("source-relative");
        let error = assert_artifact_only(
            "消费者",
            "[dependencies]\nhost = { path = \"crates/host\" }\n",
            &tree.consumer,
            &tree.unpacked,
            &tree.repo,
        )
        .expect_err("源码相对写法必须判红");
        assert!(error.contains("不存在"), "{error}");
    }

    #[test]
    fn unpacked_version_reads_the_packaged_manifest() {
        let tree = Tree::new("version");
        let host = tree.unpacked.join("host");
        fs::write(
            host.join("Cargo.toml"),
            b"[package]\nname = \"rust-cordis-host\"\nversion = \"9.8.7\"\n",
        )
        .unwrap();
        assert_eq!(unpacked_version(&host).unwrap(), "9.8.7");
    }

    // ---------------------------------------------------------- 隔离与归属

    #[test]
    fn a_root_inside_the_repo_is_rejected() {
        let tree = Tree::new("inside");
        let inside = tree.repo.join("target").join("package-check");
        let error = assert_outside_repo(&inside, &tree.repo).expect_err("仓库内的目录必须判红");
        assert!(error.contains("不能位于仓库内"), "{error}");
    }

    #[test]
    fn a_root_below_a_dot_dot_component_is_rejected_before_any_io() {
        let tree = Tree::new("dotdot");
        // `repo/..` 在字符串上不以 repo 开头，canonicalize 之前无法判断。
        let sneaky = tree.repo.join("..").join("package-check");
        let error = assert_outside_repo(&sneaky, &tree.repo).expect_err("`..` 必须直接判红");
        assert!(error.contains("不能含 `..`"), "{error}");
    }

    #[test]
    fn a_sibling_root_is_accepted() {
        let tree = Tree::new("sibling");
        assert!(assert_outside_repo(&tree.consumer, &tree.repo).is_ok());
    }

    #[test]
    #[cfg(windows)]
    fn a_case_variant_of_a_repo_path_is_rejected() {
        // Windows 的 canonicalize 会还原盘上真实大小写，所以大写写法也必须判红。
        let tree = Tree::new("case");
        let shout = PathBuf::from(tree.repo.to_string_lossy().to_uppercase())
            .join("target")
            .join("package-check");
        let error = assert_outside_repo(&shout, &tree.repo).expect_err("大小写变体必须判红");
        assert!(error.contains("不能位于仓库内"), "{error}");
    }

    #[test]
    fn deepest_existing_ancestor_walks_up_to_the_nearest_directory() {
        let tree = Tree::new("ancestor");
        let target = tree.repo.join("a").join("b");
        fs::create_dir_all(&target).unwrap();
        let deep = target.join("c").join("d");
        assert_eq!(deepest_existing_ancestor(&deep), target);
        assert_eq!(deepest_existing_ancestor(&tree.repo), tree.repo);
    }

    #[test]
    fn remove_owned_dir_refuses_a_foreign_directory() {
        let tree = Tree::new("foreign");
        fs::write(tree.root.join("keep.txt"), b"keep").unwrap();
        let error = remove_owned_dir(&tree.root).expect_err("外来目录必须拒绝");
        assert!(error.contains("拒绝清空"), "{error}");
        assert!(
            tree.root.join("keep.txt").exists(),
            "拒绝的目录必须原样保留"
        );
    }

    #[test]
    fn remove_owned_dir_deletes_a_marked_directory() {
        let tree = Tree::new("marked");
        write_marker(&tree.root).unwrap();
        assert!(matches!(
            remove_owned_dir(&tree.root),
            Ok(RemoveOutcome::Removed)
        ));
        assert!(!tree.root.exists(), "带标记的目录必须被删掉");
    }

    #[test]
    fn remove_owned_dir_deletes_an_empty_directory_and_tolerates_a_missing_one() {
        let tree = Tree::new("empty");
        let empty = tree.root.join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(matches!(
            remove_owned_dir(&empty),
            Ok(RemoveOutcome::Removed)
        ));
        assert!(!empty.exists());
        assert!(matches!(
            remove_owned_dir(&empty),
            Ok(RemoveOutcome::Absent)
        ));
    }

    // ---------------------------------------------------------- 新鲜度

    #[test]
    fn fresh_crates_have_a_two_second_slack() {
        // 余量必须覆盖 FAT/exFAT/网络盘的 2s 粒度，否则会把刚写的产物误判成旧的。
        assert_eq!(FRESHNESS_SLACK, Duration::from_secs(2));
    }

    #[test]
    fn a_crate_from_before_the_packaging_start_is_stale() {
        let now = SystemTime::now();
        assert!(
            !is_fresh(now - Duration::from_secs(60), now),
            "旧产物必须判旧"
        );
        assert!(is_fresh(now, now), "刚写的产物必须判新");
        // 文件系统的 mtime 粒度可能到 2s：留一点余量，避免误红。
        assert!(is_fresh(now - Duration::from_millis(500), now));
    }

    // ---------------------------------------------------------- 清单单一来源

    #[test]
    fn package_lists_are_derived_from_one_table() {
        // 清残留/新鲜度按 ALL_PACKAGES 走：任何一个包只写在别处都会静默漏删。
        assert_eq!(ALL_PACKAGES.len(), 5);
        // 底层包在前，依赖内部包的 host/sdk 在后，两张表合起来正好覆盖全部。
        assert_eq!(
            HEAD_PACKAGES.len() + TAIL_PACKAGES.len(),
            ALL_PACKAGES.len()
        );
        for package in TAIL_PACKAGES {
            assert!(!HEAD_PACKAGES.contains(package));
        }
        // 消费者子集覆盖全部 5 个可发布包（含 sdk 与过程宏）。
        for package in CONSUMER_PACKAGES {
            assert!(
                ALL_PACKAGES.contains(package),
                "{package} 不在 ALL_PACKAGES 里"
            );
        }
        assert_eq!(
            CONSUMER_PACKAGES,
            &[
                CORE_PACKAGE,
                ABI_PACKAGE,
                MACROS_PACKAGE,
                HOST_PACKAGE,
                SDK_PACKAGE,
            ][..]
        );
    }

    #[test]
    fn consumer_exe_lives_under_the_root_target_dir() {
        let tree = Tree::new("exe");
        let exe = consumer_exe(&tree.root);
        assert!(
            exe.starts_with(tree.root.join("target")),
            "{}",
            exe.display()
        );
        assert!(
            exe.file_name()
                .map(|name| name
                    .to_string_lossy()
                    .starts_with("cordis-package-consumer"))
                .unwrap_or(false),
            "{}",
            exe.display()
        );
    }

    // ---------------------------------------------------------- 摘要措辞

    #[test]
    fn the_reuse_dir_summary_is_not_worded_as_a_pass() {
        // 自动化按"通过"字样判绿：调试复跑必须自证不是门禁结果。
        let reused = headline(true, 0);
        assert!(!reused.contains("通过"), "{reused}");
        assert!(reused.contains("--reuse-dir"), "{reused}");
        assert!(reused.contains("不作为发布证据"), "{reused}");
        let full = headline(false, 4);
        assert!(full.contains("package-check 通过：4 个 crate"), "{full}");
    }

    // ---------------------------------------------------------- 失败保留措辞

    #[test]
    fn failure_reporting_uses_state_flags_not_the_marker_file() {
        let tree = Tree::new("retention");
        let options = |root: PathBuf, reuse_dir: bool| Options {
            root,
            keep: None,
            offline: true,
            reuse_dir,
        };
        let root = tree.root.clone();
        let untouched = PrepareState::default();
        // 本命令建好目录、也写好了 marker。
        assert_eq!(
            retention(
                &options(root.clone(), false),
                PrepareState {
                    created: true,
                    marked: true
                }
            ),
            Retention::OwnedMarked
        );
        // 刚建目录但写 marker 失败：目录是本次运行的，绝不能报成"不是本次运行的"。
        assert_eq!(
            retention(
                &options(root.clone(), false),
                PrepareState {
                    created: true,
                    marked: false
                }
            ),
            Retention::OwnedUnmarked
        );
        // 盘上有目录、但本次没走到建目录（外来目录，或上次失败留下的）。
        assert_eq!(
            retention(&options(root.clone(), false), untouched),
            Retention::Foreign
        );
        // 路径上什么都没有，且本命令没建目录。
        assert_eq!(
            retention(&options(root.join("missing"), false), untouched),
            Retention::Absent
        );
        // `--reuse-dir` 不认领目录，也不清理。
        assert_eq!(
            retention(&options(root, true), untouched),
            Retention::Reused
        );
    }

    // ---------------------------------------------------------- 参数

    #[test]
    fn parse_rejects_a_dir_with_a_parent_component() {
        // 绝对路径 + `..`：canonicalize 之前无法判断它是否落在仓库内，必须直接拒。
        let sneaky = std::env::temp_dir()
            .join("cordis-package-check-sneaky")
            .join("..")
            .join("elsewhere");
        let args = vec!["--dir".to_string(), sneaky.to_string_lossy().into_owned()];
        let error = match parse(&args) {
            Err(message) => message,
            Ok(_) => panic!("含 `..` 的 --dir 必须在解析阶段被拒"),
        };
        assert!(error.contains("不能含 `..`"), "{error}");
    }

    #[test]
    fn parse_rejects_a_drive_root() {
        // 驱动器根没有父目录：`remove_owned_dir` 允许删空目录，空的盘符根就会被
        // `remove_dir_all` 整盘删掉。
        let drive_root = if cfg!(windows) { "C:\\" } else { "/" };
        let error = match parse(&["--dir".to_string(), drive_root.to_string()]) {
            Err(message) => message,
            Ok(_) => panic!("驱动器根必须被拒"),
        };
        assert!(error.contains("驱动器根"), "{error}");
    }

    #[test]
    fn parse_records_where_keep_came_from() {
        let parsed = parse(&["--keep".to_string(), "--reuse-dir".to_string()]);
        let Ok(Parsed::Run(options)) = parsed else {
            panic!("--keep/--reuse-dir 必须解析成运行参数");
        };
        assert!(matches!(options.keep, Some(KeepSource::Flag)));
        assert!(options.reuse_dir);
        // 保留理由要按来源如实打印（环境变量与开关不是同一个旋钮）。
        assert_eq!(KeepSource::Flag.describe(), "--keep");
        assert_eq!(KeepSource::Env.describe(), "CORDIS_PACKAGE_KEEP");
    }
}
