# 保证 × 平台矩阵

> 本表只记录**实际执行过**的平台证据。`待验证` 不等于保证；不能因为“CI 通过”
> 就把未执行的平台填成通过。结构由 `cargo xtask matrix` 检查：每一条保证必须有
> 一行，四个平台列都必须是 `通过` / `失败` / `待验证`，证据列必须引用 `.rs`
> 测试文件。
>
> `Windows GNU` 是本地 `cargo.cmd` 的工具链；`Windows MSVC` 是 GitHub Actions
> `windows-latest` 的默认目标，必须单独记录，不能拿 GNU 结果代替。

当前基线：2026-11-02 完成首次三平台 CI（run
[`41460414477`](https://github.com/yun1091724578/rust-cordis/actions/runs/41460414477)，
commit `ea0489e`）。Windows MSVC / Linux / macOS 的 `test` job 均通过完整
`cargo test --workspace --all-targets --locked`、`cargo xtask demo` 与
`cargo xtask e2e`；Windows GNU 另有本地 `.\cargo.cmd xtask ci` 全量通过。
逐条保证的测试名见 `docs/support-contract.md`。

## 平台矩阵

| 保证 | Windows GNU | Windows MSVC | Linux | macOS | 证据 |
|---|---|---|---|---|---|
| 撞名的 `provide` 失败后不增长恢复栈 | 通过 | 通过 | 通过 | 通过 | `crates/core/tests/weak_audit.rs` |
| 手动释放的 effect 不留在恢复栈 | 通过 | 通过 | 通过 | 通过 | `crates/core/tests/weak_audit.rs` |
| 丢弃 `dispose` Future 后剩余恢复栈仍执行 | 通过 | 通过 | 通过 | 通过 | `crates/core/tests/lifecycle_races.rs` |
| 提供者先等直接消费者拆完再关资源 | 通过 | 通过 | 通过 | 通过 | `crates/core/tests/lifecycle_races.rs` |
| 卸载行后宿主拆除时不残留插件对象 | 通过 | 通过 | 通过 | 通过 | `crates/host/tests/loader_release.rs` |
| 取消后迟到的 `fulfill` 被拒绝 | 通过 | 通过 | 通过 | 通过 | `crates/host/tests/abi_m2c.rs` |
| 未知 dispatch 的 abort 是可读错误 | 通过 | 通过 | 通过 | 通过 | `crates/host/tests/abi_m2c.rs` |
| 同一键原地换值不重挂消费者 | 通过 | 通过 | 通过 | 通过 | `crates/components/tests/registry.rs` |
| ABI 前缀与尺寸判界 | 通过 | 通过 | 通过 | 通过 | `crates/host/tests/abi_matrix.rs` |
| 跨 ABI 错误能力位与结构化载荷（传输层） | 通过 | 通过 | 通过 | 通过 | `crates/abi/tests/error_format.rs` |
| 配置 schema 在装载前可见（插件自述，不据此验证） | 通过 | 通过 | 通过 | 通过 | `crates/abi/tests/schema.rs` |
| Pending 诊断给出精确缺失键与原因 | 通过 | 通过 | 通过 | 通过 | `crates/core/tests/pending_diagnostics.rs` |
| 看门狗超时撤销派发记录（四条路径） | 通过 | 通过 | 通过 | 通过 | `crates/host/tests/abi_m2_timeout.rs` |
| 插件 panic 被截在 ABI 边界内 | 通过 | 通过 | 通过 | 通过 | `crates/abi/src/sdk.rs` / `crates/abi/tests/invalid_schema_entry.rs` |
| 重复成功/失败/丢弃 Future/卸载/waterfall/stream/重载后资源计数回到基线 | 通过 | 通过 | 通过 | 通过 | `crates/host/tests/resource_baseline.rs` |
| 多轮暂存后台账回到基线 | 通过 | 通过 | 通过 | 通过 | `crates/host/tests/staged.rs` |
| 冻结 ABI v1 真 cdylib × 新宿主 | 通过 | 通过 | 通过 | 通过 | `crates/host/tests/frozen_plugin.rs` |
