# R3 评审：维护者视角（ABI 兼容 / 可实施性 / 测试充分性）

对象：`docs/rsi-sdk-roadmap.html` v1。只读核实，行号以当前工作区为准。

## WP1 trace sink

### [P1] 派发出口是单点，但埋点应放在 `handlers()` 而非 `dispatch_cancellable`
- 五个 ctx 动词（context.rs:698-753）全部走 `Store::dispatch` → `dispatch_cancellable`（store.rs:1269-1320）→ `event::dispatch`（event.rs:159-235，四个 match 臂各自展开 handler 循环）。
- 但 `handlers()`（store.rs:1218-1248）把 `hook.ctx` 丢掉，只返回 `Vec<Handler>`；到 `event::dispatch` 时已无 callee 身份。
- 旁路两处：`internal/config` waterfall（store.rs:1498-1504）与 `internal/update` 行内钩子拼接（lib.rs:95-105），均为 `internal/*`，可排除。
- **结论：埋点 1 处**——在 `handlers()` 循环里把每个 `hook.handler` 包成计时闭包（捕获 `hook_ctx.fiber_opt()` 的 uid/`runtime.name` 与 `from.fiber_opt()`）。四种模式（含 parallel 的 JoinSet、waterfall 的递归）自动覆盖，不必改 event.rs。注意：waterfall 的 elapsed 含下游耗时，需在文档写明；`internal/*` 事件须跳过，否则 `internal/dispatch` 自身被记录。
- caller：`from` 是派发点 ctx，root ctx 的 `fiber_opt()` 返回 UID_ROOT fiber 而非 None（store.rs:1341-1346 已依赖此事实），`caller_uid` 不需要 Option。

### [P1] iface 路径：埋点 1 处即可，不需要动 `call_inbound`/`settle`
- T1→T2 与 T2→T2 都收敛到 `Context::call_iface`（context.rs:468-486）：host 的 `ctx_call_iface` 也调它（lib.rs:3377）。`PluginIface::call`（lib.rs:3161-3207）内部已 `await` `settle`，所以在 `iface.call(..).await` 两侧计时即覆盖 PENDING 等待期；Cancelled/TimedOut 以 `code=host_cancel/HOST_TIMEOUT` 的 Err 返回（lib.rs:1771-1788），可映射成 outcome。
- callee 身份：`iface_target` 走 `require` → `store.resolve`（store.rs:685-742）返回 `impl_.get()` 丢掉了 `Impl.fiber`（store.rs:56）。需补一个 `resolve_impl` 返回 `Arc<Impl>`，再取 `provider_uid()`（store.rs:86）。约 20 行。
- **盲区**：`ctx.iface::<dyn T>()` 的 typed 直调（context.rs:395）不经 `call_iface`，T1→T1 typed 调用不可追踪。WP4 的 evaluator 必须用 `call_iface` 调 solver，并在 ADR 写明。
- `call_iface_stream`（context.rs:495）需单独埋点或明确排除。

### [P2] Store 放 sink 的位置现成
`log_sink: Mutex<LogSink>` + `set_log_sink`（store.rs:413, 462）即模板。零开销要求建议用 `OnceLock<Arc<dyn TraceSink>>` 或 `AtomicBool` 快速判空，不要每次派发取 Mutex。

## WP2 report mounts

### [P0] HMR 指纹是 `(mtime, len)`，不是内容哈希，不能当 `source_digest`
`fingerprint(path)`（loader.rs:299-308）读 metadata；同内容不同 mtime 会变，"同一文件两次启动 digest 相同"的验收必不通过。应改用已有 `source::sha256_file`（source.rs:355）。**不要**给 `LoadedFile`（loader.rs:3194，api-inventory 受控项）加字段——外部 `PluginSource` 实现用字面量构造，违反 ADR 0006；改为 loader 拿到 `LoadedFile.staged` 后自己算 sha256 存进私有 `Mounted`。T1 内建（假源 `fingerprint: None`）填 `"builtin"` 可行。

### [P2] `Mounted` 加字段自然；uid 可取
`Mounted`（loader.rs:257-297）私有，含 `fiber: Weak<Fiber>`、`config`、`volatile`；加 `source_digest/config_digest/epoch` 不影响公开面。没有 per-label epoch，需新增计数（现有 `revision` 是全局单调）。`report()`（loader.rs:1727-1775）已有逐行 `pending` 数组，`publish_report` 用 `ctx.set`（1814），开销 O(行数) 构 JSON，几十行可接受。

## WP2b interface lock

### [P2] 现状比方案假设更好；注意 inject 混合了行级声明
`inspect --json`（cli/src/lib.rs:267-288）已输出 `label/plugin/version/inject{required,optional}/provision/volatile/schema/schema_error/provenance(sha256)`。`inject.required` = 插件声明 + YAML 行级 `required`（lib.rs:196），锁比对时要么接受"行+插件"口径，要么只取 provision/schema。`validate` 退出码 0/3（main.rs:109-121），新 flag 走 `parse_config_args`（main.rs:286）需加 allow 位，与现有 `--source-policy/--allow-unverified` 风格一致。验收命令用 `cargo test -p cordis-cli` 包名正确；但 WP1/WP2 写的 `-p cordis-core/-p cordis-host` 错，实际是 `rust-cordis-core/rust-cordis-host`。

## WP4 e2e

### [P0] `cordis status --json` 不能观测运行中的宿主
`status_with_source`（cli/src/lib.rs:740-772）**新起 App → 挂载 → 取 report → 关闭**，与 `cordis run` 进程无关；`mounts[solver].epoch` 永远是 1，`failed` 也看不到 HMR 失败。evolve.py 的整条观测链不成立。且 `cordis run` 硬编码 `hmr: None`（lib.rs:684），YAML 条目里无处开 HMR（`HmrConfig` 是 `LoaderConfig` 字段，loader.rs:84/198）。
两条路：(a) 示例 `host/` 做成自定义 Rust 宿主（同 `examples/harness-spike/src/main.rs:189-220` 形态），自行设 `hmr.intervalMs`，并在 evaluator 里读 `loader.report` 写 `status.json`；(b) 给 `cordis run` 加 `--hmr <ms>`/`--status-out <path>`。推荐 (a)，不扩 CLI 面。

### [P2] Windows 文件锁风险已不存在（G6 可销）
`stage_plugin`（host/src/lib.rs:5068-5096）把源复制为 `<stem>.s<pid>-<n>.r<rev>.<ext>` 再 `LoadLibraryW`（dylib.rs:39），源文件不被锁，原地覆盖可行；HMR 轮询 `stale_labels` + `reload_one`（loader.rs:4510-4516），默认 500ms（loader.rs:214）。evolve 覆盖文件时需确保 mtime 变化（Python 用 `copyfile` 而非 `copy2`）。

### [P1] "加载即 panic"须限定为 apply/入口构造期 panic
这两类被 SDK 截在 ABI 内（support-contract.md:85），走 `SwapOutcome::RolledBack`（loader.rs:4533）并 `record_reload_failure`（4891）写入 `report.failed`。DllMain/静态初始化崩溃会带走进程，不在回滚语义内。另：无 `--source-policy` 时默认拒绝加载（main.rs:350），示例需 `set_allow_unverified(true)` 或为每个变体生成 sidecar manifest。

### [P2] CI 接入
ci.yml 跑 `cargo test --workspace` + `xtask consumer/demo/e2e`，三平台矩阵。新 example 加进 `Cargo.toml` members 后 `--workspace` 自动覆盖其 `tests/e2e.rs`；若保留 evolve.py，CI 需保证 python3（runner 自带，但增加一个非 Rust 依赖）。建议 e2e.rs 用 Rust 直接驱动，evolve.py 作为文档样例不进 CI。

## 兼容承诺

### [P1] support-contract 没有"实验性"段；新增项须进 api-inventory
`docs/support-contract.md:97-108` 只有 保证/声明/待验证/非目标 四类，V7 的"实验性段"不存在。建议新增 pub 项归 **待验证**（有实现无长期承诺），或在 ADR 0009 另立"实验面"并修改用词规则。`set_trace_sink/all_fibers`、`LoaderState::report` 新字段、CLI 新 flag 均非 ABI（`baseline.txt` 不变，V2 成立），但 `api-inventory.md:81` 受控子集含 core/host 入口，新 pub fn 必须同步登记（ADR 0006 §2）。注意 ci.yml 并未跑 `xtask inventory --check`，只有本地 `xtask ci` 跑，门禁靠自觉。`loader.report` 载荷形状未入 inventory"载荷格式"表，加 `mounts` 不触发门禁，但应在 ADR 写明只增不改。

## 工期

| WP | 方案 | 评估 | 依据 |
|---|---|---|---|
| WP0 | 1 | 1 | — |
| WP1 | 6 | 5 | 埋点 2 处（handlers + call_iface），不碰 host settle；+resolve_impl 小改 |
| WP2 | 3 | 3 | sha256 替代指纹不增工；epoch 计数新增 |
| WP2b | 2 | 2 | inspect 已齐 |
| WP4 | 6 | 8 | 自定义宿主 + status.json 导出 + Rust 驱动 e2e；去掉 Python |
| 缓冲 | 2 | 2 | 原 Windows 文件锁风险消失，转给 WP4 重设计 |
| 合计 | 20 | **21** | |

## 总体结论

**可实施性：有条件通过。** ABI 零改动的声明成立；WP1/WP2b 比方案预期更顺。必须修正：
1. WP4 观测链重设计——放弃 `cordis status --json`，示例用自定义宿主导出 `status.json`，`cordis run` 现无 HMR 开关（P0）。
2. WP2 `source_digest` 改用 `sha256_file`，不改 `LoadedFile` 公开结构（P0）。
3. WP1 埋点位置改为 `Store::handlers()` + `Context::call_iface`，补 `resolve_impl`；文档写明 typed 直调盲区与 waterfall 含下游耗时（P1）。
4. V7 用词改"待验证"或 ADR 0009 定义实验面；新 pub 项登记 api-inventory（P1）。
5. 修正包名 `rust-cordis-core/-host`；"加载 panic"限定为 apply/入口期（P2）。

修订后工期 21 人日。
