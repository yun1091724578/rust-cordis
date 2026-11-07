param(
    [string]$Seal = "",
    [string]$HostRef = "HEAD",
    [string]$Worktree = "C:\cordis-forward-host",
    [string]$Target = "C:\cordis-forward-target",
    [string]$BumpVersion = ""
)

$ErrorActionPreference = "Stop"
$project = Resolve-Path (Join-Path $PSScriptRoot "..")
$cargo = Join-Path $project "cargo.cmd"

if (-not $Seal) {
    $Seal = Join-Path $project "dist\cordis-0.1.0-rc2"
}
$Seal = (Resolve-Path $Seal).Path
$plugins = Join-Path $Seal "plugins"
$smoke = Join-Path $Seal "smoke\cordis.yml"
if (-not (Test-Path $plugins) -or -not (Test-Path $smoke)) {
    throw "seal is missing plugins/ or smoke/cordis.yml: $Seal"
}

if (Test-Path $Worktree) {
    git -C $project worktree remove --force $Worktree | Out-Null
}
git -C $project worktree add --detach $Worktree $HostRef | Out-Null

try {
    if ($BumpVersion) {
        $manifest = Join-Path $Worktree "Cargo.toml"
        $text = Get-Content -Raw $manifest
        $text = $text -replace 'version = "0\.1\.0"', "version = `"$BumpVersion`""
        Set-Content -NoNewline -Encoding UTF8 $manifest $text
    }

    $env:CARGO_TARGET_DIR = $Target
    & $cargo build -p cordis-cli
    if ($LASTEXITCODE -ne 0) { throw "host build failed" }

    $exe = Join-Path $Target "debug\cordis.exe"
    if (-not (Test-Path $exe)) { throw "missing host executable: $exe" }

    $config = Join-Path $Worktree "forward-greeting.yml"
    @(
        "- id: greeting",
        "  name: greeting"
    ) | Set-Content -Encoding ASCII $config

    Write-Output "forward compatibility drill"
    Write-Output "host ref=$HostRef bump=$BumpVersion seal=$Seal"
    & $exe inspect $smoke --plugin-dir $plugins
    if ($LASTEXITCODE -ne 0) { throw "inspect failed" }
    & $exe status $config --plugin-dir $plugins
    if ($LASTEXITCODE -ne 0) { throw "status/mount failed" }
    Write-Output "forward drill passed: inspect + mount + cleanup"
}
finally {
    git -C $project worktree remove --force $Worktree 2>$null
}
