# 发布清单与门禁

## 可发布 crate 与发布顺序

按依赖顺序发布到 crates.io（`cordis-components`、`cordis-cli` 与 `examples/*` 保持 `publish = false`）：

1. `rust-cordis-abi`
2. `rust-cordis-core`
3. `rust-cordis-macros`
4. `rust-cordis-host`
5. `rust-cordis-sdk`（库名 `cordis`）

## 当前版本

- **`0.1.6`**（待发布）：
  - **结构化流错误与受管后台任务**：新增 `StreamSink::fail_error`、`HostApi::ctx_stream_fail_payload`（尾追加独立协商的结构化流错误入口，保留原 `ctx_stream_fail` 纯文本语义并对 `0.1.5` 及更早宿主自动降级）与 `abi::sdk::Ctx::stream_fail_payload`；新增 `Ctx::spawn_scoped` / `Fiber::spawn_scoped` / `SpawnScopedError` / `ReloadDeferred`（归属 fiber 生命周期的受管后台任务，自动取消等待、首次 poll 前后释放保序、防重入自锁与 ADR 0011 `correlation_id` 透传）。
    - **`ReloadDeferred` 调用方处理**：受管后台任务（`spawn_scoped` / 原生流任务）内部重入调用所属、祖先或提供者 fiber 的 `Fiber::restart` / `Fiber::update` / `Fiber::update_with` 触发重载时，为避免与 `dispose_native_tasks` 自锁且不误报尚未执行的新一代 `apply()` 成功，调用会返回 `Err`（`err.downcast_ref::<ReloadDeferred>()` 可精确匹配），而不返回 `Ok(())` 或 `UpdateOutcome::Restarted`。调用方应据此区分“重载已受理推迟”与“重载失败”：检查 `deferred.is_scheduled()`（为 `true` 表示收敛已移交后台任务，当前任务应尽快退出以放行卸载；为 `false` 表示当前无活跃 Tokio 运行时，需在外部显式调用 `fiber.settle().await`），并在受管任务外部通过 `fiber.settle().await` / `fiber.state()` / `fiber.error()` 观测新一代的最终收敛结果。
  - **可观测关停、结构化热重载报告与三段等待预算**：
    - `core` 新增 `CloseReport`、`CloseStatus`（`Completed` / `CompletedWithErrors` / `TimedOut` / `Deferred`）、`CleanupError`、`CleanupErrorKind`、`LifecyclePolicy`（按 `apply_timeout_ms` / `drain_timeout_ms` / `dispose_timeout_ms` 拆分预算，缺省全 `None` 且拒绝 `Some(0)`）、`BackgroundCleanupGuard`，以及 `App::set_lifecycle_policy` / `App::close_with_report`、`Fiber::dispose_with_report` / `cleanup_errors` / `record_cleanup_error` / `defer_background_cleanup` / `pending_background_cleanups` / `wait_background_cleanups` / `should_offload_dispose`、`Store::pending_background_cleanups` / `wait_background_cleanups`。
    - `host` 统一 T2 动态库与暂存副本联合保活（`LoadedLibrary`，ADR 0005 §4），支持 `OP_APPLY` / `OP_CLEANUP` / `OP_DISPOSE` 预算移交与后台排空；新增 `ReloadReport`、`ReloadStatus`（显式区分 `Active` 与 `Pending`）、`reload_one_report`、`reload_one_with_plugin`、`LoaderState::last_reload` 与 `loader.report["lastReload"]`；对账卸载/替换、失败挂载与热重载回滚统一遵守 `dispose_timeout_ms` 并在旧/失败实例后台清理归零前阻断旁路重挂与并发更新。
    - **后台清理未归零时的重载语义**：`restart` / `update` / 依赖驱动的重新激活若遇到上一代超出预算、已移交后台的清理尚未完成，fiber 停在 `Pending`（不记 `error`），返回 `Err(ReloadDeferred)` 且 `is_waiting_for_cleanup() == true`，并在 `Fiber::wait_background_cleanups()` 归零后**自动重试**装载新一代；超时本身记为 `CleanupErrorKind::Timeout`。只有新一代 `apply()` 真实失败才进入 `Failed`（仍需 `update()` 恢复）。
  - **上游失败关联、行级启动策略与宿主绑定方法白名单**：
    - `core` 新增 `CallerIdentity`、`MethodUnauthorized`、`CallAuthHook`、`Context::caller_identity`、`Fiber::host_label`、`Store::set_call_auth_hook` / `has_call_auth_hook` / `authorize_call` / `plugin_with_host_label`。
    - `host` 新增 `RowStartup`（YAML `startup` / `required` / `optional`，含组行继承与回写保真）、`FailureCategory`（`RuntimeUnavailable` / `Source` / `Integrity` / `PersistedState`）、`FailureDisposition`（`Fail` / `Degrade`）、`StartupPolicy`、`MethodAllowList`（per-App 默认拒绝，覆盖宿主受管 `call_iface` / `call_iface_stream` / T2 `ctx_call_iface`，按宿主绑定身份校验且不覆盖原生类型直调与事件派发）、`LoadFailure`、`FailedRow`、`PendingUpstreamFailure`、`CorrelatedPendingInject`、`CorrelatedPendingRow`、`LoaderState::failed_rows` / `degraded_rows` / `persisted_state_errors` / `correlated_pending_report`、`PluginSource::load_classified` / `plan_entry_classified`（与 `strict_sidecars = true` 依赖预检对齐）。
    - `cordis-cli`（`publish = false`）同步接入上述策略参数与诊断输出（`RunOptions` / `RunReport::close_report` / `StatusReport` 及 CLI 参数 `--strict-sidecars`、`--degrade-source`、`--degrade-integrity`、`--degrade-persisted-state`、`--apply-timeout-ms`、`--drain-timeout-ms`、`--dispose-timeout-ms`）。
- **`0.1.5`**（tag `v0.1.5`）：为公开 `cordis_abi::ErrorPayload` 实现 `Display + Error`，宿主 `payload_of` 优先保留其结构化 `code` / `category` / `certainty` / `retry` / `detail`。
- **`0.1.4`**（tag `v0.1.4`）：`Ctx::call_iface_stream` 透传 `correlation_id`。
- **`0.1.3`**（tag `v0.1.3`）：首次发布门面 crate `rust-cordis-sdk`，引入 ADR 0011 跨 ABI `correlation_id` 传播。

## 发布前门禁

- `cargo xtask ci`：格式检查、契约/API 清单/平台矩阵检查、`clippy -D warnings --all-features`、全量测试、doctest、demo 快照与真 DLL 消费者测试。
- `cargo xtask package-check`：在隔离目录用打包产物构建消费者并跑一次跨 ABI 服务调用（验证无本地 `path` 后门）。
- 产物封存与历史版本回归格式见 [`docs/release-seal.md`](release-seal.md)；平台快照见 [`docs/platform-matrix.md`](platform-matrix.md)；夜间 soak 见 [`.github/workflows/nightly-soak.yml`](../.github/workflows/nightly-soak.yml)。
- 许可证：`MIT OR Apache-2.0`（根目录 `LICENSE-MIT` / `LICENSE-APACHE`）。
