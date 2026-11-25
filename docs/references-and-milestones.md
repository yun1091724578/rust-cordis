# 里程碑代号、上游对照与参考文献索引

> 本文件用于解释 `rust-cordis` 源码注释与 ADR 中出现的内部代号、上游 TypeScript Cordis 文件行号及理论出处，方便新贡献者独立导航，而不必依赖原作者记忆。

---

## 1. 理论与上游实现对照

- **Cordis 设计与形式化语义（注释中简称为“论文 §X.Y” / “Def N” / “Theorem N”）**：
  - 注：对应外部参考手稿 / 内部设计草稿（不随本仓库公开分发），仅作为源码历史溯源线索；**权威契约一律以本仓 Rust 代码、ADR 与自动化测试为准**。核心概念对照如下：
    - `Def 26/27`：提供者侧 interception 元数据合并（`d(k) ⊕_k ι(k)`，右偏）。
    - `Def 48/76`：`provision` 声明与实际提供服务集合的一致性审计。
    - `Algorithm 5` / `Theorem 63`：先卸载依赖者再拆除提供者，以及提交视图（`CommittedView`）在卸载全程保活。
    - `§6.2–§6.6`：跨进程载体边界、沙箱非目标、访问中介与 `<name>@<major>` 服务键命名。
- **上游 TypeScript Cordis 源码对照（注释中简称为 `fiber.ts:NNN`、`reflect.ts:NNN`、`events.ts:NNN`、`loader/src/index.ts:NNN`、`entry.ts:NNN`、`patch.ts:NNN`）**：
  - 对应 TypeScript 版 `cordis`（`@cordisjs/core` 与 `@cordisjs/loader`）的语义对照点，用于标明 Rust 版在状态机、提交视图、事件短路（`bail`）与配置装载行为上与上游保持一致的依据。
  - **权威契约以本仓 Rust 代码与回归测试为准**；上游 TS 行号仅作历史语义溯源参考，本仓不内置 TS 源码。
- **早期设计笔记（`《SDK方案.md》`、`《原理.md》`）**：
  - 为本仓建库初期的外部设计草稿，其有效结论已全部沉淀为仓库内的 `docs/adr/*.md`、`docs/support-contract.md` 与 `SDK指南.md`；新变更只以仓库内文档与测试为准。

---

## 2. 源码注释中的里程碑与设计代号

| 代号前缀 | 含义 | 常见落点 |
|---|---|---|
| **M0** | 最小可运行骨架与构建基础设施（ABI v1 冻结前缀、`copy-on-attach`、`cargo.cmd`） | `crates/abi`、`cargo.cmd` |
| **M1 / M1-1 / M1-2** | 动态库生命周期、事务性热替换（换名重挂）与派生句柄（`isolate`/`intercept`/`use_child`） | `crates/host/src/lib.rs`、`loader.rs` |
| **M2 / M2b / M2c / M2-1 / M2-2** | 异步入站挂起/结算（`STATUS_PENDING`/`ctx_fulfill`）、`next`/`abort`/`CancelToken`、`ctx_require`/`ctx_set` | `crates/abi/src/sdk.rs`、`crates/core/src/cancel.rs`、`crates/host` |
| **M3 / M3-2 / M3-3 / M3-4** | `intercept` 元数据合并、`internal/config` 瀑布、`internal/get` / `internal/set` 钩子、`internal/service` 广播 | `crates/core/src/store.rs`、`fiber.rs` |
| **M4 / M4-4 / M4-5** | 弱引用防环纪律、配置瀑布防静默回落、`volatile` 原地热更新 | `crates/core/src/volatile.rs`、`crates/core/tests/weak_audit.rs` |
| **M5 / M5-1** | `provision` 审计账与 FFI/apply/disposer `catch_unwind` panic 隔离护栏 | `crates/abi/src/sdk.rs`、`crates/core/src/fiber.rs` |
| **M6** | 过程宏与 typed SDK 面（`#[cordis::service]` / `#[cordis::events]` / `#[cordis::config]` / `#[cordis::plugin]`） | `crates/macros`、`crates/sdk` |
| **M7** | 组件层具名槽位注册表（`registry@1` / `RegistryHandle`） | `crates/components` |
| **M8** | T3 第二载体决策评估（见 [ADR 0001](adr/0001-second-carrier.md)，已决定骨架内不实现） | `docs/adr/0001-second-carrier.md` |
| **D1–D9** | 核心设计红线（如 D1 信任模型硬话、D3 状态码挂起协议、D8 测试 seam、D9 `CallFn` 假插件） | `SDK指南.md`、`crates/abi`、`crates/host` |
| **G*** | 契约保证编号（如 G7 注册表、G12 typed 事件、G15 copy-on-attach） | `docs/support-contract.md`、`crates/abi` |
| **O* / P*** | 装载器重构与并发收敛批次号（如 O11 拆分 `ffi_buf.rs`、P1/P2 对账与隔离域修复） | `crates/host/src/loader.rs`、`ffi_buf.rs` |

---

## 3. 文档权威优先级

当注释与文档出现歧义时，按以下顺序为准：
1. 自动化测试与 `xtask` 门禁（`xtask arch` / `xtask docs` / `xtask inventory --check`）
2. `docs/support-contract.md` 与 `docs/adr/*.md`
3. `README.md` 与 `SDK指南.md`
4. 源码内历史溯源注释
