# Builds the release DLL and lays it out as a VST3 bundle in release\Haunter.vst3.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    $bundle = Join-Path $root 'release\Haunter.vst3\Contents\x86_64-win'
    New-Item -ItemType Directory -Force -Path $bundle | Out-Null
    Copy-Item -Force (Join-Path $root 'target\release\haunter.dll') (Join-Path $bundle 'Haunter.vst3')
    Write-Host "Packaged $(Join-Path $bundle 'Haunter.vst3')"
}
finally {
    Pop-Location
}
