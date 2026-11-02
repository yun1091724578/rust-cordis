# A2 baseline check: delegate to the portable `cargo xtask demo` entry.
#
# `xtask demo` builds the plugins, runs the demo, normalizes line endings and
# compares against expected.txt. It also retries the known HMR log-order jitter.
#
# Usage (from repo root):
#   powershell -ExecutionPolicy Bypass -File examples\demo\compare.ps1
#
# This file intentionally contains ASCII only: PowerShell 5.1 parses BOM-less
# .ps1 files as ANSI, so non-ASCII text would be mojibake'd.

$ErrorActionPreference = 'Stop'

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Split-Path -Parent (Split-Path -Parent $here)
Set-Location $repo

& (Join-Path $repo 'cargo.cmd') xtask demo
exit $LASTEXITCODE
