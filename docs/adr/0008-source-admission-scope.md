# ADR 0008：来源准入的作用域与优先级

- **状态**：已接受（2026-11-17）
- **日期**：2026-11-17
- **依据**：`docs/adr/0005-trust-model-and-plugin-source-boundary.md`（§I5）、
  `docs/adr/0006-public-api-stability.md`、`docs/adr/0007-version-is-diagnostic-only.md`；
  `crates/host/src/source.rs`；`crates/host/src/loader.rs`；
  `crates/host/tests/source_policy.rs`、`crates/host/tests/source_policy_reload.rs`、
  `crates/host/tests/probe_source_admission.rs`、`crates/host/src/loader/source_admission_tests.rs`
- **决定摘要**：准入结论**跟着来源对象走**（`PluginSource::admission`），不放在
  `LoaderConfig`，也不按 App 注册表。三态优先级：显式严格 > 显式开发 > 兼容回落。
  隔离保证只覆盖显式入口；兼容回落与 `ctx_use` 的进程级 seam 仍受全局影响。

---

## 1. 背景：两个全局量让并发的 App 互相污染

本轮之前，准入只有两个进程级量：`ACTIVE_POLICY`（`OnceLock<RwLock<Option<_>>>`）
与 `ALLOW_UNVERIFIED`（`AtomicBool`），读口是 `source::admission()`。它们是
"后写覆盖先写"的全局状态，**没有任何 App 维度**。同一进程里两个 App 策略不同时，
污染有两条确定路径：

**路径 A：dev 放行放宽另一个严格 App。** App-B 以严格策略运行，把
`set_active_policy(Some(policy))` 当作唯一门禁。App-A 只是"用开发模式启动"：
CLI 的 `activate_source_policy` 在 `--allow-unverified` 分支里先
`set_active_policy(None)` 再 `set_allow_unverified(true)`——B 的策略被这一句抹掉。
此后 B 的下一次装载读到 `Ok(None)` ⇒ **跳过校验**：未登记 / 被篡改的 artifact
会真的走到 `stage_plugin` 与 `LoadLibrary`/`dlopen`，对外错误从"不在 allow-list 中"
变成"打不开动态库"——一个未登记的构建被读成坏构建，诊断口径与安全结论同时失真。
A 完全不需要知道 B 存在；"结束开发"时的对称复位还会把 B 的策略永久清空。
（嵌入方在同一进程里直接调 `set_allow_unverified(true)` 是同一路径的更短形态：
只要 B 还没设策略，默认拒绝当场消失。）

**路径 B：严格 App 的策略管到 dev App。** 反方向同样成立：App-B 设置策略后，
App-A 的裸源读的是同一个 cell，于是 A 的本地未登记插件被拒（"不在 allow-list 中"），
而 A 从未要求校验；A 的"开发模式"这一意图只是没被表达，不是被尊重。
两条路径合起来说明一件事：**进程级结论无法表达"这个来源的准入由谁说了算"**。

## 2. 决策：准入结论随来源对象走

新增 `PluginSource::admission() -> SourceAdmission`（带默认实现），结论由**来源对象**
持有；`PolicyPluginSource` 是把结论挂到既有来源上的包装层。两个被否掉的载体：

- **不是 `LoaderConfig` 字段。** `LoaderConfig` 有 12 个公开字段，仓内外三十余处
  字面量构造（`crates/host/tests/*`、`crates/host/src/loader.rs` 的测试辅助、
  `crates/cli/src/lib.rs`）。加字段＝所有字面量构造编译失败，属 ADR 0006 定义的
  0.2 破坏；而且准入是**来源**的属性，不是配置行的属性——同一份配置换了来源对象
  就该换结论。
- **不是按 App 的注册表。** `PluginSource::load(&Entry, &[PathBuf], u64)` 的签名里
  没有 `ctx`：来源本来就不知道自己这次在为哪个 App 装载。要按 App 登记就得改
  `load` 的签名（同样的 0.2 破坏），或者从 thread-local / 任务上下文中猜——后者把
  "谁说了算"变成不可测的隐式状态，比全局 static 更难审计。

包装层两头都躲开：`load` / `plan_entry` 转发时把自己的结论经
`load_admitted` / `plan_entry_admitted` 交给内层，并覆写 `admission()` 让装载器
（`reload_one`、探测）看到同一份结论。

## 3. 三态定义与优先级

| 态 | 构造器 | 解析结论 | 与进程级全局的关系 |
|---|---|---|---|
| 显式严格 | `SourceAdmission::strict(policy)` / `PolicyPluginSource::strict` | 恒为 `Some(policy)` | **忽略**进程级 dev 标志，永不被全局放宽 |
| 显式开发 | `SourceAdmission::dev_unverified()` / `PolicyPluginSource::dev_unverified` | 恒为 `None`（放行未校验） | 只对**自身**放行，**不写**进程级标志 |
| 兼容回落 | `SourceAdmission::fallback()`（也是 `PluginSource::admission` 的缺省值） | 逐字等于旧 `source::admission()` | 仍受全局影响：策略优先，其次是显式开发模式，两者都没有 ⇒ 拒绝 |

优先级就是严格 > 开发 > 回落，且只有 `Fallback` 会读全局量：`Strict` /
`DevUnverified` 的答案是常量，所以进程级开关**放宽不了**它们。
三个值不能合并成一个开关：严格是"我不接受别人的便利开关"，开发是"我只放行自己"，
回落是"我沿用旧语义、别改我行为"——它们的安全含义各不相同。

## 4. 隔离边界（本决策不承诺什么）

- **只覆盖显式入口。** 隔离保证的前提是来源对象被显式包成 `PolicyPluginSource`
  或自己覆写了 `admission()`；裸 `DiskPluginSource`（以及所有没换来源的宿主）
  走的就是 `fallback`。
- **兼容回落与 `ctx_use` 的进程级 seam 仍受全局影响。** `fallback` 按定义读全局；
  `ctx_use` 挂子组件走的是进程级 `set_plugin_source` seam，那条路径既拿不到
  `SourceAdmission` 参数，也看不到所在 App 的来源结论。
- **显式传参给 `load_admitted` / `plan_entry_admitted` 是调用方选择，不是远端输入。**
  传入的结论覆盖来源自身的声明；插件拿不到来源对象，所以不存在"从插件侧构造
  显式降级"的入口。
- **嵌套包装以最外层声明为准。** 外层转发自己的结论，内层不得用自己的覆盖它。
- **包装只对覆写了 `*_admitted` 的来源有效。** 只实现 `load` / `plan_entry`
  且自己读进程级全局的第三方来源，包装层收紧不了它——转发进去的结论会被它忽略，
  `admission()` 也只是多摆一个没人看的声明。

## 5. 生命周期责任

策略由**嵌入方**创建（`PluginSourcePolicy::new / from_json / from_file`），由
**来源对象**持有（`Arc`）。于是生命周期就是普通所有权：

- **替换**＝构造一个新来源对象并重新 `load_with_source`，不是"改全局再改回来"；
- **关闭**＝drop 来源对象，不是"记得把全局复位"。

旧写法的复位义务（`set_active_policy(None)` + `set_allow_unverified(false)`）
因此从"使用方纪律"变成"不需要存在"。没有 drop 顺序要求的全局量，也就没有
"忘了复位"这一类缺陷。

## 6. 与既有 ADR 的关系

- **ADR 0006（公开 API 稳定）**：本决策是**纯加法**——新增类型 `SourceAdmission`、
  包装 `PolicyPluginSource`、trait 三个带默认实现的方法；`LoaderConfig` 字段、
  `DiskPluginSource` 的单位结构、既有字面量构造形状全都不变。0.1.x 不因此需要
  迁移。
- **ADR 0005 §I5（策略按宿主实例而不是全局）**：`DispatchPolicy` 已明确拒绝
  "只有一个进程级 global static"，本决策与之**同构**：准入也从"全局 static +
  使用方纪律"变成"跟着对象走"。
- **ADR 0007（版本只做诊断）**：准入结论不改变"声明不是 verified"。允许装载
  不等于插件行为可信，`plan_entry` 读到的 `provides`/`requires` 仍是声明，
  不是运行时事实。

## 7. 后果

- **`plan_entry` 的默认拒绝是公开可观察的行为变化。** 没有来源策略、也没开开发
  模式时，凡是要读 sidecar 的预检，现在以「未配置插件来源策略」**失败**，且**先于**
  manifest 结论（清单无效 / 不合法的报错轮不到出现）。旧实现是"跳过校验、只读清单"。
  这修的是同一件事的两个环境给出不同首错的漂移，但它确实会让"从来没配策略、
  只靠 sidecar 做依赖预检"的嵌入选型在升级后第一次失败。
- **CLI 行为不变。** `--source-policy` / `--allow-unverified` 仍写进程级全局；
  CLI 是单 App 进程，`admission_mode()` 仍是它的诊断口径。新增的
  `SourceAdmission::mode()` 报 `enforced` / `dev-unverified` / `fallback`；第三个值
  故意不复用 `admission_mode()` 的 `denied`，因为它说的是"这个来源没有自己的结论"，
  此刻进程级结论是什么由 `resolve()` 回答（CLI 也没有把它接进 `status`/`inspect`）。
- 新语义需要每个会读全局的来源在覆写 `load`/`plan_entry` 时**同时**覆写
  `*_admitted`，否则一次操作里会出现两份快照（`probe` 与 `reload_one` 都踩过）。

## 8. 验收与证据

- `crates/host/tests/source_policy.rs::explicit_strict_source_ignores_the_global_development_flag`
  ——显式严格在全局 dev 打开时仍报准入失败，且不改写全局标志。
- `crates/host/tests/source_policy.rs::explicit_dev_source_does_not_write_the_global_flag`
  ——显式 dev 只放行自己；同一刻的裸源仍默认拒绝。
- `crates/host/tests/source_policy.rs::two_sources_keep_their_own_admission_without_touching_globals`
  ——三方 `Barrier` 下两个来源并发，互不影响、都不碰全局。
- `crates/host/tests/source_policy.rs::bare_disk_source_still_reads_the_process_wide_policy`、
  `::bare_disk_source_plan_entry_still_reads_the_process_wide_policy`——`fallback`
  逐字等于旧语义（`load` 与 `plan_entry` 两侧）。
- `crates/host/tests/source_policy_reload.rs::reload_one_honours_the_sources_own_strict_policy_even_when_global_dev_is_on`
  ——热重载走来源自己的结论，且失败也推进指纹。
- `crates/host/tests/probe_source_admission.rs::probe_uses_one_admission_snapshot_for_verify_and_open`
  ——探测的校验与打开共用一份快照。
- `crates/host/src/loader/source_admission_tests.rs::reload_one_rejects_a_staged_copy_with_different_bytes`
  ——暂存副本在打开前被换掉时按已校验 hash 拒绝。
- 记录与边界：`docs/support-contract.md` 的插件来源准入条目。
