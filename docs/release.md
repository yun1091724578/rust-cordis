# 发布检查清单

## 0.1.1 发布记录（2026-11-07）

0.1.1 是元数据补丁，源码提交 `8ab64f98001c19d3bca2b1cd2cd6dd4469645c8a`，
正式标签 `v0.1.1`，ABI / 公开 API 未变：

- [`rust-cordis-core`](https://crates.io/crates/rust-cordis-core) `0.1.1`
- [`rust-cordis-abi`](https://crates.io/crates/rust-cordis-abi) `0.1.1`
- [`rust-cordis-host`](https://crates.io/crates/rust-cordis-host) `0.1.1`
- [`rust-cordis-macros`](https://crates.io/crates/rust-cordis-macros) `0.1.1`

本版补齐 `repository` / `homepage` 元数据；`cargo publish` 不再产生缺失元数据
warning。三平台 CI run 为 [42332542433](https://github.com/yun1091724578/rust-cordis/actions/runs/42332542433)（main）
与 [42332542329](https://github.com/yun1091724578/rust-cordis/actions/runs/42332542329)（tag）。

历史兼容：用正式 v0.1.1 宿主加载 0.1.0 封存插件，`inspect + mount + cleanup`
通过，日志 `dist/cordis-0.1.1-historical.log`。这关闭了“有一次正式后续版本验证”的
事实，但支持契约仍需要可重复测试才能升级为“保证”。

## 0.1.0 发布记录（2026-11-07）

低层 ABI 首发已发布到 crates.io，源码提交
`24fde519825c5aa357ad8d2d41958110b58e70f9`，正式标签 `v0.1.0`：

- [`rust-cordis-core`](https://crates.io/crates/rust-cordis-core) `0.1.0`
- [`rust-cordis-abi`](https://crates.io/crates/rust-cordis-abi) `0.1.0`
- [`rust-cordis-host`](https://crates.io/crates/rust-cordis-host) `0.1.0`
- [`rust-cordis-macros`](https://crates.io/crates/rust-cordis-macros) `0.1.0`

发布顺序为 core → abi → macros → host；host 的发布验证从 crates.io 下载了
core/abi。发布后用 `desolate/registry-consumer` 做了纯 crates.io 依赖的
编译、加载、调用与关闭验收。`rust-cordis-sdk`、`cordis-components`、
`cordis-cli` 与 examples 均未发布。

已知发布 warning：workspace 缺少 `documentation` / `homepage` / `repository`
元数据，应在下一版本前补齐。SCM 与历史产物兼容仍是“不承诺 / 待验证”。

### 两个未验证项的处理方式

**Windows SCM**：真实 `sc create/start/stop` 需要管理员权限。已提供
`scripts/scm-smoke.ps1`；2026-11-07 在管理员 PowerShell（Windows 10.0.26300）
实际跑通：

```text
SCM smoke passed: create -> RUNNING -> stop -> STOPPED
```

脚本执行 `sc create → sc start → sc query（RUNNING）→ sc stop → query（STOPPED）
→ sc delete`，日志 `dist/cordis-scm-smoke.log`；测试服务已删除（`sc query` 返回
1060）。这是一次真实手工验证，持续 CI 重跑仍待补，不能夸大为跨机器持续保证。

**历史产物 × 后续宿主**：已提供 `scripts/historical-compat.ps1`，用法：

```powershell
# 真正的后续宿主版本（不要用 -BumpVersion 代替正式版本）
.\scripts\historical-compat.ps1 -Seal dist\cordis-0.1.0-rc2 -HostRef v0.1.1
```

脚本从指定宿主版本创建临时 worktree、构建 `cordis-cli`，对封存插件执行
`inspect`（打开 + PING）和 `status`（挂载 + cleanup），最后清理 worktree。

2026-11-07 已用 **临时 0.1.1 宿主**做了一次前向演练并通过：

```powershell
.\scripts\historical-compat.ps1 -Seal dist\cordis-0.1.0-rc2 `
  -HostRef v0.1.0 -BumpVersion 0.1.1
```

但这是**演练**，不是正式 0.1.1 发布；支持契约仍保持“历史发布版 × 新宿主 =
待验证”。只有后续正式版本存在并用它加载 0.1.0 封存产物、记录 run/日期后，才能
升级为“保证”。`-BumpVersion` 只用于当前演练，不能替代真实后续版本。

## 必须通过

| 门禁 | 命令 / 位置 | 说明 |
|---|---|---|
| 本地统一入口 | `.\cargo.cmd xtask ci` | 格式检查 + inventory/docs/平台矩阵检查 + clippy（`-D warnings`）+ 全量测试 + demo 快照 + harness e2e；2026-11-02 在 Windows GNU 全量通过。**限制**：仅 Windows/cmd；硬编码 `CARGO_HOME=C:\cordis-rs-cargo-home`、`CARGO_TARGET_DIR=C:\cordis-rs-target`、本地代理 `127.0.0.1:7897` 与 rustup self-contained PATH；`RUSTFLAGS` 与 CI 裸 `cargo test` 不同。换机或 CI 仍以三平台结果为准，裸 `cargo xtask ci` 不作为受支持入口 |
| 三平台运行 | GitHub Actions `test` job | Linux / Windows MSVC / macOS 实际编译、运行测试与真 DLL e2e（Windows GNU 是本地工具链，不是 CI 的 Windows 目标）；2026-11-02 [run 41460414477](https://github.com/yun1091724578/rust-cordis/actions/runs/41460414477) 首次通过（一次通过；后续变更需重跑） |
| MSRV | GitHub Actions `msrv` job | 用 Rust 1.85.0 检查声明的 `rust-version`（Windows GNU 本地已用 `cargo.cmd +1.85.0-x86_64-pc-windows-gnu check --workspace --all-targets` 通过） |
| 打包 | GitHub Actions `package` job | `rust-cordis-core` / `rust-cordis-abi` / `rust-cordis-host` / `rust-cordis-macros` 的 `cargo package --no-verify --locked` |
| 格式 | `cargo xtask fmt`（含独立夹具）/ CI lint job | 本地检查 workspace + `tests/fixtures/abi-v1` + `tests/fixtures/abi-error` + `tests/fixtures/abi-err-v1`；CI lint job 跑 `cargo fmt --all -- --check` 与 `cargo clippy --workspace --all-targets --locked -- -D warnings`，夹具格式目前不在 CI 门禁内 |
| 支持契约 | `cargo xtask docs`（CI 由 `cargo test --workspace` 覆盖索引检查） | 保证索引必须引用带 `#[test]` / `#[tokio::test]` 的测试；违规非零退出 |
| API 清单 | `cargo xtask inventory --check`（CI 由 xtask 的 inventory CLI 用例覆盖） | ABI 常量/字段与受控 marker 漂移即失败；新增宿主入口需人工加入清单 |
| 平台矩阵（结构 + 结果） | `.\cargo.cmd xtask matrix`（Windows GNU 本地；CI 用裸 `cargo`）/ 已接入 `xtask ci` | 17 条保证逐条有行、四平台状态合法、证据引用 `.rs` 文件；结构检查本身不验证真实平台结果。2026-11-02 首次三平台 CI 通过后，17 条保证在四个平台列均已实填 `通过`；表内注明这是快照，后续变更需重跑 |
| 首发封存入口 | `cargo xtask seal [--out <dir>]` / `cargo xtask seal --verify <dir>` | 构建三个宿主目标 release 插件，记录源码树 / 工具链 / ABI / SHA256，为每个 DLL 写 sidecar manifest 与根 `allow-list.json`，用同版本 `inspect --source-policy` 逐插件核对身份，并把 stdout、smoke 配置、artifact、manifest 与 allow-list 一起写入校验和；2026-11-02 Windows GNU 本地演练通过（旧 `/1` 格式）。封存对象是 `publish = false` 的示例插件，不是可发布 crate。格式与边界见 `docs/release-seal.md`；**不是 CI 门禁**，正式首发时必须运行并把整个目录归档 |
| 资源增长回归 | `cargo test -p rust-cordis-host --test resource_baseline` / `--test staged`（三平台 CI 已包含；2026-11-02 首次通过） | 实例 / 派发 / 挂起 / 派发帧 / 流 / effect / 暂存台账回到基线；独立 soak 见 A5-2 |
| 夜间 soak | `cargo xtask soak [cycles]` / `.github/workflows/nightly-soak.yml`（每日 02:17 CST 或手动 dispatch） | workflow 默认 1,000,000 轮（约 38 分钟，Linux 单测试；手动 dispatch 可覆盖）；2026-11-03 首次手动 20,000 轮 [run 41562347773](https://github.com/yun1091724578/rust-cordis/actions/runs/41562347773) 通过；1M 默认已由 [run 41592298543](https://github.com/yun1091724578/rust-cordis/actions/runs/41592298543) 验证（`elapsed_ms=2265533`，artifact `soak-log-41592298543-c1000000`）；cron 首跑待补，不阻塞 PR；后续变更需重跑，artifact 保留 90 天 |
| 冻结 ABI v1 兼容 | `cargo test -p rust-cordis-host --test frozen_plugin`（三平台 CI 已包含；2026-11-02 首次通过） | 真 72B v1 结构体（无 `provision`）；volatile / 回调 / dispose；版本与尺寸拒绝；哈希冻结；夹具按 LF 字节固定 |
| 来源准入 | `cargo test -p rust-cordis-host --test source_policy`；`cargo test -p cordis-cli --test source_policy`；`cargo xtask seal --verify <dir>` | sidecar manifest + SHA256 allow-list 在 `dlopen` 前校验；无策略默认拒绝；`--allow-unverified` 仅显式开发模式；seal `/2` 产物包含 sidecar 与 allow-list |
| 声明与诊断 | `cargo test -p rust-cordis-abi --test schema --test macro_schema --test invalid_schema_entry`；`cargo test -p rust-cordis-macros`；`cargo test -p rust-cordis-core --test pending_diagnostics --test runtime_cycles`；`cargo test -p rust-cordis-host --test loader_cycles --test source_policy`；`cargo test -p cordis-cli --test status --test inspect --test args --test source_policy` | schema 装载前可见，非法/超限 schema 在入口降级不 abort，未声明与非法可区分；原生宏在展开期校验；Pending 区分 missing / version_mismatch / provider_not_ready / realm_mismatch / cycle，并提供者未 apply 时明确为 missing；`status` 列出 failed/disabled、支持 `--json`、失败退出码非 0；来源标注为插件自述 |
| 错误通道兼容 | `cargo test -p rust-cordis-host --test legacy_error --test sdk_detailed_timeout --test abi_outbound_timeout --test abi_outbound_dispatch_timeout --test abi_next_timeout --test sdk_iface_unload_detailed --test sdk_provider_error_detailed --test abi_iface_unload_abort` | 旧 ABI 文本 / `unknown`；异步详细入口；iface / dispatch / next 超时 `HOST_TIMEOUT/timeout/unknown`；卸载/取消 `host_lifecycle|host_cancel`；provider 业务错误 `callback_failed/business`；旧宿主格为桩并在 ADR 标注 |

## 首发封存流程

1. 在发布用的干净源码树上运行 `cargo xtask seal`（或指定 `--out`）。
2. 确认输出目录包含 `seal.json`、`SHA256SUMS`、`allow-list.json`、`plugins/`
   （每个 DLL 旁边有 `.manifest.json`）、`smoke/cordis.yml` 与
   `smoke/inspect.txt`，且命令末尾报告同版本加载检查通过。
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

## 发布前状态

### A5-1 已关闭

- [x] **首次 GitHub Actions 三平台结果**：2026-11-02 [run 41460414477](https://github.com/yun1091724578/rust-cordis/actions/runs/41460414477)（commit `ea0489e`）已通过；2026-11-03 评审修正后 [run 41560497289](https://github.com/yun1091724578/rust-cordis/actions/runs/41560497289)（commit `617f470`）再次全绿。Linux / Windows MSVC / macOS 的 `test` job 均完成全量测试、demo 与真 DLL e2e，`lint` / `msrv` / `package` 也通过。通过结果不是持续保证，后续变更需重跑。
- [x] **保证 × 平台矩阵**：`docs/platform-matrix.md` 的 17 条保证已在 Windows GNU / Windows MSVC / Linux / macOS 四列实填 `通过`，依据是上述三平台 CI 与本地 Windows GNU 全量门禁；表内注明这是快照，后续变更需重跑。
- [x] **验证入口可重复**：本机 `cargo.cmd` 解决 Windows GNU 的 `dlltool/as` 与固定目录/代理；三平台 CI 已获得一次通过结果，但跨机可重复性仍需在后续变更上重跑验证。

### A5-2 已启用（首次 20k / 默认 1M 通过，持续运行未闭合）

- [x] `.github/workflows/nightly-soak.yml` 已配置每日 02:17 CST（18:17 UTC）运行默认 1,000,000 轮，支持 `workflow_dispatch` 覆盖轮次并上传 `soak.log` artifact。
- [x] `cargo xtask soak` 先以 `--ignored --list` 确认目标测试仍存在且被忽略；
  0 个 ignored 测试不会再被当成通过。
- [x] `.cargo/config.toml` 的 `xtask` alias 已加 `--locked`；nightly 最外层
  构建不会在 runner 上静默重写 `Cargo.lock`。
- [x] 2026-11-03 **手动 dispatch** 首次 [run 41562347773](https://github.com/yun1091724578/rust-cordis/actions/runs/41562347773) 通过：`loaded_instances=0`、`staged_leaked=0`、`runtime=0`、`hooks=0`；artifact `soak-log-41562347773`。
- [x] 2026-11-03 Windows GNU 本地 `.\cargo.cmd xtask soak 20000` 通过（exit 0）。
- [x] 2026-11-03 加强断言后 Windows GNU 本地再次 20,000 轮通过：
  `elapsed_ms=201893`，`calls=applies=cleanups=disposes=58334`；版本包含带在飞
  派发的同 key 重载与精确总数断言。
- [x] 2026-11-03 1,000,000 轮 CI [run 41592298543](https://github.com/yun1091724578/rust-cordis/actions/runs/41592298543)
  通过：`elapsed_ms=2265533`（约 37 分 46 秒），
  `calls=applies=cleanups=disposes=2916668`；artifact
  `soak-log-41592298543-c1000000`（15,674 字节，含 rustc 1.99.1 与
  `Cargo.lock` SHA-256 `7419d157…`）。
- [ ] 首次 cron 触发与连续 nightly 结果（当前两次都是手动 dispatch）。
- [ ] 把首次与后续 nightly artifact 转存到发布存储（见“仍需确认”第 6 项）。
- 边界：当前 nightly 只在 Linux runner 跑单个测试；默认 1,000,000 轮约 38 分钟，首次手动 20k 约 20 秒；这是短时高频迭代，**不是整夜长跑**。暂存部分仍是 `stage_plugin` / `StagedFile` primitive，不覆盖真 DLL `open_staged`；artifact 保留 90 天。

### 仍需确认

1. 仓库 URL：`Cargo.toml` 已在 0.1.1 补齐 `repository` / `homepage`（`https://github.com/yun1091724578/rust-cordis`），发布 warning 消失；仓库仍是私有的，公开发布地址/可见性另行决定。
2. Windows SCM 真实 `sc create` / `sc start` / `sc stop` 实测：已提供 `scripts/scm-smoke.ps1`；2026-11-07 在管理员 PowerShell（Windows 10.0.26300）实际跑通 `create → RUNNING → stop → STOPPED → delete`，日志 `dist/cordis-scm-smoke.log`，服务已确认删除（`sc query` 返回 1060）。这是一次真实手工验证；持续 CI 重跑仍待补，因此不把它夸大为跨机器持续保证。
3. **首发产物封存**：已完成。0.1.0 rc2 与 0.1.1 均运行 `cargo xtask seal`/`--verify`，归档 `seal.json` / `SHA256SUMS` / `plugins` / `smoke`；封存对象是三个 `publish = false` 示例插件，不是可发布 crate。这是历史兼容的基线。
4. **首发产物 × 同版本宿主**：已完成。`seal` 内置同版本 `inspect` 加载检查，stdout 写入 `smoke/inspect.txt` 并纳入 `SHA256SUMS`；0.1.0/0.1.1 证据已归档。它只证明产物可用，**不证明跨版本兼容**。
5. **历史产物 × 后续宿主版本**：2026-11-07 已用正式 v0.1.1 宿主加载 0.1.0 封存插件，`inspect + mount + cleanup` 通过（`dist/cordis-0.1.1-historical.log`）。这是一次真实发布级验证；支持契约仍要求可重复测试，因此状态保持待验证，不把单次结果写成保证。
6. **nightly artifact 长期转存**：当前 `soak.log` 由 GitHub Actions artifact 保存，保留期按仓库设置；正式发布前需把首次与后续 nightly 结果转存到发布存储。nightly schedule 可能在仓库无活动时被 GitHub 自动暂停，发布前需确认最近一次结果。

## 已确定

- 许可证：`MIT OR Apache-2.0`，根目录提供 `LICENSE-MIT` / `LICENSE-APACHE`，四个发布 crate 继承 workspace 元数据。

## 当前可发布 crate

此处表示当前允许打包的范围，不代表完整 typed SDK 已具备 registry 交付路径。
CI 使用 `cargo package --no-verify`，不替代解包构建和独立消费工程验证。

- `rust-cordis-core`
- `rust-cordis-abi`
- `rust-cordis-host`
- `rust-cordis-macros`

`rust-cordis-sdk`、`cordis-components`、`cordis-cli` 与 examples 目前保持 `publish = false`。

## 发布范围与仓外验收（R2：首发低层 ABI）

**决策（2026-11-03）**：首发承诺的支持路径是**低层 ABI + 固定 revision 的源码依赖**。
registry typed SDK **不在首发范围**；`rust-cordis-sdk` 继续保持 `publish = false`，
`rust-cordis-macros` 虽可打包，但单独发布不构成 typed 门面，直到单独决定并验证
registry 交付。

- [x] 冻结首发承诺的使用路径：低层 ABI + 固定 revision 源码依赖。
  宏生成代码依赖 `::cordis::typed`，目前由不发布的 `rust-cordis-sdk` 提供；不能把四个
  crate 可打包表述成标准 typed 插件已能仅靠 registry 依赖使用。
- [x] 按选定路径在 workspace 外创建消费工程：`../desolate`，直接 path 依赖
  `crates/abi` / `crates/host`，固定 `rust-cordis` revision `9f5ddfb`，完成编译、
  低层 ABI 宏展开、真实 DLL 装载、调用成功/失败、卸载、重载和 v1→v2 升级。
  记录见 `../desolate/R1-REPORT.md`。这是低层 ABI 交付，不冒充已发布 SDK。
- [x] 文档明确 typed 门面不在首发范围；只有未来决定承诺 registry typed SDK 时，才
  处理门面发布、许可证 / 包元数据与打包后构建验证。
- [x] 仓外技术接入与样例归档任务已完成：`../desolate` 的 manifest、构建、调用、
  卸载、重载、升级路径已跑通；2026-11-04 又用 31 篇自造样例完成索引、搜索、
  重载、v1→v2 升级、标签过滤与报告写出。`R1` 剩余项是真实数据与长期维护反馈，
  不是等待第三方复现。第三方复现是额外证据，不是首发门禁。
- [x] R3 调用通道使用契约已完成仓外验证：三个详细入口、正常/超时/取消路径、
  卸载后不重复投递，以及按 `category` / `certainty` 选择动作的窄策略消费者；
  记录见 `../desolate/R3-REPORT.md`。完整重试/补偿不在首发保证范围。
- [x] 真实 agent 首链路已完成仓外验证：模型 token 流、真实归档工具、六条成功/
  失败路径、按实际结果分类、统一终止与多消息部分快照、正常关闭；记录见
  `../desolate/AGENT-REPORT.md`。它证明可开始真实 agent 集成，不证明在线增量
  交付、网络模型、取消、通用预算或历史兼容。

执行状态以 `roadmap.md` 的 R1/R2/R3 与真实 agent 链路为准。仓外验收不必等待
A5-4：后者只能在后续宿主版本出现后验证；首发时保持跨版本兼容待验证，并明确其
范围。
