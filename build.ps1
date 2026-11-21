# Builds the portable release executable: dist\NexMirror.exe
# Requires Rust (https://rustup.rs) with the x86_64-pc-windows-gnu toolchain.
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot
$sysroot = (& rustc --print sysroot).Trim()
$env:Path = "$env:Path;$sysroot\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained"
cargo test --release
cargo build --release
New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item target\release\nexmirror.exe dist\NexMirror.exe -Force
Get-Item dist\NexMirror.exe | Select-Object Name, @{n="Size (MB)"; e={[math]::Round($_.Length / 1MB, 2)}}, @{n="Version"; e={$_.VersionInfo.FileVersion}}
