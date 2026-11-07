param(
    [string]$Seal = "",
    [string]$Exe = "",
    [string]$ServiceName = "CordisScmSmoke",
    [string]$LogPath = ""
)

$ErrorActionPreference = "Stop"
$project = Resolve-Path (Join-Path $PSScriptRoot "..")

if (-not $Seal) {
    $Seal = Join-Path $project "dist\cordis-0.1.0-rc2"
}
$Seal = (Resolve-Path $Seal).Path
$plugins = Join-Path $Seal "plugins"
if (-not (Test-Path $plugins)) {
    throw "sealed plugin directory not found: $plugins"
}

if (-not $Exe) {
    $Exe = "C:\cordis-rs-target\debug\cordis.exe"
}
if (-not (Test-Path $Exe)) {
    throw "cordis.exe not found: $Exe; build it with cargo.cmd build -p cordis-cli or pass -Exe"
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "This script must run in an elevated PowerShell (administrator)."
}

if ($LogPath) {
    Start-Transcript -Path $LogPath -Force | Out-Null
}

$config = Join-Path $env:TEMP "cordis-scm-$PID.yml"
@(
    "- id: greeting",
    "  name: greeting"
) | Set-Content -Encoding ASCII $config

$binPath = "`"$Exe`" service `"$config`" --name $ServiceName --plugin-dir `"$plugins`""
$created = $false
try {
    & sc.exe create $ServiceName binPath= $binPath start= demand | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "sc create failed with $LASTEXITCODE" }
    $created = $true

    & sc.exe start $ServiceName | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "sc start failed with $LASTEXITCODE" }

    $deadline = (Get-Date).AddSeconds(30)
    do {
        $query = & sc.exe query $ServiceName
        if ($LASTEXITCODE -ne 0) { throw "sc query failed with $LASTEXITCODE" }
        $text = $query -join "`n"
        if ($text -match "RUNNING") { break }
        Start-Sleep -Seconds 1
    } while ((Get-Date) -lt $deadline)
    if ($text -notmatch "RUNNING") {
        throw "service did not reach RUNNING:`n$text"
    }

    & sc.exe stop $ServiceName | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "sc stop failed with $LASTEXITCODE" }

    $deadline = (Get-Date).AddSeconds(30)
    do {
        $query = & sc.exe query $ServiceName
        if ($LASTEXITCODE -ne 0) { throw "sc query failed with $LASTEXITCODE" }
        $text = $query -join "`n"
        if ($text -match "STOPPED") { break }
        Start-Sleep -Seconds 1
    } while ((Get-Date) -lt $deadline)
    if ($text -notmatch "STOPPED") {
        throw "service did not reach STOPPED:`n$text"
    }

    Write-Output "SCM smoke passed: create -> RUNNING -> stop -> STOPPED"
    Write-Output "service=$ServiceName config=$config plugins=$plugins"
}
finally {
    if ($created) {
        & sc.exe delete $ServiceName | Out-Null
    }
    Remove-Item -Force $config -ErrorAction SilentlyContinue
    if ($LogPath) {
        Stop-Transcript | Out-Null
    }
}
