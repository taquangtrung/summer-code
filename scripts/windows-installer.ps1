# Builds the packaged Windows installer (Inno Setup) at the version declared
# in Cargo.toml. Requires Inno Setup 6 (ISCC.exe): https://jrsoftware.org/isdl.php
#
# Run via the Makefile (`make installer` to just build, `make install` to
# also run it) or directly:
#   powershell -File scripts/windows-installer.ps1 [-Run]
[CmdletBinding()]
param(
    # Silently run the freshly built installer afterwards, so it registers
    # in Add/Remove Programs instead of just producing the setup.exe.
    [switch]$Run
)

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $PSScriptRoot

$cargoToml = Join-Path $RepoRoot 'Cargo.toml'
$match = Select-String -Path $cargoToml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $match) {
    throw "could not read version from $cargoToml"
}
$version = $match.Matches[0].Groups[1].Value

$iscc = Get-Command 'ISCC.exe' -ErrorAction SilentlyContinue
if ($iscc) {
    $isccPath = $iscc.Source
}
else {
    $candidates = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    )
    $isccPath = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $isccPath) {
        throw "ISCC.exe (Inno Setup 6) not found. Install it from https://jrsoftware.org/isdl.php"
    }
}

$exe = Join-Path $RepoRoot 'target\release\summer.exe'
if (-not (Test-Path $exe)) {
    throw "$exe not found. Run 'make build-release' first."
}

$issPath = Join-Path $RepoRoot 'packaging\windows\summer.iss'
$outputExe = Join-Path $RepoRoot "target\installer\summer-setup-$version.exe"

& $isccPath '/Q' "/DMyAppVersion=$version" $issPath
if ($LASTEXITCODE -ne 0) {
    throw "ISCC.exe failed with exit code $LASTEXITCODE"
}
Write-Host "built $outputExe"

if ($Run) {
    if (-not (Test-Path $outputExe)) {
        throw "$outputExe not found after build"
    }
    & $outputExe '/VERYSILENT' '/SUPPRESSMSGBOXES' '/NORESTART'
    if ($LASTEXITCODE -ne 0) {
        throw "installer exited with code $LASTEXITCODE"
    }
    Write-Host 'Summer installed for the current user.'
}
