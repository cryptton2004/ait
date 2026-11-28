<#
.SYNOPSIS
Builds ait in release mode and copies ait.exe next to this script (the
repository root, where /ait.exe is git-ignored).

.EXAMPLE
.\build.ps1
.\build.ps1 -Gate
.\build.ps1 -Dest D:\code\my-game\tools\ait\ait.exe
.\build.ps1 -Jobs 4

-Gate     runs the quality gate (fmt, clippy, tests) before building.
-Dest     also copies the new ait.exe to this path (for example the
          tools\ait\ait.exe of a repository that uses ait).
-Jobs     build parallelism; default is half the CPU cores so the PC stays
          usable.

The dashboard page is built into ait.exe, so a dashboard change only shows
after a rebuild and a restart of `ait serve`. Stop any running ait.exe
before building: Windows will not let a running program be replaced.
If vendor\ and .cargo\config.toml exist, cargo uses them by itself and the
build works offline. Works in Windows PowerShell 5.1 and PowerShell 7.
#>
param(
    [switch]$Gate,
    [string]$Dest = "",
    [int]$Jobs = 0
)
# Not "Stop": Windows PowerShell 5.1 turns any line a native program (cargo)
# writes on stderr into a terminating error. Every step checks its own result.
$ErrorActionPreference = "Continue"

function Fail([string]$msg, [int]$code = 1) {
    [Console]::Error.WriteLine("build: $msg")
    exit $code
}

function Step([string]$label, [string[]]$argv) {
    Write-Host "==> cargo $($argv -join ' ')" -ForegroundColor Cyan
    & cargo @argv
    if ($LASTEXITCODE -ne 0) { Fail "$label failed (exit code $LASTEXITCODE)" $LASTEXITCODE }
}

$root = $PSScriptRoot
if (-not $root) { $root = Split-Path -Parent $MyInvocation.MyCommand.Path }
if (-not (Test-Path (Join-Path $root "Cargo.toml"))) { Fail "Cargo.toml not found next to build.ps1 ($root)" }
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Fail "cargo not found. Install Rust from https://rustup.rs and open a new terminal."
}

$built = Join-Path $root "target\release\ait.exe"
$local = Join-Path $root "ait.exe"

# A running ait.exe (for example `ait serve`) locks the file it runs from.
$running = @(Get-Process -Name ait -ErrorAction SilentlyContinue | Where-Object {
    $_.Path -and (($_.Path -ieq $built) -or ($_.Path -ieq $local) -or ($Dest -and ($_.Path -ieq $Dest)))
})
if ($running.Count -gt 0) {
    $ids = ($running | ForEach-Object { "$($_.Id) $($_.Path)" }) -join "; "
    Fail "ait.exe is running ($ids). Stop it (Ctrl+C in its window, or Stop-Process -Id <id>) and run build.ps1 again."
}

if ($Jobs -le 0) {
    $Jobs = [Math]::Max(1, [int][Math]::Floor([Environment]::ProcessorCount / 2))
}

$oldJobs = $env:CARGO_BUILD_JOBS
$env:CARGO_BUILD_JOBS = "$Jobs"
Push-Location $root
try {
    if (Test-Path (Join-Path $root "vendor")) { Write-Host "Offline build: using vendor\" -ForegroundColor DarkGray }
    if ($Gate) {
        Step "format check" @("fmt", "--all", "--", "--check")
        Step "clippy" @("clippy", "--all-targets", "--all-features", "--", "-D", "warnings")
        Step "tests" @("test", "--all-targets")
    }
    Step "release build" @("build", "--release")
}
finally {
    Pop-Location
    $env:CARGO_BUILD_JOBS = $oldJobs
}

if (-not (Test-Path $built)) { Fail "build finished but $built is missing" }

Copy-Item -LiteralPath $built -Destination $local -Force
if (-not $?) { Fail "could not copy to $local" }
Write-Host "Built: $local" -ForegroundColor Green

if ($Dest) {
    $destDir = Split-Path -Parent $Dest
    if ($destDir -and -not (Test-Path $destDir)) { Fail "folder for -Dest does not exist: $destDir" }
    Copy-Item -LiteralPath $built -Destination $Dest -Force
    if (-not $?) { Fail "could not copy to $Dest" }
    Write-Host "Copied: $Dest" -ForegroundColor Green
}

& $local version
