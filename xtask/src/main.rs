//! 跨平台构建入口：`cargo xtask <command>`。
//!
//! 仓库里不保存代理、绝对 target 目录或平台专用链接参数；本机差异放在
//! `cargo.cmd`（本地便利包装）或用户级 Cargo 配置里。

use std::path::PathBuf;
use std::process::{Command, ExitCode};

mod contract;
mod inventory;
mod matrix;
mod package_check;
mod seal;

const USAGE: &str = "\
cargo xtask <命令>

命令:
  fmt              cargo fmt --all -- --check
  clippy           cargo clippy --workspace --all-targets --all-features --locked
                   -- -D warnings
  test             cargo test --workspace --all-targets --locked
  consumer         测试独立服务消费者（只使用公开 path 依赖）
  build-plugins    构建 greeting / hello / harness-tool 三个 cdylib
  demo             构建插件并运行 demo
  e2e              构建插件并运行 harness-spike
  ci               fmt + inventory + docs + matrix + clippy(--all-features)
                   + check(--all-features) + test + doctest + consumer + demo + e2e
  docs [path]      检查支持契约的保证索引（默认 docs/support-contract.md）
  inventory [--write|--check] [path]
                   检查/写入公开 API 清单（默认 docs/api-inventory.md）
  matrix [support] [platform]
                   检查保证 × 平台矩阵结构（默认 docs/support-contract.md +
                   docs/platform-matrix.md；不验证真实平台结果）
  seal [--out <dir>] [--root <workspace>]
                   构建宿主目标 release 插件并封存（源码树 / 工具链 / ABI /
                   SHA256），保存同版本 inspect 证据；--root 可为历史 tag worktree
  seal --verify <dir> [--require-v2]
                   只读校验预期 artifact 集合、SHA256、smoke 与 inspect 证据
                   --require-v2 拒绝把 /2 伪装成 /1 绕过 provenance
  seal-consume <dir>
                   用正式封存的 allow-list 跑独立消费者（发布门禁，不进入 ci）
  package-check [--keep] [--offline|--online] [--reuse-dir] [--dir <绝对路径>]
                   打包 core/abi/macros/host 四个 .crate → 解包到 ASCII 临时目录
                   → 用**产物**（不是仓库源码 path）构建一个隔离消费者 → 跑一次
                   跨 ABI 服务调用（断言返回值与关闭后实例归零）；失败保留目录
                   并打印路径。反后门判据：消费者清单里不得出现仓库源码 path
                   （文本 + cargo metadata 双向核对）。默认 --locked --offline，
                   --online 才联网（`--offline` 与默认等价，保留它只为在默认将来
                   改成联网时仍能显式钉住离线）；
                   --keep 或 CORDIS_PACKAGE_KEEP=1 保留目录；
                   CORDIS_PACKAGE_CONSUMER_DIR 指定临时根目录（不进入 ci）
                   --dir 必须是绝对路径、不含 `..`、不是驱动器根、且在仓库之外
                   （canonicalize 后比较）；打包前会删掉目标 .crate 并断言产物是
                   本次新写的
                   --reuse-dir（仅调试）跳过准备/打包/解包，复用现有临时目录
                   （须带 package-check 标记）只做 resolve/build/run；用于证明
                   改坏解包产物必然变红（成功行明写“不作为发布证据”，不是门禁结果）
  soak [cycles]    运行被忽略的宿主资源 soak（默认 1000 轮；不进入 ci）
                   位置参数只影响主 soak（CORDIS_SOAK_CYCLES）；真 DLL 的
                   open_staged soak 由 CORDIS_STAGED_SOAK_CYCLES 控制（默认 200）
                   运行前逐 target 做 `--ignored --list` 注册检查
  dhat             跑 dhat 堆剖析（--features dhat-heap + --list 守卫 + stats；不进入 ci）
  doctor           打印本机构建环境诊断
";

fn main() -> ExitCode {
    let command = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "help".to_string());
    let code = match command.as_str() {
        "fmt" => fmt_check(),
        "clippy" => run(&[
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--locked",
            "--",
            "-D",
            "warnings",
        ]),
        "test" => run(&["test", "--workspace", "--all-targets", "--locked"]),
        "consumer" => consumer_check(),
        "build-plugins" => build_plugins(),
        "demo" => demo_check(),
        "e2e" => build_then(&["run", "--locked", "-p", "harness-spike"]),
        "ci" => ci(),
        "docs" => docs_check(std::env::args().nth(2)),
        "inventory" => inventory_command(std::env::args().skip(2).collect()),
        "matrix" => matrix_command(std::env::args().skip(2).collect()),
        "seal" => seal::command(std::env::args().skip(2).collect()),
        "seal-consume" => seal_consume(std::env::args().skip(2).collect()),
        "package-check" => package_check::command(std::env::args().skip(2).collect()),
        "soak" => soak_command(std::env::args().skip(2).collect()),
        "dhat" => dhat_command(std::env::args().skip(2).collect()),
        "doctor" => {
            doctor();
            0
        }
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            0
        }
        other => {
            eprintln!("未知命令 \"{other}\"\n\n{USAGE}");
            2
        }
    };
    ExitCode::from(code.clamp(0, 255) as u8)
}

fn fmt_check() -> i32 {
    let code = run(&["fmt", "--all", "--", "--check"]);
    if code != 0 {
        return code;
    }
    // 独立夹具 crate，`cargo fmt --all` 不会覆盖它们。
    for manifest in [
        "tests/fixtures/abi-v1/Cargo.toml",
        "tests/fixtures/abi-error/Cargo.toml",
        "tests/fixtures/abi-err-v1/Cargo.toml",
        "tests/consumer-service/Cargo.toml",
    ] {
        let code = run(&["fmt", "--manifest-path", manifest, "--", "--check"]);
        if code != 0 {
            return code;
        }
    }
    0
}

fn ci() -> i32 {
    let code = fmt_check();
    if code != 0 {
        return code;
    }
    let code = inventory_check(None);
    if code != 0 {
        return code;
    }
    let code = docs_check(None);
    if code != 0 {
        return code;
    }
    let code = matrix_check(None, None);
    if code != 0 {
        return code;
    }
    for args in [
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            // `--all-features`：历史上 `dhat-heap` 这类只在 feature 下出现的代码
            // 曾带着 unused import / 死代码合进来，默认 feature 的 clippy 看不见。
            "--all-features",
            "--locked",
            "--",
            "-D",
            "warnings",
        ][..],
        // 只编译不运行：`dhat-heap` 这类 feature 不开时测试会被 `#![cfg]` 整块裁掉，
        // 缺编译验证就会在发布前才炸。
        &[
            "check",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--locked",
        ][..],
        &["test", "--workspace", "--all-targets", "--locked"][..],
        // `--all-targets` 不含 doctest：文档示例必须单独跑，否则 `ignore`/改坏都无声。
        &["test", "--workspace", "--doc", "--locked"][..],
    ] {
        let code = run(args);
        if code != 0 {
            return code;
        }
    }
    let code = consumer_check();
    if code != 0 {
        return code;
    }
    let code = demo_check();
    if code != 0 {
        return code;
    }
    run(&["run", "--locked", "-p", "harness-spike"])
}

fn resolve_repo_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        inventory::repo_root().join(path)
    }
}

fn docs_check(path: Option<String>) -> i32 {
    let path = path
        .map(PathBuf::from)
        .map(resolve_repo_path)
        .unwrap_or_else(contract::default_contract_path);
    let violations = contract::check_path(&path);
    if violations.is_empty() {
        println!("支持契约检查通过：{}", path.display());
        0
    } else {
        eprintln!(
            "支持契约检查失败（{} 条，{}）：",
            violations.len(),
            path.display()
        );
        for violation in &violations {
            eprintln!("  - {violation}");
        }
        1
    }
}

fn inventory_command(args: Vec<String>) -> i32 {
    let mut mode: Option<bool> = None;
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "--write" => {
                if mode.replace(true).is_some() {
                    eprintln!("--write 与 --check 只能选一个\n\n{USAGE}");
                    return 2;
                }
            }
            "--check" => {
                if mode.replace(false).is_some() {
                    eprintln!("--write 与 --check 只能选一个\n\n{USAGE}");
                    return 2;
                }
            }
            other if other.starts_with('-') => {
                eprintln!("未知参数 \"{other}\"\n\n{USAGE}");
                return 2;
            }
            other => path = Some(PathBuf::from(other)),
        }
    }
    let write = mode.unwrap_or(false);
    let path = path
        .map(resolve_repo_path)
        .unwrap_or_else(inventory::default_path);
    if write {
        match inventory::write_path(&path) {
            Ok(()) => {
                println!("API 清单已写入：{}", path.display());
                0
            }
            Err(err) => {
                eprintln!("API 清单写入失败：{err}");
                1
            }
        }
    } else {
        inventory_check(Some(path))
    }
}

fn inventory_check(path: Option<PathBuf>) -> i32 {
    let path = path.unwrap_or_else(inventory::default_path);
    let violations = inventory::check_path(&path);
    if violations.is_empty() {
        println!("API 清单检查通过：{}", path.display());
        0
    } else {
        eprintln!(
            "API 清单检查失败（{} 条，{}）：",
            violations.len(),
            path.display()
        );
        for violation in &violations {
            eprintln!("  - {violation}");
        }
        1
    }
}

fn matrix_command(args: Vec<String>) -> i32 {
    let mut paths = Vec::new();
    for arg in args {
        if arg.starts_with('-') {
            eprintln!("未知参数 \"{arg}\"\n\n{USAGE}");
            return 2;
        }
        paths.push(PathBuf::from(arg));
        if paths.len() > 2 {
            eprintln!("matrix 最多接受 support 与 platform 两个路径\n\n{USAGE}");
            return 2;
        }
    }
    let support = paths.first().cloned();
    let platform = paths.get(1).cloned();
    matrix_check(support, platform)
}

fn matrix_check(support: Option<PathBuf>, platform: Option<PathBuf>) -> i32 {
    let support = support
        .map(resolve_repo_path)
        .unwrap_or_else(matrix::default_support_path);
    let platform = platform
        .map(resolve_repo_path)
        .unwrap_or_else(matrix::default_matrix_path);
    let errors = matrix::check_paths(&support, &platform);
    if errors.is_empty() {
        println!(
            "平台矩阵结构检查通过（不验证真实平台结果）：{} + {}",
            support.display(),
            platform.display()
        );
        0
    } else {
        eprintln!(
            "平台矩阵检查失败（{} 条，{} + {}）：",
            errors.len(),
            support.display(),
            platform.display()
        );
        for error in errors {
            eprintln!("  - {error}");
        }
        1
    }
}

fn build_plugins() -> i32 {
    run(&[
        "build",
        "--locked",
        "-p",
        "plugin-greeting",
        "-p",
        "plugin-hello",
        "-p",
        "plugin-harness-tool",
    ])
}

fn build_then(args: &[&str]) -> i32 {
    let code = build_plugins();
    if code == 0 {
        run(args)
    } else {
        code
    }
}

/// 一个必须存在的 `#[ignore]` 集成测试目标。
///
/// `cargo test … -- --ignored` 在 0 个 ignored 测试时**仍然退出 0**，所以测试被删、
/// 改名、或去掉 `#[ignore]` 之后整条命令会变成假绿。运行前先按 target 逐个 `--list`。
struct IgnoredTarget {
    package: &'static str,
    target: &'static str,
    test: &'static str,
    /// 该测试只在开启这个 feature 时才被编译（`dhat_profile` 要 dhat 全局分配器，
    /// 不开 feature 时 `#![cfg(feature = "dhat-heap")]` 会把整个 target 裁成空壳）。
    feature: Option<&'static str>,
}

const SOAK_TARGET: IgnoredTarget = IgnoredTarget {
    package: "rust-cordis-host",
    target: "soak",
    test: "repeated_soak_cycles_leave_no_residue",
    feature: None,
};

const STAGED_SOAK_TARGET: IgnoredTarget = IgnoredTarget {
    package: "rust-cordis-host",
    target: "open_staged_soak",
    test: "real_open_staged_cycles_return_to_baseline",
    feature: None,
};

const DHAT_TARGET: IgnoredTarget = IgnoredTarget {
    package: "rust-cordis-host",
    target: "dhat_profile",
    test: "real_open_staged_path_keeps_live_heap_bounded",
    feature: Some("dhat-heap"),
};

/// 目标测试必须注册为 `#[ignore]` 测试；否则拒绝运行（避免空跑被当成通过）。
fn require_ignored_test(spec: &IgnoredTarget) -> Result<(), i32> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut args = vec![
        "test",
        "-p",
        spec.package,
        "--test",
        spec.target,
        "--locked",
    ];
    if let Some(feature) = spec.feature {
        args.extend(["--features", feature]);
    }
    args.extend(["--", "--ignored", "--list"]);
    match Command::new(&cargo)
        .args(&args)
        .current_dir(workspace_root())
        .output()
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // 行级等值：`foo_x: test` 这种后缀同名不该被当成 `foo: test`。
            let expected = format!("{}: test", spec.test);
            if stdout.lines().any(|line| line.trim_end() == expected) {
                Ok(())
            } else {
                eprintln!(
                    "{} 目标里的 {} 未注册为 #[ignore] 测试；拒绝运行空的 {} 测试\n{}",
                    spec.target,
                    spec.test,
                    spec.target,
                    stdout.trim_end()
                );
                Err(1)
            }
        }
        Ok(output) => {
            eprintln!(
                "列出 {} 目标测试失败（exit {:?}）\n{}",
                spec.target,
                output.status.code().unwrap_or(1),
                String::from_utf8_lossy(&output.stderr).trim_end()
            );
            Err(output.status.code().unwrap_or(1))
        }
        Err(err) => {
            eprintln!("执行 {cargo} 失败: {err}");
            Err(1)
        }
    }
}

/// 从环境变量读一个正整数轮次；未设置时用默认值。`0` / 非法值一律拒绝，
/// 否则"0 轮"会让 soak 静默通过。
fn positive_cycles(var: &str, default: u64) -> Result<u64, String> {
    match std::env::var(var) {
        Ok(raw) => raw
            .parse::<u64>()
            .ok()
            .filter(|cycles| *cycles > 0)
            .ok_or_else(|| format!("{var} 必须是正整数，实际 \"{raw}\"")),
        Err(_) => Ok(default),
    }
}

fn soak_command(args: Vec<String>) -> i32 {
    if matches!(args.as_slice(), [flag] if flag == "-h" || flag == "--help") {
        print!("{USAGE}");
        return 0;
    }
    if args.len() > 1 {
        eprintln!("soak 最多接受一个 cycles 参数\n\n{USAGE}");
        return 2;
    }
    let cycles = match args.first() {
        Some(raw) if raw.starts_with('-') => {
            eprintln!("soak 未知参数 \"{raw}\"\n\n{USAGE}");
            return 2;
        }
        Some(raw) => match raw.parse::<u64>() {
            Ok(cycles) if cycles > 0 => cycles,
            _ => {
                eprintln!("soak cycles 必须是正整数，实际 \"{raw}\"\n\n{USAGE}");
                return 2;
            }
        },
        None => match positive_cycles("CORDIS_SOAK_CYCLES", 1000) {
            Ok(cycles) => cycles,
            Err(message) => {
                eprintln!("{message}\n\n{USAGE}");
                return 2;
            }
        },
    };
    // 真 DLL 的 `open_staged` soak 读的是**另一个**环境变量。这里必须显式读取并打印，
    // 否则 1M 轮的 nightly 里真 DLL 只跑默认的 200 轮，而且没人看得出来。
    let staged_cycles = match positive_cycles("CORDIS_STAGED_SOAK_CYCLES", 200) {
        Ok(cycles) => cycles,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            return 2;
        }
    };
    for spec in [&SOAK_TARGET, &STAGED_SOAK_TARGET] {
        if let Err(code) = require_ignored_test(spec) {
            return code;
        }
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    println!(
        "+ {cargo} test -p rust-cordis-host --test soak --test open_staged_soak --locked -- --ignored --nocapture (CORDIS_SOAK_CYCLES={cycles}, CORDIS_STAGED_SOAK_CYCLES={staged_cycles})"
    );
    match Command::new(&cargo)
        .args([
            "test",
            "-p",
            "rust-cordis-host",
            "--test",
            "soak",
            "--test",
            "open_staged_soak",
            "--locked",
            "--",
            "--ignored",
            "--nocapture",
        ])
        .env("CORDIS_SOAK_CYCLES", cycles.to_string())
        .env("CORDIS_STAGED_SOAK_CYCLES", staged_cycles.to_string())
        .current_dir(workspace_root())
        .status()
    {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("执行 {cargo} 失败: {err}");
            1
        }
    }
}

/// dhat 堆剖析：开 feature → `--list` 守卫 → 执行（stats 由测试自己打印，需 --nocapture）。
fn dhat_command(args: Vec<String>) -> i32 {
    if matches!(args.as_slice(), [flag] if flag == "-h" || flag == "--help") {
        print!("{USAGE}");
        return 0;
    }
    if let Some(unknown) = args.first() {
        eprintln!("dhat 不接受参数，实际 \"{unknown}\"\n\n{USAGE}");
        return 2;
    }
    if let Err(code) = require_ignored_test(&DHAT_TARGET) {
        return code;
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    println!(
        "+ {cargo} test -p rust-cordis-host --test dhat_profile --locked --features dhat-heap -- --ignored --nocapture（stats 见 dhat: 行）"
    );
    match Command::new(&cargo)
        .args([
            "test",
            "-p",
            "rust-cordis-host",
            "--test",
            "dhat_profile",
            "--locked",
            "--features",
            "dhat-heap",
            "--",
            "--ignored",
            "--nocapture",
        ])
        .current_dir(workspace_root())
        .status()
    {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("执行 {cargo} 失败: {err}");
            1
        }
    }
}

/// 用正式封存的 allow-list 跑独立消费者；发布门禁，不进入普通 ci。
fn seal_consume(args: Vec<String>) -> i32 {
    if args.len() != 1 {
        eprintln!("seal-consume 需要且只需要一个封存目录\n\n{USAGE}");
        return 2;
    }
    let dir = PathBuf::from(&args[0]);
    let dir = if dir.is_absolute() {
        dir
    } else {
        workspace_root().join(dir)
    };
    for required in ["seal.json", "allow-list.json", "plugins"] {
        if !dir.join(required).exists() {
            eprintln!(
                "封存目录缺少 {required}：{}（先运行 `cargo xtask seal --verify`）",
                dir.display()
            );
            return 1;
        }
    }
    if let Err(errors) = seal::verify_for_consume(&dir) {
        eprintln!("封存校验失败：{}", errors.join("；"));
        return 1;
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(&cargo)
        .args([
            "test",
            "--manifest-path",
            "tests/consumer-service/Cargo.toml",
            "--test",
            "formal_seal",
            "--locked",
        ])
        .env("CORDIS_CONSUMER_SEAL_DIR", &dir)
        .current_dir(workspace_root())
        .status();
    match status {
        Ok(status) if status.success() => 0,
        Ok(status) => {
            eprintln!("正式封存消费者测试失败：{status}");
            1
        }
        Err(err) => {
            eprintln!("运行正式封存消费者测试失败：{err}");
            1
        }
    }
}

/// 独立服务消费者：独立 workspace，只通过公开 path 依赖使用 SDK。
fn consumer_check() -> i32 {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let build = Command::new(&cargo)
        .args([
            "build",
            "--locked",
            "-p",
            "plugin-greeting",
            "-p",
            "plugin-hello",
        ])
        .current_dir(workspace_root())
        .status();
    if !matches!(build, Ok(status) if status.success()) {
        eprintln!("构建真实 ABI 消费者插件失败：{build:?}");
        return 1;
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"));
    let target = if target.is_absolute() {
        target
    } else {
        workspace_root().join(target)
    };
    let status = Command::new(&cargo)
        .args([
            "test",
            "--manifest-path",
            "tests/consumer-service/Cargo.toml",
            "--locked",
        ])
        .env("CORDIS_CONSUMER_PLUGIN_DIR", target.join("debug"))
        .current_dir(workspace_root())
        .status();
    match status {
        Ok(status) if status.success() => 0,
        Ok(status) => {
            eprintln!("独立消费者测试失败：{status}");
            1
        }
        Err(err) => {
            eprintln!("运行独立消费者测试失败：{err}");
            1
        }
    }
}

/// 跑 demo，并按**规范化行尾后的字节**比较 expected.txt。
///
/// 热重载段的日志已在 demo 内缓冲排序；这里保留有限重试作为最后保险，
/// 失败时保存最后一次原始输出供排查。
fn demo_check() -> i32 {
    let code = build_plugins();
    if code != 0 {
        return code;
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let expected =
        std::fs::read(workspace_root().join("examples/demo/expected.txt")).unwrap_or_default();
    let expected = normalize_newlines(&expected);
    let mut last_actual = Vec::new();
    for attempt in 1..=3 {
        let output = match Command::new(&cargo)
            .args(["run", "--locked", "--quiet", "-p", "demo"])
            .current_dir(workspace_root())
            .output()
        {
            Ok(output) => output,
            Err(err) => {
                eprintln!("运行 demo 失败: {err}");
                return 1;
            }
        };
        if !output.status.success() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            return output.status.code().unwrap_or(1);
        }
        let actual = normalize_newlines(&output.stdout);
        if expected == actual {
            println!(
                "demo output matches expected.txt byte-for-byte ({} bytes, attempt {attempt})",
                actual.len()
            );
            return 0;
        }
        last_actual = output.stdout;
    }
    let actual_path = std::env::temp_dir().join("cordis-demo-actual.txt");
    let _ = std::fs::write(&actual_path, &last_actual);
    let actual = normalize_newlines(&last_actual);
    println!(
        "demo snapshot mismatch after 3 attempts: expected {} bytes, actual {} bytes; actual written to {}",
        expected.len(),
        actual.len(),
        actual_path.display()
    );
    if let Some(index) = expected
        .iter()
        .zip(&actual)
        .position(|(left, right)| left != right)
    {
        println!("first difference at byte {index}");
    }
    1
}

fn normalize_newlines(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            out.push(b'\n');
            index += 2;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    out
}

fn run(args: &[&str]) -> i32 {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    println!("+ {cargo} {}", args.join(" "));
    match Command::new(&cargo)
        .args(args)
        .current_dir(workspace_root())
        .status()
    {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("执行 {cargo} 失败: {err}");
            1
        }
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask 必须位于 workspace 根目录下")
        .to_path_buf()
}

fn doctor() {
    let root = workspace_root();
    println!("workspace = {}", root.display());
    println!("path is ASCII = {}", root.to_string_lossy().is_ascii());
    println!("rustc = {}", rustc_version());
    for key in [
        "CARGO_TARGET_DIR",
        "CARGO_HOME",
        "CARGO_HTTP_PROXY",
        "HTTP_PROXY",
        "HTTPS_PROXY",
    ] {
        println!(
            "{key} = {}",
            std::env::var(key).unwrap_or_else(|_| "(unset)".to_string())
        );
    }
    if !root.to_string_lossy().is_ascii() {
        println!();
        println!("提示: 当前 workspace 路径含非 ASCII 字符。Windows GNU 链接器可能无法处理；");
        println!("      请设置 CARGO_TARGET_DIR 到纯 ASCII 路径，例如 C:\\cordis-rs-target。");
    }
}

fn rustc_version() -> String {
    Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|| "(rustc 不可用)".to_string())
}
