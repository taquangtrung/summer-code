# Plain, per-user Windows install/uninstall for summer: no Inno Setup
# needed, just a file copy and a user PATH entry. Run via the Makefile
# (`make install-files`) or directly:
#   powershell -File scripts/windows-setup.ps1 -Action install -Prefix ...
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('install', 'config')]
    [string]$Action,

    [string]$Prefix = "$env:LOCALAPPDATA\Programs\Summer"
)

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $PSScriptRoot

function Add-UserPath {
    param([string]$Dir)
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $current) { $current = '' }
    $parts = $current -split ';' | Where-Object { $_ -ne '' }
    if ($parts -contains $Dir) { return }
    $updated = ($parts + $Dir) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
    Write-Host "added $Dir to the user PATH (open a new shell for it to take effect)"
}

switch ($Action) {
    'install' {
        $exe = Join-Path $RepoRoot 'target\release\summer.exe'
        if (-not (Test-Path $exe)) {
            throw "$exe not found. Run 'make build-release' first."
        }
        New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
        Copy-Item -Path $exe -Destination (Join-Path $Prefix 'summer.exe') -Force
        Add-UserPath -Dir $Prefix
        Write-Host "installed summer to $Prefix"
    }
}
