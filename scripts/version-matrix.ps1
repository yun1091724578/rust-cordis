param(
    [string]$Seal010 = "dist\cordis-0.1.0-formal",
    [string]$Seal011 = "dist\cordis-0.1.1-formal",
    [string]$WorkRoot = "C:\cordis-version-matrix",
    [string]$TargetRoot = "C:\cordis-version-matrix-target",
    [string]$Cargo = ""
)

$ErrorActionPreference = "Stop"
$project = Resolve-Path (Join-Path $PSScriptRoot "..")
$cargo = if ($Cargo) { $Cargo } else { Join-Path $project "cargo.cmd" }

function Invoke-Host([string]$Ref, [string]$Target, [string]$Seal, [bool]$Policy) {
    $worktree = Join-Path $WorkRoot $Ref.Replace("/", "-")
    if (Test-Path $worktree) {
        git -C $project worktree remove --force $worktree | Out-Null
    }
    git -C $project worktree add --detach $worktree $Ref | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "worktree add failed: $Ref" }

    try {
        $env:CARGO_TARGET_DIR = $Target
        Push-Location $worktree
        try {
            & $cargo build -p cordis-cli
            if ($LASTEXITCODE -ne 0) { throw "host build failed: $Ref" }
        }
        finally {
            Pop-Location
        }
        $exe = Join-Path $Target "debug\cordis.exe"
        if (-not (Test-Path $exe)) { throw "missing host: $exe" }

        $sealPath = (Resolve-Path $Seal).Path
        $config = Join-Path $sealPath "smoke\cordis.yml"
        $plugins = Join-Path $sealPath "plugins"
        $args = @("inspect", $config, "--plugin-dir", $plugins)
        if ($Policy) {
            $args += @("--source-policy", (Join-Path $sealPath "allow-list.json"))
        }
        $output = & $exe @args 2>&1 | Out-String
        if ($LASTEXITCODE -ne 0) {
            throw "inspect failed: host=${Ref} seal=$Seal`n$output"
        }
        if ($output -notmatch "mounted = 0") {
            throw "inspect did not report mounted=0: host=${Ref} seal=$Seal`n$output"
        }
        foreach ($name in @("greeting", "hello", "harness-tool")) {
            if ($output -notmatch [regex]::Escape("(plugin: $name)")) {
                throw "missing plugin ${name}: host=${Ref} seal=$Seal`n$output"
            }
        }
        Write-Output "version matrix: host=$Ref seal=$([IO.Path]::GetFileName($sealPath)) policy=$Policy PASS"
    }
    finally {
        git -C $project worktree remove --force $worktree 2>$null
    }
}

Invoke-Host "v0.1.0" (Join-Path $TargetRoot "v0.1.0") $Seal010 $false
Invoke-Host "v0.1.1" (Join-Path $TargetRoot "v0.1.1") $Seal010 $false
Invoke-Host "v0.1.1" (Join-Path $TargetRoot "v0.1.1-current-seal") $Seal011 $false
Invoke-Host "HEAD" (Join-Path $TargetRoot "head") $Seal010 $true
Invoke-Host "HEAD" (Join-Path $TargetRoot "head-011") $Seal011 $true
