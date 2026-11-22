#requires -Version 5.1
<# Update only explicitly recognized machine-readable Rust version settings.
   No README, license text, release version, or source code is rewritten. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string]$Version
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Utf8 = [Text.UTF8Encoding]::new($false)
function ReplaceExactVersion([string]$RelativePath, [string]$Pattern) {
    $Path = Join-Path $Root $RelativePath
    $Text = [IO.File]::ReadAllText($Path)
    $Match = [regex]::Matches($Text, $Pattern)
    if ($Match.Count -ne 1) {
        throw "$RelativePath must have exactly one recognizable x.y.z Rust version field; got $($Match.Count)"
    }
    $Item = $Match[0]
    $Current = $Item.Groups[2].Value
    if ($Current -cne $Version) {
        $Replaced = $Text.Substring(0, $Item.Index) + $Item.Groups[1].Value + $Version + $Item.Groups[3].Value + $Text.Substring($Item.Index + $Item.Length)
        [IO.File]::WriteAllText($Path, $Replaced, $Utf8)
    }
    Write-Host "$RelativePath : $Current -> $Version"
}
ReplaceExactVersion 'Cargo.toml' '(?m)^(rust-version\s*=\s*")(\d+\.\d+\.\d+)("\s*)$'
ReplaceExactVersion 'rust-toolchain.toml' '(?m)^(channel\s*=\s*")(\d+\.\d+\.\d+)("\s*)$'
& (Join-Path $PSScriptRoot 'Assert-Rust-Version-Consistency.ps1')
