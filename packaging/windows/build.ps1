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
# 固定 WinSW 版本并校验字节；绝不在构建机器注册服务。
# Pin and verify WinSW bytes; never register a service on the build machine.
$wrapperDir = Join-Path $root 'target/package-tools'
New-Item -ItemType Directory -Force $wrapperDir | Out-Null
$wrapper = Join-Path $wrapperDir 'ling-service.exe'
curl.exe --fail --location --retry 3 --output $wrapper 'https://github.com/winsw/winsw/releases/download/v2.12.0/WinSW-x64.exe'
if ($LASTEXITCODE -ne 0) { throw 'WinSW download failed' }
$expectedHash = '05B82D46AD331CC16BDC00DE5C6332C1EF818DF8CEEFCD49C726553209B3A0DA'
if ((Get-FileHash -Algorithm SHA256 -LiteralPath $wrapper).Hash -ne $expectedHash) { throw 'WinSW SHA256 mismatch' }
& $compiler "/DVersion=$Version" "/DSourceRoot=$root" (Join-Path $PSScriptRoot 'installer.iss')
if ($LASTEXITCODE -ne 0) { throw 'Installer build failed' }
$stage = Join-Path $root 'target/package-windows/ling'
New-Item -ItemType Directory -Force "$stage/bin", "$stage/docs" | Out-Null
Copy-Item target/x86_64-pc-windows-msvc/release/ling.exe, target/x86_64-pc-windows-msvc/release/ling-cli.exe "$stage/bin/"
Copy-Item packaging/ling.toml "$stage/ling.example.toml"
Copy-Item README.md $stage
Copy-Item docs/*.md "$stage/docs/"
Compress-Archive -Path $stage -DestinationPath "dist/ling_${Version}_windows_x86_64.zip" -Force
