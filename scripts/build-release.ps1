$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Set-Location (Split-Path -Parent $PSScriptRoot)

$rustc = & rustc --version
if ($rustc -notmatch '1\.98\.') {
    throw "Expected Rust 1.98.x, found: $rustc"
}

cargo fmt --all --check
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo clippy --workspace --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo test --workspace
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo build --workspace --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$out = Join-Path (Get-Location) 'artifacts\release\win-x64'
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory -Path $out | Out-Null
Copy-Item 'target\release\game-larper.exe' (Join-Path $out 'GameLarper.exe')
Copy-Item 'target\release\game-larper-runner.exe' (Join-Path $out 'GameLarper.Runner.exe')
if (-not (Test-Path (Join-Path $out 'GameLarper.Runner.exe'))) {
    throw 'Runner missing from the release folder.'
}
$zip = Join-Path (Get-Location) 'artifacts\release\GameLarper-win-x64.zip'
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path (Join-Path $out '*') -DestinationPath $zip
Write-Host "Release ready: $out"
