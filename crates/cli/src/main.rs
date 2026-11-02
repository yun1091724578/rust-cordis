//! cordis-cli：`cordis` 命令行工具。
//!
//! 子命令：
//! - `inspect`：装载前打印能力清单，可选 `--json`；
//! - `validate`：主配置 + patch 层校验，**不挂载**；
//! - `run`：装载后运行，按 Enter / Ctrl-C / Unix SIGTERM 走 `App::close` 优雅关闭；
//! - `service`：Windows SCM 服务入口（必须由 SCM 启动）。
//!
//! `dev`/HMR 监听仍留给后续里程碑：生命周期与跨平台构建稳定后再做。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
cordis <子命令> [参数]

子命令:
  inspect <config.yml> [--json] [--plugin-dir <dir>]
                                                 打印装载前能力清单
  status <config.yml> [--patch <file>]...       短暂挂载后打印诊断并关闭
      [--plugin-dir <dir>] [--no-strict] [--json]
                                                 （会执行插件 apply/cleanup）
  validate <config.yml> [--patch <file>]...     校验主配置 + patch 层（不挂载）
      [--plugin-dir <dir>]
  run <config.yml> [--patch <file>]...          装载并运行，按 Enter / Ctrl-C 优雅关闭
      [--plugin-dir <dir>] [--no-strict]
  service <config.yml> --name <service-name>    Windows SCM 服务入口
      [--patch <file>]... [--plugin-dir <dir>] [--no-strict]

退出码: 0 成功 / 2 用法错误 / 3 配置错误 / 4 运行时错误";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("inspect") => inspect(&args[1..]),
        Some("status") => status(&args[1..]),
        Some("validate") => validate(&args[1..]),
        Some("run") => run(&args[1..]),
        Some("service") => service(&args[1..]),
        Some("help" | "--help" | "-h") | None => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("未知子命令 \"{other}\"\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn inspect(args: &[String]) -> ExitCode {
    let parsed = match parse_config_args(args, false, true, false, true) {
        Ok(parsed) => parsed,
        Err(message) => return usage_error("inspect", &message),
    };
    let search = parsed.plugin_dir.map(|dir| vec![dir]).unwrap_or_else(|| {
        let dir = Path::new(&parsed.path)
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        vec![dir]
    });
    match cordis_cli::inspect_in(Path::new(&parsed.path), &search) {
        Ok(report) => {
            if parsed.json {
                println!("{}", report.to_json());
            } else {
                print!("{}", report.render());
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("inspect 失败: {err}");
            ExitCode::from(3)
        }
    }
}

fn validate(args: &[String]) -> ExitCode {
    let parsed = match parse_config_args(args, true, true, false, false) {
        Ok(parsed) => parsed,
        Err(message) => return usage_error("validate", &message),
    };
    let search = parsed.plugin_dir.map(|dir| vec![dir]).unwrap_or_else(|| {
        let dir = Path::new(&parsed.path)
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        vec![dir]
    });
    match cordis_cli::validate_in(Path::new(&parsed.path), &parsed.patches, &search) {
        Ok(report) => {
            print!("{}", report.render());
            if report.warnings.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(3)
            }
        }
        Err(err) => {
            eprintln!("validate 失败: {err}");
            ExitCode::from(3)
        }
    }
}

fn status(args: &[String]) -> ExitCode {
    let parsed = match parse_config_args(args, true, true, true, true) {
        Ok(parsed) => parsed,
        Err(message) => return usage_error("status", &message),
    };
    let options = cordis_cli::RunOptions {
        patches: parsed.patches,
        plugin_dir: parsed.plugin_dir,
        strict: parsed.strict,
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("无法创建 tokio runtime: {err}");
            return ExitCode::from(4);
        }
    };
    match runtime.block_on(cordis_cli::status(Path::new(&parsed.path), options)) {
        Ok(report) => {
            if parsed.json {
                println!("{}", report.to_json());
            } else {
                print!("{}", report.render());
            }
            if report.is_healthy() {
                ExitCode::SUCCESS
            } else {
                // `--no-strict` 下装载器会降级继续；诊断命令仍要把失败
                // 反映到退出码，避免 CI 把“有行没装上”当成健康。
                ExitCode::from(4)
            }
        }
        Err(err) => {
            eprintln!("status 失败: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}

fn run(args: &[String]) -> ExitCode {
    let parsed = match parse_config_args(args, true, true, true, false) {
        Ok(parsed) => parsed,
        Err(message) => return usage_error("run", &message),
    };
    let options = cordis_cli::RunOptions {
        patches: parsed.patches,
        plugin_dir: parsed.plugin_dir,
        strict: parsed.strict,
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("无法创建 tokio runtime: {err}");
            return ExitCode::from(4);
        }
    };
    match runtime.block_on(cordis_cli::run(Path::new(&parsed.path), options)) {
        Ok(report) => {
            println!("mounted = {}", report.mounted.len());
            for label in &report.mounted {
                println!("- {label}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("run 失败: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}

fn service(args: &[String]) -> ExitCode {
    let mut path = None;
    let mut name = None;
    let mut patches = Vec::new();
    let mut plugin_dir = None;
    let mut strict = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--name" => match iter.next() {
                Some(value) => name = Some(value.to_string()),
                None => return usage_error("service", "--name 缺少服务名"),
            },
            "--patch" => match iter.next() {
                Some(value) => patches.push(PathBuf::from(value)),
                None => return usage_error("service", "--patch 缺少文件名"),
            },
            "--plugin-dir" => match iter.next() {
                Some(value) => plugin_dir = Some(PathBuf::from(value)),
                None => return usage_error("service", "--plugin-dir 缺少目录"),
            },
            "--no-strict" => strict = false,
            other if other.starts_with('-') => return usage_error("service", other),
            other => {
                if path.is_some() {
                    return usage_error("service", "只能指定一个配置文件");
                }
                path = Some(other.to_string());
            }
        }
    }
    let Some(path) = path else {
        return usage_error("service", "缺少配置文件");
    };
    let Some(name) = name else {
        return usage_error("service", "缺少 --name <service-name>");
    };

    let options = cordis_cli::RunOptions {
        patches,
        plugin_dir,
        strict,
    };
    match cordis_cli::service::run(Path::new(&path), &name, options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("service 失败: {err}");
            ExitCode::from(4)
        }
    }
}

struct ConfigArgs {
    path: String,
    patches: Vec<PathBuf>,
    plugin_dir: Option<PathBuf>,
    strict: bool,
    json: bool,
}

/// 解析 config 子命令的公共参数：位置参数、`--patch`、`--plugin-dir`、
/// `--no-strict`、`--json`。缺值、重复位置参数和未知 flag 都是用法错误。
fn parse_config_args(
    args: &[String],
    allow_patch: bool,
    allow_plugin_dir: bool,
    allow_strict: bool,
    allow_json: bool,
) -> Result<ConfigArgs, String> {
    let mut path = None;
    let mut patches = Vec::new();
    let mut plugin_dir = None;
    let mut strict = true;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--patch" if allow_patch => {
                patches.push(PathBuf::from(next_value(&mut iter, "--patch")?));
            }
            "--plugin-dir" if allow_plugin_dir => {
                plugin_dir = Some(PathBuf::from(next_value(&mut iter, "--plugin-dir")?));
            }
            "--no-strict" if allow_strict => strict = false,
            "--json" if allow_json => json = true,
            other if other.starts_with('-') => return Err(format!("未知参数 {other}")),
            other => {
                if path.is_some() {
                    return Err("只能指定一个配置文件".to_string());
                }
                path = Some(other.to_string());
            }
        }
    }
    Ok(ConfigArgs {
        path: path.ok_or_else(|| "缺少配置文件".to_string())?,
        patches,
        plugin_dir,
        strict,
        json,
    })
}

fn next_value(iter: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    let value = iter.next().ok_or_else(|| format!("{flag} 缺少值"))?;
    if value.starts_with('-') {
        return Err(format!("{flag} 缺少值"));
    }
    Ok(value.clone())
}

fn usage_error(command: &str, message: &str) -> ExitCode {
    eprintln!("{command}: {message}\n\n{USAGE}");
    ExitCode::from(2)
}
