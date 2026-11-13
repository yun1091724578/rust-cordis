//! 跨平台构建入口：`cargo xtask <command>`。
//!
//! 仓库里不保存代理、绝对 target 目录或平台专用链接参数；本机差异放在
//! `cargo.cmd`（本地便利包装）或用户级 Cargo 配置里。

use std::path::PathBuf;
use std::process::{Command, ExitCode};

mod contract;
mod inventory;
mod matrix;
mod seal;

const USAGE: &str = "\
cargo xtask <命令>

命令:
  fmt              cargo fmt --all -- --check
  clippy           cargo clippy --workspace --all-targets --locked -- -D warnings
  test             cargo test --workspace --all-targets --locked
  consumer         测试独立服务消费者（只使用公开 path 依赖）
  build-plugins    构建 greeting / hello / harness-tool 三个 cdylib
  demo             构建插件并运行 demo
  e2e              构建插件并运行 harness-spike
  ci               fmt + inventory + docs + matrix + clippy + test + consumer + demo + e2e
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
  soak [cycles]    运行被忽略的宿主资源 soak（默认 1000 轮；不进入 ci）
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
        "soak" => soak_command(std::env::args().skip(2).collect()),
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
            "--locked",
            "--",
            "-D",
            "warnings",
        ][..],
        &["test", "--workspace", "--all-targets", "--locked"][..],
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
        None => match std::env::var("CORDIS_SOAK_CYCLES") {
            Ok(raw) => match raw.parse::<u64>() {
                Ok(cycles) if cycles > 0 => cycles,
                _ => {
                    eprintln!("CORDIS_SOAK_CYCLES 必须是正整数，实际 \"{raw}\"\n\n{USAGE}");
                    return 2;
                }
            },
            Err(_) => 1000,
        },
    };
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let test_name = "repeated_soak_cycles_leave_no_residue";
    // `cargo test -- --ignored` 在 0 个 ignored 测试时也退出 0。先列出目标测试，
    // 防止测试被删/改名/去掉 #[ignore] 后 nightly 变成假绿。
    let listed = Command::new(&cargo)
        .args([
            "test",
            "-p",
            "rust-cordis-host",
            "--test",
            "soak",
            "--locked",
            "--",
            "--ignored",
            "--list",
        ])
        .current_dir(workspace_root())
        .output();
    match listed {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.contains(&format!("{test_name}: test")) {
                eprintln!(
                    "soak 测试 {test_name} 未注册为 #[ignore] 测试；拒绝运行空 soak\n{}",
                    stdout.trim_end()
                );
                return 1;
            }
        }
        Ok(output) => {
            eprintln!(
                "列出 soak 测试失败（exit {:?}）",
                output.status.code().unwrap_or(1)
            );
            return output.status.code().unwrap_or(1);
        }
        Err(err) => {
            eprintln!("执行 {cargo} 失败: {err}");
            return 1;
        }
    }
    println!(
        "+ {cargo} test -p rust-cordis-host --test soak --test open_staged_soak --locked -- --ignored --nocapture (cycles={cycles})"
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
