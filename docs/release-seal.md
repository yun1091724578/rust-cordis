# 首发产物封存

本文件定义首发插件的封存格式与验证入口。它只解决两件事：

1. 第一次发布时，把 release 插件二进制、源码树、工具链、ABI 版本与 SHA256
   固定在同一个目录里；
2. 用**同版本宿主**证明这批产物能被加载。

封存对象是 `examples/plugins/` 下三个 `publish = false` 的示例插件
（greeting / hello / harness-tool），不是四个可发布 crate。后者由 CI 的
package job 产出 `.crate`，其校验和不在本封存内。

它**不证明跨版本兼容**。历史产物 × 后续宿主版本的结论仍要等后续宿主版本实际
加载，见 `docs/release.md` 与 `docs/support-contract.md`。

## 命令

```text
cargo xtask seal [--out <dir>] [--root <workspace>]
cargo xtask seal --verify <dir>
```

`--root` 用于给已发布 tag 的干净 worktree 生成正式 `/2` 封存：构建与源码摘要
来自 `--root` 指定的树，smoke 的 `inspect` 仍使用当前工作区的宿主 CLI（这样
历史 tag 不需要回填新的 `--source-policy` 客户端）。`--root` 不能与 `--verify`
同用；默认输出目录在 `--root/dist/` 下。

默认输出目录是：

```text
dist/cordis-plugins-<workspace-version>-<target>/
```

`seal` 会：

- 以 `--release --locked` 构建 `plugin-greeting` / `plugin-hello` /
  `plugin-harness-tool` 三个 cdylib；显式使用**宿主目标**，不做交叉编译；
- 把产物复制到 `plugins/`，逐个计算 SHA256；
- 为每个 artifact 写 `plugins/<dll>.manifest.json`：
  `{id, version, source_revision, abi_version, sha256}`。`id` 是插件身份名
  （`greeting` / `hello` / `harness-tool`），`version` 是 workspace 版本，
  `sha256` 是该 DLL 的完整 SHA-256；
- 在封存根写 `allow-list.json`：`{"allowed":[...]}`，条目与三个 sidecar
  manifest 逐字段一致。它是 `cordis --source-policy` 的输入格式；
- 记录 workspace 版本、ABI 主版本、结构化错误格式版本、目标三元组、rustc /
  cargo / RUSTFLAGS，以及封存时 workspace 工作树的聚合 SHA256；
- 记录 smoke 宿主身份 `host.workspace_version` / `host.source_sha256` /
  `host.source_files`。普通 `seal` 的 host 与 source 是同一棵树；`--root`
  的 host 是当前工作区，若两者 `source_sha256` 不同，smoke 只是跨版本加载检查，
  不能当作同版本证据；
- 生成只读 smoke 配置，并用同版本 `cordis-cli inspect --source-policy
  allow-list.json` 加载三个插件（先验 sidecar / allow-list，再
  `LoadedPlugin::open` + `OP_PING` / `OP_VOLATILE_PATHS`；不挂载、不执行
  `apply`），逐个断言 `(plugin: <名称>)`；
- 把 `inspect` 的完整 stdout 保存为 `smoke/inspect.txt`；
- 写 `seal.json` 与 `SHA256SUMS`（覆盖三个 artifact、三个 sidecar manifest、
  `allow-list.json`、smoke 配置与 inspect 证据），再做一次只读自检。

`source.sha256` 是**封存时**工作树的快照，不是当前工作区的实时摘要；linked
worktree 根上的 `.git` 指针文件不会进入摘要。`host.source_sha256` 与
`source.sha256` 相同才表示 smoke 由同版本树执行。校验封存
不会重新计算它。它包含未跟踪文件，不读 `.gitignore`；排除 `.git/`、
`target/`、`dist/`、`.cargo-home/`、`node_modules/`、`CARGO_TARGET_DIR`
指向的目录、临时封存目录和编辑器交换文件。ABI / error_format 记录的是构建
所用 workspace 的常量（`cordis_abi::ABI_VERSION` / `ERROR_FORMAT_V1`），不是
逐个产物探测的自述值。

### `source_revision` 约定

sidecar 的 `source_revision` 固定写成 `tree:<source.sha256>`（例如
`tree:41cf6794…`）。seal 没有 git commit 字段：源码树聚合 SHA256 是唯一
可重建、与构建输入一一对应的 revision 标识。它**不是提交号**——包含未跟踪
文件、不读 `.gitignore`，所以同一 commit 的工作树如果有额外文件，也会得到
不同的 `source_revision`。需要 git 身份时，把 `source_revision` 与发布记录
里的 commit / tag 一起归档。

`--verify` 不构建，只检查：

- 清单字段、三个预期 artifact 的 package/crate 集合、路径、大小与 SHA256；
- 每个 artifact 的 sidecar manifest 存在、是普通文件，且 id / version /
  source_revision / abi_version / sha256 与 `seal.json` 推导出的期望值逐字一致；
- 封存根 `allow-list.json` 存在，且 `allowed` 条目与三个 manifest 逐字段一致；
- `plugins/`、`smoke/` 与封存根没有未登记文件；
- smoke 配置与 `smoke/inspect.txt` 的存在、大小与 SHA256；
- `SHA256SUMS` 覆盖三个 artifact、三个 sidecar、`allow-list.json`、smoke 配置
  与 inspect 证据，且没有缺项 / 多项；
- `smoke.checked == true`。

其中 sidecar / `allow-list.json` 检查只适用于 `/2`；`/1` 历史封存只做
artifact / smoke / `SHA256SUMS` 检查（见“目录格式”）。

它只证明“目录内容与清单一致”，**不证明来源或真实性**：清单没有签名，能同时
改写 `seal.json`、sidecar、`allow-list.json` 与 `SHA256SUMS` 的人也能让校验
通过；`smoke.checked` 是封存时记录的结论，`--verify` 不重跑加载检查。
`SHA256SUMS` 是标准 `sha256sum` 格式，可在没有本仓库的机器上直接复核 artifact
字节。

manifest 与 `allow-list.json` 是**准入输入**，不是执行隔离：它们把“允许加载
哪个 hash / 哪个 revision”变成可检查的文件，但不阻止拿到策略文件的人放行
别的插件，也不限制已加载插件的运行时行为。

## 目录格式

```text
<seal>/
  seal.json
  SHA256SUMS
  allow-list.json
  plugins/
    greeting.dll          # Linux: libgreeting.so；macOS: libgreeting.dylib
    greeting.dll.manifest.json
    hello.dll
    hello.dll.manifest.json
    harness_tool.dll
    harness_tool.dll.manifest.json
  smoke/
    cordis.yml
    inspect.txt
```

`seal.json` 的 `format` 新写为 `cordis-plugin-seal/2`。`/2` 的唯一变化是
sidecar manifest 与 `allow-list.json` 成为必需文件并进入 `SHA256SUMS`；
`/1` 封存（例如 0.1.0/0.1.1 的 r3 目录）保持只读复验能力，但只检查
artifact / smoke / `SHA256SUMS`，不检查 provenance 文件。`/2` 不会静默降级：
缺少 sidecar 或 allow-list 即失败。

## 不可覆盖

输出目录已存在且非空时直接拒绝。首发产物一旦封存，不得用第二次运行覆盖；
需要新的封存时换 `--out`，或先归档旧目录再另建目录。临时目录使用随机后缀，
不会删除已存在的同名目录。

## 同版本加载检查的边界

`inspect` 会加载动态库并调用 `OP_PING` / `OP_VOLATILE_PATHS`，因此能证明
ABI 版本、结构尺寸、入口符号与探活可用；它**不挂载**，也不执行插件
`apply`，所以不证明业务行为。真正的挂载 / 回调 / 卸载仍由
`crates/host/tests/frozen_plugin.rs` 与 harness e2e 覆盖；`legacy_error.rs`
只覆盖旧插件的错误文本，不覆盖完整挂载。

“同版本”指 `seal.json` 的 `source.sha256` 所代表的那棵树构建出的宿主，不是
版本号 `0.1.0` 本身。

## 本地演练记录（2026-11-02，非正式发布）

在 Windows GNU 上运行：

```text
.\cargo.cmd xtask seal --out dist\cordis-plugins-0.1.0-x86_64-pc-windows-gnu-r3
.\cargo.cmd xtask seal --verify dist\cordis-plugins-0.1.0-x86_64-pc-windows-gnu-r3
```

两条命令均通过。封存目录：

```text
dist/cordis-plugins-0.1.0-x86_64-pc-windows-gnu-r3/
```

本次记录的 ABI 版本为 1，结构化错误格式为 1，`rustc 1.98.1` /
`cargo 1.98.1`；三个 artifact、smoke 配置与 `smoke/inspect.txt` 的 SHA256
以该目录的 `seal.json` 与 `SHA256SUMS` 为准。r3 是**封存时**工作树的快照：
之后修改文档或测试不会让 `--verify` 失败，但也不代表当前工作树与 r3 一致。
`dist/` 不提交；正式首发时要把整个目录归档到发布存储，不能只保存其中某个 DLL。
