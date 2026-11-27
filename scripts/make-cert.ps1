<#
.SYNOPSIS
Makes the TLS certificate for `ait serve --lan` (ECDSA P-256, self-signed,
valid 10 years) and prints its fingerprint in the form ait expects.

.EXAMPLE
.\scripts\make-cert.ps1
.\scripts\make-cert.ps1 -Dir D:\ait-tls -Force

Default folder: <git-common-dir>\ait\tls (next to the default database; never
committed). An existing certificate and key are kept unless -Force is given.
Uses the openssl.exe that ships with Git for Windows. Works in Windows
PowerShell 5.1 and PowerShell 7.
#>
param(
    [string]$Dir = "",
    [switch]$Force,
    [string]$Name = "ait-host"
)
# Not "Stop": Windows PowerShell 5.1 turns any line a native program (git,
# openssl) writes on stderr into a terminating error. Every step below
# checks its own result instead.
$ErrorActionPreference = "Continue"

function Fail([string]$msg, [int]$code = 1) {
    [Console]::Error.WriteLine("make-cert: $msg")
    exit $code
}

# Runs a native program; returns its combined output lines. $LASTEXITCODE
# tells whether it worked.
function Run-Native([string]$exe, [string[]]$argv) {
    $out = & $exe @argv 2>&1 | ForEach-Object { "$_" }
    return $out
}

function Find-OpenSsl {
    $cmd = Get-Command openssl.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $roots = @()
    if ($env:ProgramFiles) { $roots += (Join-Path $env:ProgramFiles "Git") }
    if (${env:ProgramFiles(x86)}) { $roots += (Join-Path ${env:ProgramFiles(x86)} "Git") }
    if ($env:LOCALAPPDATA) { $roots += (Join-Path $env:LOCALAPPDATA "Programs\Git") }
    $git = Get-Command git.exe -ErrorAction SilentlyContinue
    if ($git) {
        # ...\Git\cmd\git.exe or ...\Git\mingw64\bin\git.exe
        $roots += (Split-Path (Split-Path $git.Source -Parent) -Parent)
        $roots += (Split-Path (Split-Path (Split-Path $git.Source -Parent) -Parent) -Parent)
    }
    foreach ($r in $roots) {
        foreach ($sub in @("usr\bin\openssl.exe", "mingw64\bin\openssl.exe")) {
            $p = Join-Path $r $sub
            if (Test-Path -LiteralPath $p) { return $p }
        }
    }
    return $null
}

$openssl = Find-OpenSsl
if (-not $openssl) {
    Fail "openssl.exe not found. Install Git for Windows (it ships openssl in usr\bin) or add openssl to PATH."
}

if (-not $Dir) {
    $common = Run-Native "git" @("rev-parse", "--git-common-dir") | Select-Object -First 1
    if ($LASTEXITCODE -ne 0 -or -not $common) {
        Fail "not inside a git repository; give the folder: make-cert.ps1 -Dir <folder>" 2
    }
    $resolved = Resolve-Path -LiteralPath $common -ErrorAction SilentlyContinue
    if (-not $resolved) { Fail "cannot find the git folder '$common'" 2 }
    $Dir = Join-Path $resolved.Path "ait\tls"
}

$cert = Join-Path $Dir "cert.pem"
$key = Join-Path $Dir "key.pem"
$haveCert = Test-Path -LiteralPath $cert
$haveKey = Test-Path -LiteralPath $key

if (-not $Force -and ($haveCert -or $haveKey)) {
    # A complete pair: a certificate openssl can read and a key next to it.
    $complete = $false
    if ($haveCert -and $haveKey -and (Get-Item -LiteralPath $key).Length -gt 0) {
        $null = Run-Native $openssl @("x509", "-in", $cert, "-noout")
        $complete = ($LASTEXITCODE -eq 0)
    }
    if ($complete) {
        Fail ("$cert and $key already exist; pass -Force to replace them " +
            "(then restart ait serve --lan and update remote_fingerprint on every PC)")
    }
    $left = @()
    if ($haveCert) { $left += $cert }
    if ($haveKey) { $left += $key }
    Fail ("found an incomplete certificate/key pair, probably from an earlier failed run: " +
        ($left -join ", ") + ". Run again with -Force to replace it (nothing uses it yet).")
}

New-Item -ItemType Directory -Force -Path $Dir -ErrorAction SilentlyContinue | Out-Null
if (-not (Test-Path -LiteralPath $Dir)) { Fail "cannot create the folder $Dir" }
if ($Force) {
    Remove-Item -LiteralPath $cert, $key -Force -ErrorAction SilentlyContinue
}

# Keep Git's MSYS openssl from rewriting /CN=... as a Windows path.
$env:MSYS_NO_PATHCONV = "1"
$env:MSYS2_ARG_CONV_EXCL = "*"
# openssl reports progress ("Generating an EC private key") on stderr: keep it
# and show it only if something went wrong.
$log = Run-Native $openssl @(
    "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
    "-nodes", "-sha256", "-days", "3650", "-subj", "/CN=$Name",
    "-keyout", $key, "-out", $cert)
$ok = ($LASTEXITCODE -eq 0) -and (Test-Path -LiteralPath $cert) -and (Test-Path -LiteralPath $key)
if (-not $ok) {
    $log | ForEach-Object { [Console]::Error.WriteLine($_) }
    Remove-Item -LiteralPath $cert, $key -Force -ErrorAction SilentlyContinue
    Fail "openssl failed (output above); nothing was kept"
}

# The key stays readable by the current user only.
$null = Run-Native "icacls.exe" @($key, "/inheritance:r", "/grant:r", "$($env:USERNAME):(R,W)")
if ($LASTEXITCODE -ne 0) {
    Write-Warning "could not restrict the key file permissions (icacls failed)"
}

$line = Run-Native $openssl @("x509", "-in", $cert, "-noout", "-fingerprint", "-sha256") |
    Where-Object { $_ -match "=" } | Select-Object -First 1
if ($LASTEXITCODE -ne 0 -or -not $line) {
    Fail "cannot read the fingerprint of $cert"
}
$fp = (($line -split "=", 2)[1] -replace ":", "").Trim().ToLowerInvariant()
Write-Output "certificate: $cert"
Write-Output "key:         $key  (keep it on this PC only)"
Write-Output "fingerprint: sha256:$fp"
Write-Output ""
Write-Output "On every agent PC, in its remote.cfg:  remote_fingerprint = sha256:$fp"
