# 发布清单与门禁

## 可发布 crate 与发布顺序

按依赖顺序发布到 crates.io（`cordis-components`、`cordis-cli` 与 `examples/*` 保持 `publish = false`）：

1. `rust-cordis-abi`
2. `rust-cordis-core`
3. `rust-cordis-macros`
4. `rust-cordis-host`
5. `rust-cordis-sdk`（库名 `cordis`）

## 当前版本

- **`0.1.5`**（tag `v0.1.5`）：为公开 `cordis_abi::ErrorPayload` 实现 `Display + Error`，宿主 `payload_of` 优先保留其结构化 `code` / `category` / `certainty` / `retry` / `detail`。
- **`0.1.4`**（tag `v0.1.4`）：`Ctx::call_iface_stream` 透传 `correlation_id`。
- **`0.1.3`**（tag `v0.1.3`）：首次发布门面 crate `rust-cordis-sdk`，引入 ADR 0011 跨 ABI `correlation_id` 传播。

## 发布前门禁

- `cargo xtask ci`：格式检查、契约/API 清单/平台矩阵检查、`clippy -D warnings --all-features`、全量测试、doctest、demo 快照与真 DLL 消费者测试。
- `cargo xtask package-check`：在隔离目录用打包产物构建消费者并跑一次跨 ABI 服务调用（验证无本地 `path` 后门）。
- 产物封存与历史版本回归格式见 [`docs/release-seal.md`](release-seal.md)；平台快照见 [`docs/platform-matrix.md`](platform-matrix.md)；夜间 soak 见 [`.github/workflows/nightly-soak.yml`](../.github/workflows/nightly-soak.yml)。
- 许可证：`MIT OR Apache-2.0`（根目录 `LICENSE-MIT` / `LICENSE-APACHE`）。
