# 安装程序及便携 ZIP；Build an installer and portable ZIP.
param([Parameter(Mandatory=$true)][string]$Version)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
Set-Location $root
$compiler = 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe'
if (-not (Test-Path $compiler)) {
    choco install innosetup --yes --no-progress
    if ($LASTEXITCODE -ne 0) { throw 'Inno Setup installation failed' }
}
& $compiler "/DVersion=$Version" "/DSourceRoot=$root" (Join-Path $PSScriptRoot 'installer.iss')
if ($LASTEXITCODE -ne 0) { throw 'Installer build failed' }
$stage = Join-Path $root 'target/package-windows/linger-cache-db'
New-Item -ItemType Directory -Force "$stage/bin", "$stage/docs" | Out-Null
Copy-Item target/x86_64-pc-windows-msvc/release/linger-server.exe, target/x86_64-pc-windows-msvc/release/linger-cli.exe "$stage/bin/"
Copy-Item packaging/linger.toml "$stage/linger.example.toml"
Copy-Item README.md $stage
Copy-Item docs/config.md, docs/persistence.md "$stage/docs/"
Compress-Archive -Path $stage -DestinationPath "dist/linger-cache-db_${Version}_windows_x86_64.zip" -Force
