#requires -Version 5.1
<# Isolated smoke test for structured version synchronization. Does not edit the repository. #>
[CmdletBinding()]
param()
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Temp = Join-Path ([IO.Path]::GetTempPath()) ('codex-version-sync-test-' + [guid]::NewGuid().ToString('N'))
try {
    New-Item -ItemType Directory -Force -Path (Join-Path $Temp 'tools') | Out-Null
    foreach ($name in @('Cargo.toml', 'rust-toolchain.toml')) {
        Copy-Item -LiteralPath (Join-Path $Root $name) -Destination (Join-Path $Temp $name)
    }
    foreach ($name in @('Sync-Rust-Version.ps1', 'Assert-Rust-Version-Consistency.ps1')) {
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination (Join-Path (Join-Path $Temp 'tools') $name)
    }
    $manifest = [IO.File]::ReadAllText((Join-Path $Temp 'Cargo.toml'))
    $toolchain = [IO.File]::ReadAllText((Join-Path $Temp 'rust-toolchain.toml'))
    $packageVersion = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"\r\n]+)"').Groups[1].Value
    $probe = '99.98.97'
    & (Join-Path (Join-Path $Temp 'tools') 'Sync-Rust-Version.ps1') -Version $probe
    $actual = & (Join-Path (Join-Path $Temp 'tools') 'Assert-Rust-Version-Consistency.ps1') -EmitVersion
    if ($actual -cne $probe) { throw 'Version sync failed' }
    $after = [IO.File]::ReadAllText((Join-Path $Temp 'Cargo.toml'))
    $afterVersion = [regex]::Match($after, '(?m)^version\s*=\s*"([^"\r\n]+)"').Groups[1].Value
    if ($afterVersion -cne $packageVersion) { throw 'Product release version was accidentally changed' }
    & (Join-Path (Join-Path $Temp 'tools') 'Sync-Rust-Version.ps1') -Version $probe
    if ($after -cne [IO.File]::ReadAllText((Join-Path $Temp 'Cargo.toml'))) { throw 'Sync not idempotent' }
    $back = [regex]::Match($manifest, '(?m)^rust-version\s*=\s*"([^"\r\n]+)"').Groups[1].Value
    & (Join-Path (Join-Path $Temp 'tools') 'Sync-Rust-Version.ps1') -Version $back
    if ($manifest -cne [IO.File]::ReadAllText((Join-Path $Temp 'Cargo.toml'))) { throw 'Manifest round-trip changes unrelated fields or formatting' }
    if ($toolchain -cne [IO.File]::ReadAllText((Join-Path $Temp 'rust-toolchain.toml'))) { throw 'Toolchain round-trip changes unrelated fields or formatting' }
    Write-Host 'Rust structured version sync: PASS (round-trip, idempotence, product version protection)'
} finally {
    if (Test-Path -LiteralPath $Temp) { Remove-Item -LiteralPath $Temp -Recurse -Force }
}
