<#
.SYNOPSIS
Makes the TLS certificate for `ait serve --lan` (ECDSA P-256, self-signed,
valid 10 years) and prints its fingerprint in the form ait expects.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts\make-cert.ps1
powershell -ExecutionPolicy Bypass -File scripts\make-cert.ps1 -Dir D:\ait-tls -Force

Default folder: <git-common-dir>\ait\tls (next to the default database; never
committed). Existing files are kept unless -Force is given. Uses the
openssl.exe that ships with Git for Windows.
#>
param(
    [string]$Dir = "",
    [switch]$Force,
    [string]$Name = "ait-host"
)
$ErrorActionPreference = "Stop"

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
    Write-Error "openssl.exe not found. Install Git for Windows (it ships openssl in usr\bin) or add openssl to PATH."
    exit 1
}

if (-not $Dir) {
    $common = (& git rev-parse --git-common-dir 2>$null)
    if ($LASTEXITCODE -ne 0 -or -not $common) {
        Write-Error "Not inside a git repository; give the folder: make-cert.ps1 -Dir <folder>"
        exit 2
    }
    $Dir = Join-Path (Resolve-Path -LiteralPath $common).Path "ait\tls"
}

$cert = Join-Path $Dir "cert.pem"
$key = Join-Path $Dir "key.pem"
if (-not $Force -and ((Test-Path -LiteralPath $cert) -or (Test-Path -LiteralPath $key))) {
    Write-Error ("$cert or $key already exists; pass -Force to replace them " +
        "(then restart ait serve --lan and update remote_fingerprint on every PC)")
    exit 1
}

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
# Keep Git's MSYS openssl from rewriting /CN=... as a Windows path.
$env:MSYS_NO_PATHCONV = "1"
$env:MSYS2_ARG_CONV_EXCL = "*"
& $openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -sha256 `
    -days 3650 -subj "/CN=$Name" -keyout $key -out $cert 2>$null
if ($LASTEXITCODE -ne 0) {
    Write-Error "openssl failed"
    exit 1
}

# The key stays readable by the current user only.
& icacls.exe $key /inheritance:r /grant:r "$($env:USERNAME):(R,W)" | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Warning "could not restrict the key file permissions (icacls failed)"
}

$line = (& $openssl x509 -in $cert -noout -fingerprint -sha256)
$fp = (($line -split "=", 2)[1] -replace ":", "").ToLowerInvariant()
Write-Output "certificate: $cert"
Write-Output "key:         $key  (keep it on this PC only)"
Write-Output "fingerprint: sha256:$fp"
Write-Output ""
Write-Output "On every agent PC, in default.cfg:  remote_fingerprint = sha256:$fp"
