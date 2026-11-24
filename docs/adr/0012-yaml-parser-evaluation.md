# ADR 0012：YAML 语料基线与 `serde_yaml` 替代库评估（O12）

- 状态：已采纳（2026-11-23）
- 关联：`crates/host/src/patch.rs`、`crates/host/tests/yaml_corpus.rs`

## 1. 背景与现状

本仓在 `crates/host` 中通过 `serde_yaml = "0.9"` 解析与回写 `cordis.yml` / `cordis.patch.yml` 及 `LoaderConfig`：

1. **使用面高度集中**：
   - `crates/host/src/patch.rs`：将 `!!js` 在引号/注释外改写为本地标签 `!cordis-js`，经 `serde_yaml::from_str::<serde_yaml::Value>` → `serde_json::to_value` 解为 `Entry`/`Patch`；回写（`dump_entries` / `dump_overlay`）构造 `serde_yaml::Value::Tagged` 后用 `serde_yaml::to_string` 序列化并还原 `!!js`。
   - `crates/host/src/plan.rs` / `loader.rs`：直接反序列化配置结构体（`PlanEntrySpec` 等）。
2. **回写语义澄清**：
   - `dump_entries` 走的是“归一化 `Entry` → `serde_json::Value` → `serde_yaml::Value` → `to_string`”的**值级语义序列化**，**并非原文无损 round-trip**（不保留用户注释、键序或引号风格）。
3. **风险来源**：
   - `serde_yaml` 0.9（`0.9.34+deprecated`）已被原作者归档并标注 deprecated，底层依赖 C 转译的 `unsafe-libyaml`。

## 2. 候选库评估

| 候选库 | 维护状态 | 许可 | API / `Value::Tagged` 兼容 | 底层实现与安全性 | 结论 |
|---|---|---|---|---|---|
| **`serde_yaml` 0.9（当前）** | 已归档（deprecated），无已知影响本仓配置解析的未修复漏洞 | MIT / Apache-2.0 | 基线实现 | `unsafe-libyaml`（YAML 1.1） | **0.1.x 阶段保留** |
| **`yaml_serde` 0.10**（YAML 官方组织维护） | 活跃维护 | MIT / Apache-2.0 | **100% drop-in 兼容**（支持 `package = "yaml_serde"` 重命名，`Value::Tagged` 语义一致） | `libyml`（仍为 C-FFI / YAML 1.1） | **首选 drop-in 替换候选** |
| **`serde_yaml_ng`** | 社区维护 | MIT / Apache-2.0 | 100% drop-in 兼容 | `unsafe-libyaml`（与 `serde_yaml` 同源） | 备选（与 `yaml_serde` 相比无架构优势） |
| **`serde-saphyr`** | 活跃维护 | MIT / Apache-2.0 | **非 drop-in**（无 `serde_yaml::Value::Tagged` 中间树，需重写 `patch.rs` 标签读写层） | 纯 Rust YAML 1.2（无 C-FFI） | **0.2 纯 Rust / YAML 1.2 长期候选** |
| **`serde_yml`** | **已废弃 / 归档**（历史存在 `RUSTSEC-2025-0068`） | MIT / Apache-2.0 | drop-in | 曾使用旧 `libyml` 后端 | **明确拒绝** |

## 3. 决策

1. **0.1.x 阶段明确选择“暂不替换 `serde_yaml 0.9` 依赖”**：
   - `rust-cordis-host` 是已发布/准备发布的 0.1.x crate，保持 `Cargo.lock` 与离线可构建性稳定优先；
   - 两个 drop-in 替代（`yaml_serde` / `serde_yaml_ng`）仍共享同源 `libyml` / YAML 1.1 后端，在无安全公告触发前替换收益有限；
   - 纯 Rust 方案（`serde-saphyr`）涉及 `patch.rs` 标签机制重写，适合在 0.2 或出现明确安全/YAML 1.2 需求时单独立项。
2. **建立可重跑的 YAML 语料基线**：
   - 新增 [crates/host/tests/yaml_corpus.rs](../../crates/host/tests/yaml_corpus.rs)，覆盖 `!!js` 往返、单双引号转义、注释与词尾边界、块标量、其他本地标签（`!other`）、UTF-8 BOM、显式 `null` 清空与缺省保留、嵌套 `group` 补丁与 `dump_overlay` 往返、以及 6 类非法输入 fail-loud 断言。
3. **未来迁移触发条件与路径**：
   - **若出现 `unsafe-libyaml` 安全公告或下游严格禁用 deprecated 依赖**：直接切换为 `serde_yaml = { package = "yaml_serde", version = "0.10" }`，以 `yaml_corpus.rs` + `patch.rs` + `interpolation.rs` 全绿作为验收门禁。
   - **若 0.2 计划彻底移除 C-FFI / 升级 YAML 1.2**：迁移至 `serde-saphyr`，同样以上述语料测试全绿为门禁。
