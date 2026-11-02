@echo off
rem 项目专用 cargo 包装器（M0）。
rem
rem 背景：本机 ~/.cargo/config.toml 把 crates-io 替换成了不可达的 ustc 镜像；
rem cargo 不允许"再定义一个指向官方 registry 的源"，所以只能给本项目单独一个
rem CARGO_HOME —— 它不继承 ~/.cargo/config.toml，crates-io 即官方 sparse 源，
rem 再配合 .cargo/config.toml 里的本地代理设置。不改动用户全局配置。
rem
rem 用法：
rem   cargo.cmd test --workspace --offline     # 日常 TDD（依赖已缓存）
rem   cargo.cmd fetch                          # 首次/新增依赖时联网拉取
rem   cargo.cmd run -p demo                    # 运行 demo
setlocal
set "CARGO_HOME=C:\cordis-rs-cargo-home"
if not exist "%CARGO_HOME%" mkdir "%CARGO_HOME%"
rem 本机路径含中文，GNU ld 无法处理；本机便利包装把 target 指到 ASCII 目录。
rem 仓库里的 .cargo/config.toml 不再保存这个绝对路径（CI/其他机器可移植）。
if not defined CARGO_TARGET_DIR set "CARGO_TARGET_DIR=C:\cordis-rs-target"
rem 本机代理只放在这个本地包装器里，不进仓库配置。
if not defined CARGO_HTTP_PROXY set "CARGO_HTTP_PROXY=http://127.0.0.1:7897"
rem windows-gnu 的 proc-macro lib-test 会固定传 -C prefer-dynamic；本机没有独立
rem mingw，dlltool 会失败。静态链接 std 绕开它（仅本机包装器）。
if not defined RUSTFLAGS set "RUSTFLAGS=-C prefer-dynamic=no"
rem 注意：必须写 cargo.exe —— 若只写 `cargo`，cmd 会优先命中本目录的 cargo.cmd 自身，
rem 造成批处理无限递归（Maximum setlocal recursion level reached）。
rem
rem windows-gnu 工具链 + proc-macro crate：rustc 生成 proc-macro 的导入库时按
rem PATH 找 dlltool.exe，而本机没有独立的 mingw（全靠工具链自带的 self-contained
rem binutils）。把该目录补进 PATH——幂等、只影响本次调用，不动用户全局配置。
rem `+toolchain` 时 sysroot 必须跟着工具链走；否则会把 stable 的 MinGW
rem self-contained 路径注入旧工具链，链接时找不到它自己的运行库。
set "CORDIS_TOOLCHAIN="
set "CORDIS_ARG=%~1"
if "%CORDIS_ARG:~0,1%"=="+" set "CORDIS_TOOLCHAIN=%CORDIS_ARG:~1%"
if defined CORDIS_TOOLCHAIN (
  for /f "usebackq delims=" %%i in (`rustup run %CORDIS_TOOLCHAIN% rustc --print sysroot 2^>nul`) do set "CORDIS_SYSROOT=%%i"
) else (
  for /f "usebackq delims=" %%i in (`rustc.exe --print sysroot 2^>nul`) do set "CORDIS_SYSROOT=%%i"
)
if exist "%CORDIS_SYSROOT%\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained\dlltool.exe" set "PATH=%CORDIS_SYSROOT%\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained;%PATH%"
cargo.exe %*
set "CORDIS_EXIT_CODE=%ERRORLEVEL%"
endlocal & exit /b %CORDIS_EXIT_CODE%
