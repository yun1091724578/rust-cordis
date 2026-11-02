# 发布检查清单

## 必须通过

| 门禁 | 命令 / 位置 | 说明 |
|---|---|---|
| 本地统一入口 | `.\cargo.cmd xtask ci` | 格式检查 + inventory/docs/平台矩阵检查 + clippy（`-D warnings`）+ 全量测试 + demo 快照 + harness e2e；2026-11-02 在 Windows GNU 全量通过。**限制**：仅 Windows/cmd；硬编码 `CARGO_HOME=C:\cordis-rs-cargo-home`、`CARGO_TARGET_DIR=C:\cordis-rs-target`、本地代理 `127.0.0.1:7897` 与 rustup self-contained PATH；`RUSTFLAGS` 与 CI 裸 `cargo test` 不同。换机或 CI 仍以三平台结果为准，裸 `cargo xtask ci` 不作为受支持入口 |
| 三平台运行 | GitHub Actions `test` job | Linux / Windows MSVC / macOS 实际编译、运行测试与真 DLL e2e（Windows GNU 是本地工具链，不是 CI 的 Windows 目标） |
| MSRV | GitHub Actions `msrv` job | 用 Rust 1.85.0 检查声明的 `rust-version`（Windows GNU 本地已用 `cargo.cmd +1.85.0-x86_64-pc-windows-gnu check --workspace --all-targets` 通过） |
| 打包 | GitHub Actions `package` job | `cordis-core` / `abi` / `host` / `macros` 的 `cargo package --no-verify --locked` |
| 格式 | `cargo xtask fmt`（含独立夹具）/ CI lint job | 本地检查 workspace + `tests/fixtures/abi-v1` + `tests/fixtures/abi-error` + `tests/fixtures/abi-err-v1`；CI lint job 跑 `cargo fmt --all -- --check` 与 `cargo clippy --workspace --all-targets --locked -- -D warnings`，夹具格式目前不在 CI 门禁内 |
| 支持契约 | `cargo xtask docs`（CI 由 `cargo test --workspace` 覆盖索引检查） | 保证索引必须引用带 `#[test]` / `#[tokio::test]` 的测试；违规非零退出 |
| API 清单 | `cargo xtask inventory --check`（CI 由 xtask 的 inventory CLI 用例覆盖） | ABI 常量/字段与受控 marker 漂移即失败；新增宿主入口需人工加入清单 |
| 平台矩阵结构 | `.\cargo.cmd xtask matrix`（Windows GNU 本地；CI 用裸 `cargo`）/ 已接入 `xtask ci` | 17 条保证逐条有行、四平台状态合法、证据引用 `.rs` 测试文件；**不验证真实平台结果**，`待验证` 不得当保证 |
| 首发封存入口 | `cargo xtask seal [--out <dir>]` / `cargo xtask seal --verify <dir>` | 构建三个宿主目标 release 插件，记录源码树 / 工具链 / ABI / SHA256，用同版本 `inspect` 逐插件核对身份，并把 stdout、smoke 配置与 artifact 一起写入校验和；2026-11-02 Windows GNU 本地演练通过。封存对象是 `publish = false` 的示例插件，不是可发布 crate。格式与边界见 `docs/release-seal.md`；**不是 CI 门禁**，正式首发时必须运行并把整个目录归档 |
| 资源增长回归 | `cargo test -p cordis-host --test resource_baseline` / `--test staged`（三平台 CI 已包含；首次结果待确认） | 实例 / 派发 / 挂起 / 派发帧 / 流 / effect / 暂存台账回到基线；独立 soak 由 `cargo xtask soak` 运行，见发布前项 9 |
| 冻结 ABI v1 兼容 | `cargo test -p cordis-host --test frozen_plugin`（三平台 CI 已包含；首次结果待确认） | 真 72B v1 结构体（无 `provision`）；volatile / 回调 / dispose；版本与尺寸拒绝；哈希冻结 |
| 声明与诊断 | `cargo test -p cordis-abi --test schema --test macro_schema --test invalid_schema_entry`；`cargo test -p cordis-macros`；`cargo test -p cordis-core --test pending_diagnostics --test runtime_cycles`；`cargo test -p cordis-host --test loader_cycles`；`cargo test -p cordis-cli --test status --test inspect --test args` | schema 装载前可见，非法/超限 schema 在入口降级不 abort，未声明与非法可区分；原生宏在展开期校验；Pending 区分 missing / version_mismatch / provider_not_ready / realm_mismatch / cycle，并提供者未 apply 时明确为 missing；`status` 列出 failed/disabled、支持 `--json`、失败退出码非 0；来源标注为插件自述 |
| 错误通道兼容 | `cargo test -p cordis-host --test legacy_error --test sdk_detailed_timeout --test abi_outbound_timeout --test abi_outbound_dispatch_timeout --test abi_next_timeout --test sdk_iface_unload_detailed --test sdk_provider_error_detailed --test abi_iface_unload_abort` | 旧 ABI 文本 / `unknown`；异步详细入口；iface / dispatch / next 超时 `HOST_TIMEOUT/timeout/unknown`；卸载/取消 `host_lifecycle|host_cancel`；provider 业务错误 `callback_failed/business`；旧宿主格为桩并在 ADR 标注 |

## 首发封存流程

1. 在发布用的干净源码树上运行 `cargo xtask seal`（或指定 `--out`）。
2. 确认输出目录包含 `seal.json`、`SHA256SUMS`、`plugins/`、`smoke/cordis.yml`
   与 `smoke/inspect.txt`，且命令末尾报告同版本加载检查通过。
3. 用 `cargo xtask seal --verify <dir>` 做一次只读的目录 / 校验和复验（不重跑
   加载检查）。
4. 把**整个目录**归档到发布存储；不要用第二次运行覆盖首发表。格式、字段与
   加载检查边界见 `docs/release-seal.md`。

本流程只证明发布产物可被同版本宿主加载；跨版本兼容仍要等后续宿主版本加载
已归档的首发产物。

## 关闭契约

- 控制台：Enter / Ctrl-C / Ctrl-Break 走 `App::close`。
- Unix：`SIGINT` / `SIGTERM` 走 `App::close`；`crates/cli/tests/signal_unix.rs` 在 CI 中实际发送 `SIGTERM`。
- Windows 控制台关闭/注销/关机：处理器等待清理完成，最多 4 秒。
- Windows SCM service stop：`service` 子命令已实现 `StartServiceCtrlDispatcherW` / `RegisterServiceCtrlHandlerExW`，收到 stop 后走 `App::close`。仍需管理员权限做一次真实 `sc create` / `sc stop` 实测。

## 发布前仍需确认

1. 仓库 URL：用于 `repository` / `homepage` 元数据与 CI badge；当前 `cargo package` 只因此产生警告。
2. 首次 GitHub Actions 三平台结果：本地只有 Windows GNU，MSRV 已本地通过；Linux 分支目前只做过 `cargo check --target x86_64-unknown-linux-gnu`，Windows CI 目标是 MSVC，不能拿本地 GNU 结果代替。
3. Windows SCM 真实 `sc create` / `sc start` / `sc stop` 实测（需要管理员权限；当前只验证了非 SCM 启动的失败路径）。
4. **首发产物封存**：入口与本地演练已完成（`cargo xtask seal`，2026-11-02 Windows GNU），但正式首发仍需在 release 时重新运行并归档整个 `seal.json` / `SHA256SUMS` / `plugins` / `smoke` 目录；封存对象是三个 `publish = false` 示例插件，不是可发布 crate。这是历史兼容的基线。
5. **首发产物 × 同版本宿主**：`seal` 已内置同版本 `inspect` 加载检查，并把完整 stdout 写入 `smoke/inspect.txt`、纳入 `SHA256SUMS`；正式首发时要把该证据连同产物一起归档。它只证明产物可用，**不证明跨版本兼容**。
6. **历史产物 × 后续宿主版本**：等后续宿主版本加载已封存的首发产物，才把“历史发布版 × 新宿主”从待验证转成保证或明确拒绝。
7. 保证 × 平台矩阵：`docs/platform-matrix.md` 的结构已建立并由 `.\cargo.cmd xtask matrix`（本地）/ `cargo xtask matrix`（CI）检查；Windows GNU 已填本地结果，Windows MSVC / Linux / macOS 仍待首次 CI，不能把结构通过当成平台通过。
8. **验证入口可重复**：本机 `cargo.cmd` 已能完整运行，但它只解决 Windows GNU 的 `dlltool/as` 与固定目录/代理；跨机可重复性以首次三平台 CI 结果为准，不能把单机绿当成发布证据。
9. **夜间 soak**：`cargo xtask soak [cycles]` 入口已就绪，2026-11-02 本地 1000 轮通过（成功 / 失败 / 周期性的丢弃在飞 Future 与重载 + 暂存台账）；正式发布前至少跑一次更长轮次并记录结果。它不在 `ci` 内，不能用 1000 轮本地结果代替夜间运行。

## 已确定

- 许可证：`MIT OR Apache-2.0`，根目录提供 `LICENSE-MIT` / `LICENSE-APACHE`，四个发布 crate 继承 workspace 元数据。

## 当前可发布 crate

- `cordis-core`
- `cordis-abi`
- `cordis-host`
- `cordis-macros`

`cordis-sdk`、`cordis-components`、`cordis-cli` 与 examples 目前保持 `publish = false`。
