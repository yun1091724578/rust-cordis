//! 真实 ABI 消费者的共享夹具。
//!
//! `tests/common/mod.rs` 是**目录模块**：Cargo 不会把它当成独立测试 target，
//! 于是每个测试二进制都可以 `mod common;` 复用同一份辅助代码，而**进程级
//! 全局量不会跨二进制串门**（每个测试文件是独立的进程）。
//!
//! 这里只做三件事，逐条对应 O6 的要求：
//! 1. 准入状态的 RAII 复位（策略与 `allow_unverified` 都是进程级全局量）；
//! 2. 插件 artifact 的**临时目录沙箱**：把 DLL 复制到
//!    `temp_dir()/cordis-consumer-<pid>-<label>-<seq>` 再在那里写 sidecar、
//!    让装载器的暂存副本也落在同一个目录——`target/debug` 不再被污染；
//! 3. 关闭序列：先 `App::close()`、再丢掉 `LoaderState`（暂存副本随它回收），
//!    最后由 [`Sandbox`] 的 `Drop` 删掉整个临时目录（带少量重试，见
//!    [`Sandbox::assert_removed`]：用例侧的"目录已删"断言跑在同一套重试之后）。
#![allow(dead_code)] // 每个测试二进制只用其中一部分辅助函数。

use cordis_core::{App, ExternValue};
use cordis_host::loader::{load_with_source, DiskPluginSource, LoaderConfig, LoaderState};
use cordis_host::source::{self, AllowedPlugin, PluginSourcePolicy};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 进程级串行闸门。
///
/// 来源策略（[`source::set_active_policy`]）与开发模式
/// （[`source::set_allow_unverified`]）都是**进程级**全局量，同一测试二进制里的
/// 用例因此必须串行。闸门用 `tokio::sync::Mutex` 而不是 `std::sync::Mutex`：
/// 它要**跨 await 持有**（装载、关闭都是异步的），标准锁跨 await 会在线程迁移后死锁。
pub static GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 被测的两个示例插件（`examples/plugins/<name>`，`[lib] name` 即文件名）。
const ARTIFACTS: [&str; 2] = ["greeting", "hello"];

static SEQ: AtomicU64 = AtomicU64::new(0);

/// `cargo xtask consumer` 构建完真实 DLL 后注入的目录。
pub fn consumer_plugin_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("CORDIS_CONSUMER_PLUGIN_DIR")
            .expect("run through `cargo xtask consumer` (plugin dir env missing)"),
    )
}

/// 准入状态的 RAII 守卫：`Drop` 里把进程级全局量复位成**默认拒绝**。
///
/// 用守卫而不是在测试末尾手写复位：断言 panic 时也要复位，否则同一二进制里的
/// 下一个用例会带着上一个用例的策略起跑（这正是 O6 要消灭的互相污染）。
pub struct Admission(());

impl Admission {
    /// 生产路径：显式 allow-list + sidecar manifest（空列表 = 拒绝一切）。
    pub fn enforce(allowed: Vec<AllowedPlugin>) -> Self {
        source::set_allow_unverified(false);
        source::set_active_policy(Some(Arc::new(PluginSourcePolicy::new(allowed))));
        Self(())
    }

    /// 开发路径：没有策略、显式放行未校验插件（`--allow-unverified` 的等价物）。
    pub fn dev() -> Self {
        source::set_active_policy(None);
        source::set_allow_unverified(true);
        Self(())
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        source::set_active_policy(None);
        source::set_allow_unverified(false);
    }
}

/// 一次测试的插件沙箱：临时目录 + 复制过的 artifact + sidecar manifest + `App`。
///
/// 目录布局：
/// ```text
/// cordis-consumer-<pid>-<label>-<seq>/
///   cordis.yml           # 测试用主配置
///   plugins/             # 复制来的 DLL + 它们的 sidecar；装载器的暂存副本也落在这里
/// ```
/// 断言 `plugins/` 里只剩复制的那几个文件，就等于同时断言了"暂存副本没有泄漏到盘上"。
pub struct Sandbox {
    dir: PathBuf,
    plugins: PathBuf,
    config: PathBuf,
    /// 复制进沙箱的 artifact 文件名（`leftovers` 的白名单）。
    artifacts: Vec<String>,
    allowed: Vec<AllowedPlugin>,
    app: Option<App>,
    state: Option<Arc<LoaderState>>,
}

impl Sandbox {
    /// 新建沙箱：复制真实 DLL、就地写 sidecar（sha256 按**复制后**的文件算）。
    pub fn new(label: &str) -> Self {
        let source_dir = consumer_plugin_dir();
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "cordis-consumer-{}-{label}-{seq}",
            std::process::id()
        ));
        // 同一 pid 下同名沙箱可能是上次崩溃留下的：先清掉，别让旧残留把关。
        let _ = std::fs::remove_dir_all(&dir);
        let config = dir.join("cordis.yml");
        let plugins = dir.join("plugins");
        std::fs::create_dir_all(&plugins)
            .unwrap_or_else(|err| panic!("创建临时插件目录 {} 失败: {err}", plugins.display()));

        let mut artifacts = Vec::new();
        let mut allowed = Vec::new();
        for name in ARTIFACTS {
            let source = cordis_host::resolve_plugin_path(name, std::slice::from_ref(&source_dir))
                .unwrap_or_else(|err| {
                    panic!(
                        "解析插件 {name} 失败（CORDIS_CONSUMER_PLUGIN_DIR={}）: {err}",
                        source_dir.display()
                    )
                });
            let file_name = source
                .file_name()
                .expect("插件 artifact 必须有文件名")
                .to_owned();
            let copy = plugins.join(&file_name);
            std::fs::copy(&source, &copy).unwrap_or_else(|err| {
                panic!(
                    "复制插件 {} → {} 失败: {err}",
                    source.display(),
                    copy.display()
                )
            });
            allowed.push(write_sidecar(&copy, name));
            artifacts.push(file_name.to_string_lossy().into_owned());
        }

        Self {
            dir,
            plugins,
            config,
            artifacts,
            allowed,
            app: Some(App::new()),
            state: None,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn app(&self) -> &App {
        self.app.as_ref().expect("沙箱已关闭")
    }

    /// 当前装载器状态。
    ///
    /// **不要**把返回的 `Arc` 存到用例末尾还能活到的变量里：`Mounted` 持有插件
    /// `Arc`，暂存副本的所有权在插件对象上——多一个 `Arc` 在手上，暂存文件就不会
    /// 被删，"关闭后不留残留"的断言会假红。用 [`Sandbox::close`] 统一收尾。
    ///
    /// 同一条纪律适用于 [`LoaderState::fiber_of`] 返回的 `Arc<Fiber>`：
    /// 它同样把插件（以及暂存副本）吊住，用完立刻让它离开作用域。
    pub fn state(&self) -> &Arc<LoaderState> {
        self.state.as_ref().expect("尚未装载")
    }

    pub fn allowed(&self) -> Vec<AllowedPlugin> {
        self.allowed.clone()
    }

    pub fn write_config(&self, rows: &str) {
        std::fs::write(&self.config, rows).expect("写测试配置");
    }

    /// 用当前配置装载一次。`strict` 对应 [`LoaderConfig::strict`]。
    pub async fn load(&mut self, strict: bool) -> Result<(), String> {
        let mut config = LoaderConfig::new(self.config.to_string_lossy().into_owned());
        config.plugin_dir = Some(self.plugins.to_string_lossy().into_owned());
        config.strict = strict;
        match load_with_source(self.app().root.clone(), config, Arc::new(DiskPluginSource)).await {
            Ok(state) => {
                self.state = Some(state);
                Ok(())
            }
            Err(err) => Err(err.to_string()),
        }
    }

    /// 关闭序列：处置根树（释放 ABI 实例）→ 丢掉装载器状态（回收已认领的暂存副本）。
    ///
    /// 顺序是硬要求：`Mounted` 持有插件 `Arc`，暂存文件的所有权在插件对象上；
    /// 先关 App 再丢状态，Windows 才会先 `FreeLibrary` 再删文件。
    pub async fn close(&mut self) {
        if let Some(app) = self.app.take() {
            app.close().await;
        }
        self.state = None;
    }

    /// 插件目录里**不属于**沙箱自己复制的那些文件（即暂存副本残留）。
    ///
    /// `plugins/` 不存在是**哨兵失败**而不是"没有残留"：目录被提前删掉（或用例
    /// 写坏了沙箱）时返回空 Vec 会让断言恒真，把真缺陷掩盖成绿灯。
    pub fn leftovers(&self) -> Vec<String> {
        let entries = std::fs::read_dir(&self.plugins).unwrap_or_else(|err| {
            panic!(
                "读插件目录 {} 失败（目录不存在即视为沙箱失控，不能当作\"无残留\"）: {err}",
                self.plugins.display()
            )
        });
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.retain(|name| {
            !self
                .artifacts
                .iter()
                .any(|artifact| name == artifact || *name == format!("{artifact}.manifest.json"))
        });
        names.sort();
        names
    }

    /// 关闭后的收尾断言：沙箱目录必须消失。
    ///
    /// 删除走与 [`Drop`] 完全相同的重试逻辑（[`remove_dir_with_retry`]），只有
    /// 重试**之后**仍存在才判红：Windows 上杀软/索引器可能短暂持有刚卸载的 DLL
    /// 句柄，一次 `remove_dir_all` 失败并不等于关闭序列泄漏。
    pub fn assert_removed(&self) {
        remove_dir_with_retry(&self.dir);
        assert!(
            !self.dir.exists(),
            "临时目录 {} 在 {REMOVE_ATTEMPTS} 次删除重试后仍然存在；\
             可能是外部程序（杀软/索引器）短暂占用目录内文件，未必是关闭序列泄漏",
            self.dir.display()
        );
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // 先 drop App / LoaderState（卸载动态库、回收暂存副本），再删目录。
        self.state = None;
        self.app = None;
        // Drop 里不 panic：删不掉就交给系统临时目录清理，目录断言由
        // [`Sandbox::assert_removed`] 负责（它跑的是同一套重试）。
        remove_dir_with_retry(&self.dir);
    }
}

/// 删除临时目录的重试次数与每次间隔（毫秒）。
const REMOVE_ATTEMPTS: u32 = 3;
const REMOVE_BACKOFF_MS: u64 = 20;

/// 删掉临时目录，失败就短暂等待后重试。
///
/// 只吸收**环境抖动**：Windows 上杀软/索引器/搜索服务可能在 `FreeLibrary` 之后
/// 还握着一瞬间的文件句柄，`remove_dir_all` 会因此返回错误。重试几次仍失败就
/// 放弃（目录留给系统清理），调用方若要断言删除成功，用 [`Sandbox::assert_removed`]。
fn remove_dir_with_retry(dir: &Path) {
    for _ in 0..REMOVE_ATTEMPTS {
        if !dir.exists() || std::fs::remove_dir_all(dir).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(REMOVE_BACKOFF_MS));
    }
}

/// 探针配置：`config` 注入的字符串是**消费者自己的**，不依赖示例插件的默认文案。
pub fn composed_config(greeting_text: &str, times: u64) -> String {
    format!(
        "- id: greeting\n  name: greeting\n  config:\n    text: {greeting_text}\n\
         - id: hello\n  name: hello\n  config:\n    times: {times}\n"
    )
}

/// 按名字读一个跨 ABI 服务值（`ExternValue` 的内部 JSON）。
pub fn extern_value(app: &App, name: &str) -> Value {
    let value = app
        .root
        .require_as::<ExternValue>(name)
        .unwrap_or_else(|err| panic!("读跨 ABI 服务 {name} 失败: {err}"));
    value.0.clone()
}

/// 断言两个真实插件之间的**服务链**（两条准入路径共用同一份业务断言）。
///
/// `greeting_text` / `times` 是消费者自己注入的探针值：任何一个环节把值换成
/// 写死的默认文案（示例插件的默认是 `hello from the abi` / `1`），这里都会红。
pub fn assert_composed_service_chain(app: &App, greeting_text: &str, times: u64) {
    // 纯数据服务。
    let greeting = extern_value(app, "greeting.text");
    assert_eq!(greeting["text"], json!(greeting_text));

    // hello 的返回值：四个字段分别证明"读服务值"、"同步跨 ABI 调用"、
    // "版本协商拒绝"、"有返回值派发"。
    let reply = extern_value(app, "hello.reply");
    assert_eq!(
        reply["message"],
        json!(format!("{greeting_text} × {times}")),
        "hello 必须读到 greeting 服务注入的探针串与自己的 times: {reply}"
    );
    assert_eq!(
        reply["inline_call"],
        json!(format!("{} hello!", greeting_text.to_uppercase())),
        "跨 ABI 的 iface 调用必须走同步快速路径并拿到提供方结果: {reply}"
    );
    let version_error = reply["version_error"]
        .as_str()
        .unwrap_or_else(|| panic!("version_error 必须是字符串: {reply}"));
    assert!(
        version_error.contains("请求接口版本 3"),
        "v3 接口请求必须被明确拒绝: {version_error}"
    );
    let dispatch = reply["dispatch_result"]
        .as_str()
        .unwrap_or_else(|| panic!("dispatch_result 必须是字符串: {reply}"));
    assert!(
        dispatch.contains(&format!("{greeting_text} → self")),
        "有返回值派发必须带回本插件监听器的结果: {dispatch}"
    );

    // 负向解析：根上下文（无 fiber）读一个没注册的服务 → `NoInject`。
    // 这一层是可断言的：`Inactive` 只在"本 fiber 声明过该注入"时出现，
    // 而根 ctx 根本没有 `inject` 可声明。
    let err = app
        .root
        .require_as::<ExternValue>("nonexistent.svc")
        .unwrap_err();
    assert!(
        matches!(err, cordis_core::ResolveError::NoInject(_)),
        "读未注册服务必须是 NoInject: {err:?}"
    );
}

/// 写复制后 artifact 的 sidecar manifest，返回对应的 allow-list 条目。
fn write_sidecar(artifact: &Path, id: &str) -> AllowedPlugin {
    let sha256 = source::sha256_file(artifact).expect("hash artifact");
    let manifest = serde_json::json!({
        "id": id,
        "version": "0.1.1",
        "source_revision": "tree:consumer-service-test",
        "abi_version": 1,
        "sha256": sha256.clone(),
    });
    std::fs::write(
        source::manifest_path(artifact),
        serde_json::to_string_pretty(&manifest).expect("序列化 sidecar"),
    )
    .expect("写 sidecar manifest");
    AllowedPlugin {
        id: id.to_string(),
        version: "0.1.1".to_string(),
        source_revision: "tree:consumer-service-test".to_string(),
        abi_version: 1,
        sha256,
    }
}
