# cordis-rs

Cordis 的 Rust 实现：服务容器 + epoch 驱动的 fiber 生命周期 + 可逆 effect + 配置驱动装载器 + 插件 ABI。

## 定位与边界

本仓是**可信组件**的插件式服务运行时 / SDK：服务组合、依赖管理、生命周期、
配置装配与可演进 ABI。它可以作为进程内服务组合、微服务实现或 agent 消费链的
底层运行时，但**不是 agent 产品，也不是网络微服务框架**：不提供远程协议、
安全沙箱、独立故障域，也不保证无感热插拔或自动重试安全。
支持契约与证据索引见 [`docs/support-contract.md`](docs/support-contract.md)。

插件来源按 [ADR 0005](docs/adr/0005-trust-model-and-plugin-source-boundary.md)：
只承载第一方、可审查、固定 revision 构建的 T1/T2 插件；第三方、运行时下载或
模型生成且未经人工审查的可执行插件不进入当前运行时。来源 allow-list 是准入，
不是执行隔离：`cordis inspect/validate/status/run/service --source-policy
<allow-list.json>` 会在 `dlopen` 之前检查 sidecar manifest 与 SHA256。**没有策略
默认拒绝**；只有显式传 `--allow-unverified`（或嵌入方调用
`set_allow_unverified(true)`）才进入本地开发模式。生产宿主必须传
`--source-policy`。T2 同步 `apply`/回调不可抢占；`DispatchPolicy` 只覆盖等待
挂起结算，不能中断正在执行的调用。

派发策略可按 **App / 装载器子树** 配置：`LoaderConfig.dispatchPolicy`
（camelCase，如 `{ timeoutMs: 50, outboundTimeoutMs: 20 }`）在挂载前按
`ctx.store` 身份登记，嵌入方也可调用 `set_dispatch_policy_for(&ctx, policy)`。
同一进程的其他 App 不受影响；没有 per-App 条目的 App 才回落到进程级
`set_dispatch_policy`。策略在实例注册时取快照，只影响之后注册的实例。
**明确只支持 App 粒度**，不提供 per-plugin/per-method 配置；同一 App 下多个 loader
的配置是后设置覆盖，不是每 loader 独立。当前 T2 边界如此，不要把它当成插件级策略。

容量准入同样按 App / 装载器子树配置：`LoaderConfig.hostLimits`
（camelCase，如 `{ maxInstancesPerApp: 64, maxDispatchesPerInstance: 256,
maxPendingPerInstance: 64, maxStreamsPerInstance: 8 }`），嵌入方也可调用
`set_host_limits_for(&ctx, limits)`；进程级 `set_default_host_limits` 只是回落。
缺省 **1024 / 1024 / 256 / 32**，保持既有行为。超限时宿主拒绝准入并返回可读的
结构化错误（`code=host_capacity`、`category=capacity`、
`certainty=not_executed`），**不执行插件代码**；所有字段都必须大于 0，**0 不是
“无限”而是非法配置**。这些是宿主侧表项准入（实例数、派发记录、挂起续延、流），
不是每插件的内存/CPU 硬上限；T2 插件与宿主同进程，真正隔离仍属 T3。

依赖计划有两条来源，都在**任何挂载之前**求解：

- **显式配置（覆盖）**：`LoaderConfig.dependencyPlan`（camelCase 数组，每项
  `{ id, version, provides, requires }`，`provides`/`requires` 可缺省）。非空时，
  装载器只求解它：缺失、版本不可满足或同版本多提供者都会让本次装载失败，
  不加载、不应用任何插件，也不会读取 sidecar manifest 计划。
- **sidecar manifest（缺省收集）**：`PluginManifest` 支持可选的 `provides`
  （服务键 → 该插件提供的 semver 版本）与 `requires`（服务键 → semver range）。
  显式 `dependencyPlan` 为空时，`DiskPluginSource::plan_entry` 读取每个静态启用行
  旁的 `<artifact>.manifest.json`，由装载器汇总后调用 `host::plan::resolve`。
  混合来源下只要有一行没有 sidecar/manifest 计划，整次 manifest 预检会保守跳过，
  避免把“未声明”误判成“无提供者”。`LoaderConfig.strictSidecars`（默认 false）
  控制坏 sidecar：false 跳过预检并保留历史行为，true 在装载前硬失败。manifest
  里的版本/范围不是合法 semver 时，解析该行的计划仍会失败。

manifest 示例：

```json
{
  "id": "example.plugin",
  "version": "1.0.0",
  "source_revision": "tree:…",
  "abi_version": 1,
  "sha256": "…",
  "provides": { "svc": "1.2.0" },
  "requires": { "other": "^2" }
}
```

这份计划只用于**装载前预检**，不是运行时依赖注入或隔离：它不改变 `inject` 的服务
可见性，也不证明插件真的提供了声明的服务。初装、HMR / patch 对账和 `reload_one`
前都会重跑预检；对账失败不采纳新树、不卸载已有实例。`cargo xtask seal` 会为三个
示例插件写出 `provides` / `requires`；`seal --verify` 校验的是生成器写出的规范
字节。`PluginDesc` 自述仍不参与求解。

当前执行入口见 [`docs/roadmap.md`](docs/roadmap.md)：仓外真实接入与发布收尾并行。
已知交付缺口、调用通道边界和维护事项见 [`docs/architecture-review.md`](docs/architecture-review.md)。
根目录设计长文保留历史背景，不作为当前进度或支持承诺。

## 快速开始

```text
cargo test --workspace
cargo build -p plugin-greeting -p plugin-hello -p plugin-harness-tool
cargo run -p demo
cargo run -p harness-spike
```

跨平台统一入口（推荐）：

```text
cargo xtask test
cargo xtask consumer
cargo xtask demo
cargo xtask e2e
cargo xtask ci
cargo xtask inventory
cargo xtask docs
cargo xtask matrix
cargo xtask seal
cargo xtask seal --verify <dir>
cargo xtask soak [cycles]
cargo xtask doctor
```

`cargo xtask ci` 依次执行格式检查、inventory/docs/平台矩阵检查、clippy（`-D warnings`）、workspace 测试、独立服务消费者测试、demo 快照比较与真 DLL harness；三平台 GitHub CI 也直接运行 `cargo xtask consumer` 和 consumer 的 fmt 检查。`cargo xtask consumer` 会先构建 `greeting` / `hello` cdylib，再在独立 workspace 中验证真实 ABI 服务组合、`PluginSourcePolicy` + sidecar 准入和关闭。正式封存消费者演练（本地/发布证据，不在 PR CI）：
```text
CORDIS_CONSUMER_SEAL_DIR=dist/cordis-0.1.1-formal cargo test \
  --manifest-path tests/consumer-service/Cargo.toml --test formal_seal
```

`cargo xtask fmt` 可单独检查格式；格式基线已建立，是阻塞门禁。`cargo xtask matrix` 只检查矩阵结构，不验证真实平台结果；实际结果记录在 [`docs/platform-matrix.md`](docs/platform-matrix.md)，是一次三平台 CI 的快照。`cargo xtask seal` 是发布时入口：构建三个宿主目标示例插件，写 sidecar manifest、`allow-list.json`、SHA256/源码树/工具链清单，用同版本 `inspect --source-policy` 逐插件核对身份，并保存 `smoke/inspect.txt`；它不在 `ci` 内，边界见 [`docs/release-seal.md`](docs/release-seal.md)。`cargo xtask soak [cycles]` 运行默认被忽略的宿主资源 soak（默认 1000 轮）和 200 轮真实 `open_staged` soak，同样不进入 `ci`；`scripts/version-matrix.ps1` 运行本地相邻版本矩阵。

### Windows GNU / 非 ASCII 路径

仓库级 `.cargo/config.toml` 只保留 `xtask` 别名，不保存代理和绝对 target 目录。当前 Windows GNU 环境若路径含非 ASCII 字符，请使用本地包装器：

```text
cargo.cmd test --workspace --offline
cargo.cmd xtask demo
```

`cargo.cmd` 会设置本机的 `CARGO_TARGET_DIR`、代理与 `prefer-dynamic=no`；其他环境直接使用 `cargo`。详见 `cargo xtask doctor`。

## CLI

```text
cordis inspect <config.yml> [--json] [--plugin-dir <dir>]
    --source-policy <allow-list.json>        # 生产：加载前校验；无策略默认拒绝
    --allow-unverified                       # 仅本地开发：显式关闭来源准入
cordis status <config.yml> [--patch <file>]... [--plugin-dir <dir>] [--no-strict] [--json]
    --source-policy <allow-list.json> | --allow-unverified
cordis validate <config.yml> [--patch <file>]... [--plugin-dir <dir>]
    --source-policy <allow-list.json> | --allow-unverified
cordis run <config.yml> [--patch <file>]... [--plugin-dir <dir>] [--no-strict]
    --source-policy <allow-list.json> | --allow-unverified
cordis service <config.yml> --name <service-name>   # Windows SCM
    --source-policy <allow-list.json> | --allow-unverified
```

- `inspect` / `validate` 都不挂载插件；`validate` 会叠加 patch 层并报告警告，二者都支持 `--plugin-dir`。`inspect` 仍会加载动态库并调用 `OP_PING` / `OP_VOLATILE_PATHS`，不是“只读文件头”。
- **`status` 会短暂挂载并关闭**：插件会执行 apply / cleanup，可能产生副作用；只应作为诊断命令，不要把它当成只读探针。它列出 `disabled` / `failed` / `pending` / 声明级 `cycles`，失败行返回 `4`；`--json` 输出同一份机器可读报告。
- `run` 装载后等待 Enter、Ctrl-C、Unix `SIGTERM` 或控制台关闭事件，再走 `App::close` 优雅关闭。
- `service` 仅 Windows 可用，必须由 SCM 启动；`--name` 必须与 `sc create` 的服务名一致。
- 退出码：`0` 成功、`2` 用法错误、`3` 配置错误、`4` 运行时错误。

Windows 服务示例（管理员 PowerShell）：

```powershell
$binPath = '"C:\path\cordis.exe" service "C:\path\cordis.yml" --name Cordis --plugin-dir "C:\path\plugins" --source-policy "C:\path\allow-list.json"'
New-Service -Name Cordis -BinaryPathName $binPath -StartupType Automatic
Start-Service Cordis
Stop-Service Cordis
```

`New-Service` 会把带空格的路径作为单个 `BinaryPathName` 写入，避免 PowerShell 5.1
原生参数重拼时丢掉内层引号。

## 生命周期

公开的关闭入口是 `App::close(self)`：递归处置根 fiber、释放子插件、排空宿主派发、清空根级 hook。退出路径与零残留断言见 [`docs/lifecycle.md`](docs/lifecycle.md)。

支持范围是显式契约：非祖先服务消费者有资源关闭顺序保证；**祖先消费者明确不支持**，会留下 `Warn` 诊断；运行期服务环会在建边时被拒绝并记录 `Error`。完整矩阵见 [`docs/support.md`](docs/support.md)。

## 发布状态

**0.1.1 已发布到 crates.io（2026-11-07）**：

- [`rust-cordis-core`](https://crates.io/crates/rust-cordis-core) `0.1.1`
- [`rust-cordis-abi`](https://crates.io/crates/rust-cordis-abi) `0.1.1`
- [`rust-cordis-host`](https://crates.io/crates/rust-cordis-host) `0.1.1`
- [`rust-cordis-macros`](https://crates.io/crates/rust-cordis-macros) `0.1.1`

0.1.1 是元数据补丁：`repository` / `homepage` 已补齐，发布时不再产生该 warning；
ABI 和公开 API 未变。`rust-cordis-sdk`、`cordis-components`、`cordis-cli` 与 examples
保持 `publish = false`。完整门禁见 [`docs/release.md`](docs/release.md)；平台证据见 [`docs/platform-matrix.md`](docs/platform-matrix.md)；夜间 soak 见 [`.github/workflows/nightly-soak.yml`](.github/workflows/nightly-soak.yml)；首发产物封存格式与同版本加载检查见 [`docs/release-seal.md`](docs/release-seal.md)。

首发支持路径已定为**低层 ABI + 固定 revision 的源码依赖**：仓外工程 `../desolate`
已用真实 DLL 验证装载、调用、失败、卸载、重载和 v1→v2 升级。标准 typed 门面
`rust-cordis-sdk` 仍为 `publish = false`，过程宏依赖其 `cordis::typed` 运行时胶水；
registry typed SDK 不在首发范围。仓外技术接入与 31 篇样例归档任务已完成；R1
还需真实数据与长期维护反馈，第三方复现是额外证据而非门禁，详见
[`docs/release.md`](docs/release.md)。

`../desolate` 中的 agent 链（模型流 → 归档工具 → 超时未知 → 关闭）是**仓外
消费者验证**，不是本仓产品主线；agent 事件、模型 provider 与 SSE 传输不会
进入 core/host 公共 API。协议设计仅作为消费者参考保留在
[`docs/agent-stream-protocol.md`](docs/agent-stream-protocol.md)。

## 许可证

本项目采用 `MIT OR Apache-2.0` 双许可，详见 [`LICENSE-MIT`](LICENSE-MIT) 与 [`LICENSE-APACHE`](LICENSE-APACHE)。
