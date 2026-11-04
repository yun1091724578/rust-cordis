# 支持契约（0.1）

本文件把散在 ADR、`docs/support.md` 与 `docs/release.md` 里的边界集中到一处，
并给每一条“保证”标注证据。

- `docs/support.md` 记录**生命周期支持矩阵**的实现细节；
- 本文件回答**这个 SDK 承诺什么、不承诺什么、承诺的东西由哪个测试兜底**。

保证索引检查只验证证据引用的有效性，不能自动证明断言充分或某个平台实际执行。
平台结果以 `docs/platform-matrix.md` 的 run/commit 快照为准；独立使用与交付缺口见
`docs/architecture-review.md`，其任务统一在 `docs/roadmap.md`，本轮不新增保证。

## 定位

为**可信组件**提供服务组合、依赖管理、生命周期、配置装配和可演进的插件协议。

三个限定词都不是修辞：

- **可信组件**：T1 / T2 与宿主同信任域。realm / scope 提供可见性隔离，不是安全边界。
- **服务组合**：同进程、按 key 装配。不是服务发现，也不是远程调用协议。
- **可演进**：ABI 前缀与 `size` 探测保证尾追加兼容；不保证跨 major 的语义兼容。

T1 = 链进宿主的原生插件；T2 = 同进程动态库插件；T3 = 进程外 / Wasm（未实现，
见 ADR 0001）。

## 分层与归属

| 层 | 承担 | 不承担 |
|---|---|---|
| core | 服务与生命周期机制、最小通用调用契约 | 重试策略、审批、业务账本、负载均衡 |
| abi | 线格式、能力协商、兼容与降级 | 信任判定、业务执行保证 |
| host | 装载、派发、看门狗、宿主可证明的执行阶段 | 替外部系统证明操作成功或幂等 |
| sdk / macros | 易用 API、编码适配、声明生成 | 隐式补出不存在的权限或调用事实 |
| components | 当前的 `registry`；有消费者后才评估阶段 5 的限流、队列、重试、路由 | 修改 core 的基础语义 |
| 宿主产品 / harness | 工具协议、审批、凭据、执行账本、恢复 | 把领域需求包装成所有插件的强制机制 |

## 非目标

以下六条不是“以后会做”，而是当前**明确不承诺**。任何一条要变成承诺，
必须先有独立 ADR 与验收标准。

1. **安全沙箱。** 同进程 DLL 是进程的一部分；T3 只解决地址空间与故障域，
   不提供信任边界。真正的隔离是 OS / Wasm 执行边界的产品决策。
2. **崩溃恢复 / 业务回滚。** `Disposable` 与 effect 是进程内可逆；
   生命周期清理不等于事务回滚，也不等于崩溃后的任务恢复。
3. **无感热插拔。** `registry.replace` 只做**原地换值**：槽位身份不变、依赖者
   epoch 不变（有测试）；在飞调用持有解析时的 `Arc`，因此不会读到半成品——
   这是 `Arc` 的语义，不是额外的回归保证。它不保证状态迁移，也不保证新实现
   与旧实现的契约一致。
4. **远程调用协议。** 本地 ABI 是同步重入 + 单入口 op 协议；
   跨进程需要请求关联、断连语义、协议协商，不能靠包一层网络视为等价。
5. **自动重试安全 / exactly-once。** 详细错误通道已传递 `code` / `category` /
   `certainty` / `retry` 等字段，文本降级时信息仍可能未知；这些字段不是重试许可。
   当前没有通用副作用声明、幂等键、截止时间或预算协议，不能单凭错误载荷证明可安全重放。
   是否重试是策略，由 agent / 宿主 / 领域组件决定。
6. **授权与审批。** manifest 可以声明能力要求，但它是**插件自述**，不是审批结果；
   授权主体与执行检查在宿主侧。

## 保证索引

> `cargo xtask docs` 会检查本表：状态为“保证”的行必须引用带 `#[test]` /
> `#[tokio::test]`、未被 `#[ignore]` 的测试；引用不存在的测试、或“保证”行没有证据，
> 命令以非零退出。
>
> 本索引覆盖**跨组件、可独立测试**的保证。`docs/support.md` 的生命周期矩阵是行为细节；
> 在逐行迁移到本表之前不重复登记。

| 保证 | 状态 | 证据 | 备注 |
|---|---|---|---|
| 撞名的 `provide` 失败后不增长恢复栈 | 保证 | `crates/core/tests/weak_audit.rs::failed_provide_does_not_leave_recovery_entries` | 失败次数不影响 `effect_count` 基线 |
| 手动释放的 effect 不留在恢复栈 | 保证 | `crates/core/tests/weak_audit.rs::manually_released_effects_do_not_accumulate` | 长期运行不得线性增长 |
| 丢弃 `dispose` Future 后剩余恢复栈仍执行 | 保证 | `crates/core/tests/lifecycle_races.rs::dropping_dispose_future_keeps_the_remaining_restore_stack` | 清理任务不随 Future 丢弃 |
| 提供者先等直接消费者拆完再关资源 | 保证 | `crates/core/tests/lifecycle_races.rs::provider_waits_for_consumer_before_closing_resources` | 祖先消费者不在支持范围内，见 `support.md` |
| 卸载行后宿主拆除时不残留插件对象 | 保证 | `crates/host/tests/loader_release.rs::unloading_a_row_with_a_live_dependent_also_releases_the_plugin` | 测试在 `host.dispose()` 之后断言 drop；不是“卸载行当下” |
| 取消后迟到的 `fulfill` 被拒绝 | 保证 | `crates/host/tests/abi_m2c.rs::fulfill_after_abort_is_rejected` | 取消只终止本地等待，不代表远端已停止 |
| 未知 dispatch 的 abort 是可读错误 | 保证 | `crates/host/tests/abi_m2c.rs::aborting_an_unknown_dispatch_id_is_a_readable_error` | 不静默成功 |
| 同一键原地换值不重挂消费者 | 保证 | `crates/components/tests/registry.rs::consumer_epoch_is_unchanged_when_a_named_impl_is_replaced` | 只覆盖“换值”，不覆盖“状态迁移” |
| ABI 前缀与尺寸判界 | 保证 | `crates/host/tests/abi_matrix.rs::abi_compatibility_matrix` | 每格真读一次 `field_at` |
| 跨 ABI 错误能力位与结构化载荷（传输层） | 保证 | `crates/abi/tests/error_format.rs::plugin_desc_error_format_uses_the_size_gate`、`crates/abi/tests/error_format.rs::host_error_format_uses_the_size_gate`、`crates/abi/tests/error_format.rs::host_field_at_checks_size_before_reading`、`crates/abi/src/sdk.rs::structured_error_is_used_when_the_host_supports_it`、`crates/abi/src/sdk.rs::structured_error_falls_back_to_text_when_it_does_not_fit`、`crates/abi/src/sdk.rs::text_only_host_gets_plain_text`、`crates/abi/src/sdk.rs::structured_host_errors_are_decoded`、`crates/abi/src/sdk.rs::text_host_errors_survive_without_capability`、`crates/abi/src/sdk.rs::text_host_errors_use_unknown_code_and_category`、`crates/abi/src/sdk.rs::malformed_inline_response_is_not_a_success`、`crates/abi/src/sdk.rs::non_object_inline_response_is_not_a_success`、`crates/abi/src/sdk.rs::non_string_error_field_is_not_a_success`、`crates/abi/src/sdk.rs::non_object_callback_payload_is_not_a_success`、`crates/abi/src/sdk.rs::pending_inline_response_keeps_the_continuation`、`crates/host/src/lib.rs::malformed_host_arguments_are_protocol_errors_not_null`、`crates/abi/src/sdk.rs::too_small_retries_only_the_read_path`、`crates/host/src/lib.rs::host_too_small_retries_reads_but_not_writes`、`crates/host/src/lib.rs::stream_host_errors_are_structured_for_capable_plugins`、`crates/abi/src/sdk.rs::require_detailed_distinguishes_codes_with_the_same_message`、`crates/abi/src/sdk.rs::set_detailed_distinguishes_codes_with_the_same_message`、`crates/abi/src/sdk.rs::over_cap_host_output_is_bounded_and_readable`、`crates/abi/src/sdk.rs::twenty_k_message_is_truncated_to_the_incoming_cap`、`crates/host/src/lib.rs::call_op_decodes_plugin_errors_only_with_capability`、`crates/host/src/lib.rs::host_failures_are_structured_only_for_capable_plugins`、`crates/host/src/lib.rs::call_plugin_detailed_distinguishes_codes_with_the_same_message`、`crates/host/src/lib.rs::oversized_plugin_output_is_bounded_and_readable`、`crates/host/src/lib.rs::apply_error_output_is_bounded_and_readable`、`crates/host/src/lib.rs::twenty_k_plugin_message_is_truncated_to_the_incoming_cap`、`crates/host/tests/sdk_apply_failure.rs::failed_apply_releases_sdk_callbacks_registered_before_the_error`、`crates/host/tests/sdk_callback_error.rs::structured_callback_error_is_readable_through_apply_via`、`crates/host/tests/apply_error_format.rs::loaded_plugin_apply_carries_error_format`、`crates/host/tests/abi_matrix.rs::a_handwritten_v1_shaped_plugin_loads_and_works_on_a_v2_host`、`crates/host/tests/legacy_error.rs::legacy_plugin_error_is_text_and_unknown_not_not_executed`、`crates/host/tests/sdk_detailed_timeout.rs::call_iface_detailed_receives_timeout_payload_from_the_real_host`、`crates/abi/src/sdk.rs::detailed_continuation_preserves_error_siblings`、`crates/abi/src/sdk.rs::text_continuation_still_reads_only_the_error_string`、`crates/abi/src/sdk.rs::chinese_message_truncates_on_a_char_boundary_with_marker`、`crates/abi/src/sdk.rs::tiny_buffer_is_bounded_and_not_a_nul_string`、`crates/host/src/lib.rs::chinese_fail_message_truncates_on_a_char_boundary`、`crates/host/src/lib.rs::tiny_host_buffer_is_bounded_and_not_a_nul_string`、`crates/abi/tests/copy_on_attach_oversized.rs::oversized_declared_size_is_clamped_to_the_actual_copy`、`crates/host/src/lib.rs::pending_settle_preserves_a_structured_host_failure`、`crates/abi/src/sdk.rs::dispatch_settles_text_continuation_on_malformed_inline_json`、`crates/abi/src/sdk.rs::dispatch_settles_detailed_continuation_on_malformed_inline_json`、`crates/abi/src/sdk.rs::next_detailed_delivers_a_structured_error_payload`、`crates/host/tests/sdk_provider_error_detailed.rs::provider_business_error_reaches_detailed_callback_with_code`、`crates/host/tests/sdk_iface_unload_detailed.rs::sdk_detailed_iface_receives_lifecycle_error_on_provider_unload`、`crates/host/tests/abi_iface_unload_abort.rs::iface_unload_abort_uses_iface_kind_and_lifecycle_cause` | in-process 两向 + 真实 cdylib apply + 详细入口（同消息异 code 可区分）；`args[0].error` 保留字符串并按能力位带结构化兄弟；旧插件错误按 `unknown`；异步超时 `HOST_TIMEOUT/timeout/unknown`；卸载/取消按 `host_lifecycle|host_cancel` 结算；超限按 UTF-8 截断 |
| 配置 schema 在装载前可见（插件自述，不据此验证） | 保证 | `crates/abi/tests/schema.rs::invalid_schema_is_rejected_by_the_builder`、`crates/abi/tests/schema.rs::oversized_schema_is_rejected_by_the_builder`、`crates/abi/tests/schema.rs::undeclared_schema_is_the_canonical_empty_blob`、`crates/abi/tests/schema.rs::schema_is_written_to_the_frozen_plugin_desc_prefix`、`crates/macros/src/plugin.rs::schema_must_be_a_json_object`、`crates/sdk/tests/plugin_macro.rs::plugin_macro_emits_inject_provision_and_volatile`、`crates/host/src/lib.rs::schema_over_the_display_cap_is_reported_not_parsed`、`crates/host/tests/loader_capabilities.rs::abi_plugin_can_declare_provision_through_the_build_path`、`crates/host/tests/apply_error_format.rs::loaded_plugin_apply_carries_error_format`、`crates/host/tests/probe_capabilities.rs::probe_capabilities_reads_provision_without_mounting`、`crates/cli/tests/inspect.rs::inspect_lists_capabilities_before_any_mount` | `Capabilities::schema` / CLI `inspect` 只展示；宿主不按 schema 验证配置，插件自己的 validate/apply 才裁决。非法/超限 schema 在入口或展示面留下 `schema_error`，不越过 C ABI |
| Pending 诊断给出精确缺失键与原因 | 保证 | `crates/core/tests/pending_diagnostics.rs::missing_inject_reports_missing_key`、`crates/core/tests/pending_diagnostics.rs::version_mismatch_reports_related_provider_key`、`crates/core/tests/pending_diagnostics.rs::unready_provider_is_distinguished_from_missing`、`crates/core/tests/pending_diagnostics.rs::provider_in_another_realm_is_reported_as_realm_mismatch`、`crates/core/tests/pending_diagnostics.rs::provider_that_has_not_applied_is_missing_not_not_ready`、`crates/core/tests/pending_diagnostics.rs::multiple_missing_keys_are_sorted_and_reported_individually`、`crates/core/tests/runtime_cycles.rs::runtime_service_cycle_is_rejected_before_it_can_deadlock`、`crates/host/tests/loader_cycles.rs::dependency_cycle_is_reported_with_its_path`、`crates/cli/tests/status.rs::status_reports_the_precise_missing_inject_after_mounting`、`crates/cli/tests/status.rs::status_reports_declaration_cycle_paths`、`crates/cli/tests/status.rs::status_reports_failed_rows_under_no_strict`、`crates/cli/tests/status.rs::status_lists_disabled_rows_without_failing` | 五种原因：`missing` / `version_mismatch` / `provider_not_ready` / `realm_mismatch` / `cycle`。`related` 是全局注册表候选；提供者尚未 apply/provide 时仍报 `missing`；声明级环由 `report.cycles` 解释，行内 reason 可能仍是 `missing`；运行时 `cycle` 标记可能保留到 fiber 卸载（卸载等待图需要），不据此单独推断环仍存在。`status` 会短暂挂载并关闭，同时列出 `disabled` / `failed`，失败行退出码非 0 |
| 能力清单来源标注为插件自述 | 声明 | — | schema/provision/volatile 来自插件自述；inject 视图额外合并配置行；两者都不是审批或授权。见非目标 #6 |
| 看门狗超时撤销派发记录（四条路径） | 保证 | `crates/host/tests/abi_m2_timeout.rs::dispatch_policy_timeout_settles_a_never_fulfilled_continuation_as_timeout`、`crates/host/tests/abi_outbound_timeout.rs::iface_call_times_out_and_unregisters_dispatch`、`crates/host/tests/abi_outbound_dispatch_timeout.rs::outbound_dispatch_times_out_and_unregisters_dispatch`、`crates/host/tests/abi_next_timeout.rs::ctx_next_times_out_and_delivers_an_error_to_on_done` | 入站 / iface / dispatch / waterfall 各一条；iface / dispatch / next 已断言 `code=HOST_TIMEOUT`、`category=timeout`、`certainty=unknown`。事件路径没有返回协议，仍是已知例外 |
| 插件 panic 被截在 ABI 边界内 | 保证 | `crates/abi/src/sdk.rs::plugin_panics_become_status_err_at_the_abi_boundary`、`crates/abi/src/sdk.rs::desc_slot_catches_builder_panics_instead_of_crossing_the_c_abi`、`crates/abi/tests/invalid_schema_entry.rs::invalid_schema_entry_returns_null_instead_of_aborting` | apply / 回调 / 清理三条路径都转成 `STATUS_ERR`；入口构造期（含非法 schema）的 panic 降级为空描述表，由宿主报装载错误 |
| 重复成功/失败/丢弃 Future/卸载/waterfall/stream/重载后资源计数回到基线 | 保证 | `crates/host/tests/resource_baseline.rs::repeated_cycles_return_to_baseline` | 8 轮 × 七条路径；实例 / 派发 / 挂起 / 派发帧 / 流 / effect |
| 多轮暂存后台账回到基线 | 保证 | `crates/host/tests/staged.rs::staged_ledger_returns_to_baseline_across_many_rounds` | 64 轮；暂存 +1 / 认领归零 / drop 删除 |
| 冻结 ABI v1 真 cdylib × 新宿主 | 保证 | `crates/host/tests/frozen_plugin.rs::a_frozen_abi_v1_cdylib_loads_and_calls` | 真 72B v1 结构体（无 `provision`）；volatile、probe/load/mount/回调/dispose；版本与尺寸拒绝；哈希按 LF 字节冻结，`.gitattributes` 固定 `tests/fixtures/**` |
| 历史发布版 × 新宿主 | 待验证 | — | 当前封存对象是三个 `publish = false` 示例插件，不是可发布 crate；仍需正式首发产物归档（二进制 / 源码版本 / 工具链 / SHA256）与后续宿主版本。同版本宿主加载只证明产物可用，不等于跨版本兼容 |
| 崩溃后任务恢复 | 非目标 | — | 无持久化账本；见 ADR 0003 |
| 自动重试安全 / exactly-once | 非目标 | — | 当前不提供任何重试判定输入；见 ADR 0003 §4 |
| 不可信代码隔离 | 非目标 | — | 需要 OS / Wasm 执行边界，不是载体能提供的 |
| 无感热插拔 | 非目标 | — | 只提供原地换值，不保证状态迁移 |
| 远程调用协议 | 非目标 | — | 本地 ABI 不等于远程协议；见 ADR 0001 |
| 授权与审批 | 非目标 | — | manifest 是插件自述，不是审批结果 |

## 用词规则

文档里的每个断言只能落在下面四类之一：

- **保证**：有测试兜底；测试失败即视为契约破坏。
- **声明**：插件或文档的自述，SDK 不验证（例如副作用声明、能力要求）。
- **待验证**：有实现、示例或明确的产物计划，但没有可重复的验收
  （例如历史发布版尚未产生构建产物）。
- **非目标**：当前不做，也不作为默认预期。

“保证”一词不得用于没有测试的断言。需要表达“我们提供了信息”时，写“提供”或“声明”，
不要写“保证安全”“保证不重复执行”。

## 提案四问

每个新提案必须先回答：

1. 它是机制、契约、策略、领域词汇，还是观测证据？
2. 谁负责执行，并证明它成立？
3. 哪个真实消费者需要它？
4. 应归哪个 crate？能否通过现有扩展点实现？

第 2 问的答案如果是“SDK 提供信息”，文档里就不能写成“SDK 保证”。

## 相关文档

- `docs/support.md`：生命周期支持矩阵
- `docs/release.md`：发布门禁与待确认项
- `docs/adr/0001-second-carrier.md`：第二载体（不实现）
- `docs/adr/0002-broker.md`：broker（不做）
- `docs/adr/0003-result-and-error-contract.md`：结果与错误契约
- `docs/adr/0004-abi-error-evolution.md`：错误的双向传输与兼容演进
- `docs/api-inventory.md`：公开 API / 状态码 / 载荷清单（生成 + 漂移检查）
- `tests/fixtures/abi-v1/README.md`：冻结 ABI v1 夹具的 provenance 与冻结纪律
- `tests/fixtures/abi-error/README.md`：跟随当前布局的结构化错误夹具说明
- `tests/fixtures/abi-err-v1/README.md`：独立旧 ABI 错误夹具说明
- `docs/roadmap.md`：分阶段执行计划
