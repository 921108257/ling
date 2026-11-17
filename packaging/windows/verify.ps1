# 仅在 CI 测试安装器及其服务生命周期。
# Exercise the installer and service lifecycle on CI hosts only.
param([Parameter(Mandatory=$true)][string]$Version)
$ErrorActionPreference = 'Stop'
$installer = (Resolve-Path "dist/ling_${Version}_windows_x86_64_setup.exe").Path
$installed = Join-Path $env:ProgramFiles 'ling'
$data = Join-Path $env:ProgramData 'ling'
$originalPath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
# 从 ZIP 读取正文，不把源码或安装目录当作便携制品的证据。
# Read the ZIP texts rather than treating source or installed files as portable-package evidence.
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::OpenRead((Resolve-Path "dist/ling_${Version}_windows_x86_64.zip"))
try {
    foreach ($name in 'LICENSE', 'THIRD-PARTY-NOTICES.txt') {
        $entry = $archive.Entries | Where-Object { $_.FullName.Replace('\', '/') -eq "ling/$name" }
        if (-not $entry) { throw "Portable package is missing $name" }
        $reader = [IO.StreamReader]::new($entry.Open())
        try { $text = $reader.ReadToEnd() } finally { $reader.Dispose() }
        if ($name -eq 'LICENSE' -and $text -ne (Get-Content -Raw -LiteralPath LICENSE)) { throw 'Portable MIT license differs from source' }
        if ($name -eq 'THIRD-PARTY-NOTICES.txt' -and $text -notmatch '(?m)^ring ') { throw 'Portable dependency notices are incomplete' }
    }
} finally {
    $archive.Dispose()
}
function Install-Ling {
    $process = Start-Process -FilePath $installer -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/DIR=`"$installed`"" -WindowStyle Hidden -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw "Installer failed: $($process.ExitCode)" }
}
function Assert-Running {
    (Get-Service ling).WaitForStatus('Running', [TimeSpan]::FromSeconds(15))
    if ((Get-CimInstance Win32_Service -Filter "Name='ling'").StartName -ne 'NT AUTHORITY\LocalService') { throw 'Service is not running as LocalService' }
    & "$installed/bin/ling-cli.exe" --lang en-US -c PING
    if ($LASTEXITCODE -ne 0) { throw 'Installed service failed PING' }
}
Install-Ling
Assert-Running
if ((Get-Content -Raw -LiteralPath "$installed/LICENSE") -ne (Get-Content -Raw -LiteralPath LICENSE)) { throw 'Installed MIT license differs from source' }
$notices = Get-Content -Raw -LiteralPath "$installed/THIRD-PARTY-NOTICES.txt"
if ($notices -notmatch '(?m)^ring ' -or $notices -notmatch '(?m)^Rust standard library ') { throw 'Installed dependency notices are incomplete' }
if (-not (Test-Path -LiteralPath "$installed/docs/WinSW-LICENSE.txt")) { throw 'WinSW license is missing' }
if ([Environment]::GetEnvironmentVariable('Path', 'Machine') -notlike "*$installed\bin*") { throw 'Installer did not add PATH entry' }
Add-Content -LiteralPath "$data/ling.toml" -Value '# preserved during package upgrade'
$configHash = (Get-FileHash -LiteralPath "$data/ling.toml").Hash
New-Item -ItemType File -Path "$data/state/preserve-me" | Out-Null
Install-Ling
Assert-Running
if ((Get-FileHash -LiteralPath "$data/ling.toml").Hash -ne $configHash) { throw 'Upgrade replaced configuration' }
$process = Start-Process -FilePath "$installed/unins000.exe" -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -WindowStyle Hidden -Wait -PassThru
if ($process.ExitCode -ne 0) { throw 'Uninstall failed' }
if (Get-Service ling -ErrorAction SilentlyContinue) { throw 'Uninstall left service registered' }
if ((Get-FileHash -LiteralPath "$data/ling.toml").Hash -ne $configHash) { throw 'Uninstall replaced configuration' }
if (-not (Test-Path -LiteralPath "$data/state/preserve-me")) { throw 'Uninstall removed state' }
if ([Environment]::GetEnvironmentVariable('Path', 'Machine') -ne $originalPath) { throw 'Uninstall changed original PATH entries' }
# 显式跳过服务，且保留预先存在的 PATH 条目。
# Skip service registration explicitly and preserve a pre-existing PATH entry.
$existingPath = $originalPath.TrimEnd(';') + ";$installed\bin"
[Environment]::SetEnvironmentVariable('Path', $existingPath, 'Machine')
try {
    $process = Start-Process -FilePath $installer -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS=""', "/DIR=`"$installed`"" -WindowStyle Hidden -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw 'Installation with service skipped failed' }
    if (Get-Service ling -ErrorAction SilentlyContinue) { throw 'Skipped installation registered a service' }
    $process = Start-Process -FilePath "$installed/unins000.exe" -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -WindowStyle Hidden -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw 'Uninstall after service skip failed' }
    if ([Environment]::GetEnvironmentVariable('Path', 'Machine') -ne $existingPath) { throw 'Uninstall removed pre-existing PATH entry' }
} finally {
    [Environment]::SetEnvironmentVariable('Path', $originalPath, 'Machine')
}
