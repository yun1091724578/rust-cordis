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
$sealFile = Join-Path $Seal "seal.json"
if (-not (Test-Path $plugins) -or -not (Test-Path $smoke) -or -not (Test-Path $sealFile)) {
    throw "seal is missing plugins/, smoke/cordis.yml or seal.json: $Seal"
}
$sealManifest = Get-Content -Raw $sealFile | ConvertFrom-Json
try {
    $sealVersion = [version]$sealManifest.workspace_version
} catch {
    throw "seal.json has an invalid workspace_version: $($sealManifest.workspace_version)"
}

if (Test-Path $Worktree) {
    git -C $project worktree remove --force $Worktree | Out-Null
}
git -C $project worktree add --detach $Worktree $HostRef | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "git worktree add failed for HostRef=$HostRef"
}
$hostCommit = (git -C $project rev-parse "$HostRef^{commit}").Trim()
if ($LASTEXITCODE -ne 0 -or -not $hostCommit) {
    throw "could not resolve HostRef=$HostRef to a commit"
}

try {
    if ($BumpVersion) {
        $manifest = Join-Path $Worktree "Cargo.toml"
        $text = Get-Content -Raw $manifest
        $updated = $text -replace '(?m)^version = "[^"]+"', "version = `"$BumpVersion`""
        if ($updated -eq $text) {
            throw "BumpVersion=$BumpVersion did not match the workspace version in $manifest"
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
    if (Test-Path $allowList) {
        if ($helpText -notmatch "--source-policy") {
            throw "seal has allow-list.json but host $HostRef does not support --source-policy"
        }
        $admissionArgs = @("--source-policy", $allowList)
        $admissionMode = "enforced"
    } elseif ($helpText -match "--allow-unverified") {
        $admissionArgs = @("--allow-unverified")
        $admissionMode = "dev-unverified"
    } else {
        $admissionArgs = @()
        $admissionMode = "legacy-no-policy"
    }

    $hostManifest = Join-Path $Worktree "Cargo.toml"
    $hostText = Get-Content -Raw $hostManifest
    if ($hostText -notmatch '(?m)^version = "([^"]+)"') {
        throw "could not read host workspace version from $hostManifest"
    }
    $hostVersion = [version]$Matches[1]
    if ($hostVersion -le $sealVersion) {
        throw "host $hostVersion is not later than seal $sealVersion; this is not a forward-compat drill"
    }

    $config = Join-Path $Worktree "forward-greeting.yml"
    @(
        "- id: greeting",
        "  name: greeting"
    ) | Set-Content -Encoding ASCII $config

    Write-Output "forward compatibility drill"
    Write-Output "host ref=$HostRef commit=$hostCommit version=$hostVersion bump=$BumpVersion seal=$Seal seal_version=$sealVersion admission=$admissionMode"
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
