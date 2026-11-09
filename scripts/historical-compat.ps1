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
if ($LASTEXITCODE -ne 0) {
    throw "git worktree add failed for HostRef=$HostRef"
}

try {
    if ($BumpVersion) {
        $manifest = Join-Path $Worktree "Cargo.toml"
        $text = Get-Content -Raw $manifest
        $updated = $text -replace 'version = "0\.1\.0"', "version = `"$BumpVersion`""
        if ($updated -eq $text) {
            throw "BumpVersion=$BumpVersion did not match version 0.1.0 in $manifest"
        }
        Set-Content -NoNewline -Encoding UTF8 $manifest $updated
    }

    $env:CARGO_TARGET_DIR = $Target
    Push-Location $Worktree
    try {
        & $cargo build -p cordis-cli
        if ($LASTEXITCODE -ne 0) { throw "host build failed" }
    }
    finally {
        Pop-Location
    }

    $exe = Join-Path $Target "debug\cordis.exe"
    if (-not (Test-Path $exe)) { throw "missing host executable: $exe" }

    # Old hosts do not know this flag; new hosts default-deny without it.
    $helpText = (& $exe --help 2>&1 | Out-String)
    $allowList = Join-Path $Seal "allow-list.json"
    $admissionArgs = if ((Test-Path $allowList) -and ($helpText -match "--source-policy")) {
        @("--source-policy", $allowList)
    } elseif ($helpText -match "--allow-unverified") {
        @("--allow-unverified")
    } else {
        @()
    }

    $config = Join-Path $Worktree "forward-greeting.yml"
    @(
        "- id: greeting",
        "  name: greeting"
    ) | Set-Content -Encoding ASCII $config

    Write-Output "forward compatibility drill"
    Write-Output "host ref=$HostRef bump=$BumpVersion seal=$Seal"
    $inspectArgs = @("inspect", $smoke, "--plugin-dir", $plugins)
    if ($admissionArgs.Count -gt 0) { $inspectArgs += $admissionArgs }
    & $exe @inspectArgs
    if ($LASTEXITCODE -ne 0) { throw "inspect failed" }

    $statusArgs = @("status", $config, "--plugin-dir", $plugins)
    if ($admissionArgs.Count -gt 0) { $statusArgs += $admissionArgs }
    & $exe @statusArgs
    if ($LASTEXITCODE -ne 0) { throw "status/mount failed" }
    Write-Output "forward drill passed: inspect + mount + cleanup"
}
finally {
    git -C $project worktree remove --force $Worktree 2>$null
}
