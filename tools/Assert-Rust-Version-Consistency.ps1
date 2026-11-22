#requires -Version 5.1
<# The Rust toolchain pin is the only hand-maintained Rust toolchain version.
   Both CI and nightly compatibility checks reject inconsistent MSRV values. #>
[CmdletBinding()]
param([switch]$EmitVersion)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Manifest = [IO.File]::ReadAllText((Join-Path $Root 'Cargo.toml'))
$Toolchain = [IO.File]::ReadAllText((Join-Path $Root 'rust-toolchain.toml'))
$Msrv = [regex]::Matches($Manifest, '(?m)^rust-version\s*=\s*"(\d+\.\d+\.\d+)"\s*$')
$Pinned = [regex]::Matches($Toolchain, '(?m)^channel\s*=\s*"(\d+\.\d+\.\d+)"\s*$')
if ($Msrv.Count -ne 1 -or $Pinned.Count -ne 1) {
    throw 'Rust version is missing, duplicated, or not an exact stable x.y.z in Cargo.toml / rust-toolchain.toml'
}
$Version = $Pinned[0].Groups[1].Value
if ($Msrv[0].Groups[1].Value -cne $Version) {
    throw "Cargo rust-version ($($Msrv[0].Groups[1].Value)) differs from rust-toolchain.toml ($Version)"
}
if ($EmitVersion) { Write-Output $Version }
else { Write-Host "Rust MSRV/toolchain consistent: $Version" }
