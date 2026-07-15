# Reverses whichever of `make install` / `make install-files` was used: if
# the packaged installer's uninstaller is present it's used (matching how
# Add/Remove Programs would uninstall), otherwise this falls back to a plain
# file removal plus PATH cleanup for the install-files layout.
[CmdletBinding()]
param(
    [string]$Prefix = "$env:LOCALAPPDATA\Programs\Summer"
)

$ErrorActionPreference = 'Stop'

function Remove-UserPath {
    param([string]$Dir)
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $current) { return }
    $parts = $current -split ';' | Where-Object { $_ -ne '' -and $_ -ne $Dir }
    [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User')
}

$uninstaller = Join-Path $Prefix 'unins000.exe'
if (Test-Path $uninstaller) {
    & $uninstaller '/VERYSILENT' '/SUPPRESSMSGBOXES' '/NORESTART'
    if ($LASTEXITCODE -ne 0) {
        throw "uninstaller exited with code $LASTEXITCODE"
    }
    Write-Host 'summer uninstalled via its packaged uninstaller.'
}
else {
    if (Test-Path $Prefix) {
        Remove-Item -Path $Prefix -Recurse -Force
    }
    Remove-UserPath -Dir $Prefix
    Write-Host "removed $Prefix"
}
