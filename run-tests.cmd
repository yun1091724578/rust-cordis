@echo off
rem 便捷入口：等价于 `cargo.cmd test --workspace <额外参数>`。
rem 例：
rem   run-tests.cmd --offline
rem   run-tests.cmd --offline -p cordis-host
rem   run-tests.cmd --offline -- --nocapture
rem   run-tests.cmd miri        （可选，非 A1 必需：需要 nightly + miri 组件）
if /i "%~1"=="miri" (
    echo [run-tests] 可选 miri 子命令：需要 nightly 工具链与 miri 组件，非 A1 必需。
    call "%~dp0cargo.cmd" +nightly miri test -p cordis-abi %~2 %~3 %~4
    exit /b %ERRORLEVEL%
)
call "%~dp0cargo.cmd" test --workspace %*
