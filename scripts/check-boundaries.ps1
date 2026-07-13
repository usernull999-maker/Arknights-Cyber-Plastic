$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$bridgeManifest = Join-Path $root 'crates/ruler-bridge/Cargo.toml'
$manifests = Get-ChildItem -Path $root -Recurse -Filter Cargo.toml |
    Where-Object { $_.FullName -notlike '*third_party\cost-bar-ruler*' }

foreach ($manifest in $manifests) {
    if ($manifest.FullName -eq $bridgeManifest -or $manifest.DirectoryName -eq $root) {
        continue
    }
    if (Select-String -LiteralPath $manifest.FullName -Pattern 'ruler-core|cost-bar-ruler' -Quiet) {
        throw "CostBarRuler dependency leaked outside ruler-bridge: $($manifest.FullName)"
    }
}

$leaks = rg -n --glob '*.rs' --glob '!third_party/**' '^\s*use\s+ruler_core' $root
if ($LASTEXITCODE -eq 0) {
    throw "Direct ruler_core imports are forbidden outside ruler-bridge:`n$leaks"
}
if ($LASTEXITCODE -ne 1) {
    throw 'Failed to inspect Rust import boundaries'
}

Write-Output 'CostBarRuler dependency boundary is intact.'
