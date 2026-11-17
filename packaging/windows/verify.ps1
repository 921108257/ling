# 仅在一次性 CI / 虚拟机验收，日志保留环境、摘要、命令与结果。
# Run only on disposable CI hosts / VMs; retain environment, hashes, commands and results.
param(
    [Parameter(Mandatory=$true)][string]$Version,
    [string]$PreviousVersion = $env:PREVIOUS_VERSION,
    [string]$PreviousDist = $(if ($env:PREVIOUS_DIST) { $env:PREVIOUS_DIST } else { 'target/previous-release' }),
    [string]$UpdateVersion = $env:UPDATE_VERSION,
    [switch]$DisposableMachine
)
$ErrorActionPreference = 'Stop'
if ($env:CI -ne 'true' -and -not $DisposableMachine) { throw 'Use a disposable host and pass -DisposableMachine explicitly.' }
$installer = (Resolve-Path "dist/ling_$($Version)_windows_x86_64_setup.exe").Path
$installed = Join-Path $env:ProgramFiles 'ling'
$data = Join-Path $env:ProgramData 'ling'
if (Test-Path -LiteralPath "$installed/bin/ling.exe") { throw 'A clean host without an existing ling installation is required.' }
$originalPath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
New-Item -ItemType Directory -Path target/native-acceptance -Force | Out-Null
Start-Transcript -Path target/native-acceptance/windows-x86_64.log -Force | Out-Null
try {
    Get-Date -Format o
    $os = Get-CimInstance Win32_OperatingSystem
    $os | Select-Object Caption, Version, BuildNumber, OSArchitecture | Format-List | Out-Host
    Write-Host "candidate=$Version previous=$PreviousVersion update_target=$UpdateVersion"
    if ($os.Caption -notmatch 'Windows 11') { Write-Host 'Windows 11 acceptance: 未执行 / not executed (host OS is recorded above).' }
    Write-Host 'Interactive UAC cancellation: 未执行 / not executed by this unattended installer script.'
    Get-FileHash -Algorithm SHA256 -LiteralPath $installer, "dist/ling_$($Version)_windows_x86_64.zip" | Select-Object Path, Algorithm, Hash | ConvertTo-Json | Out-Host
    # ZIP 正文来自发布制品，不把安装目录当作便携包证据。
    # Read release ZIP texts rather than using the installed directory as portable-package evidence.
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead((Resolve-Path "dist/ling_$($Version)_windows_x86_64.zip"))
    try {
        foreach ($name in 'LICENSE', 'THIRD-PARTY-NOTICES.txt') {
            $entry = $archive.Entries | Where-Object { $_.FullName.Replace('\', '/') -eq "ling/$name" }
            if (-not $entry) { throw "Portable package is missing $name" }
            $reader = [IO.StreamReader]::new($entry.Open())
            try { $text = $reader.ReadToEnd() } finally { $reader.Dispose() }
            if ($name -eq 'LICENSE' -and $text -ne (Get-Content -Raw -LiteralPath LICENSE)) { throw 'Portable MIT license differs from source' }
            if ($name -eq 'THIRD-PARTY-NOTICES.txt' -and $text -notmatch '(?m)^ring ') { throw 'Portable dependency notices are incomplete' }
        }
    } finally { $archive.Dispose() }

    function Invoke-Installer([string]$Path, [string[]]$Arguments) {
        Write-Host "Start-Process $Path $($Arguments -join ' ')"
        $process = Start-Process -FilePath $Path -ArgumentList $Arguments -WindowStyle Hidden -Wait -PassThru
        Write-Host "installer_exit_code=$($process.ExitCode)"
        if ($process.ExitCode -ne 0) { throw "Installer failed: $($process.ExitCode)" }
    }
    function Assert-Versions([string]$Expected) {
        foreach ($name in 'ling', 'ling-cli') {
            Write-Host "$installed/bin/$name.exe --version"
            $output = & "$installed/bin/$name.exe" --version
            if ($LASTEXITCODE -ne 0 -or "$output".Trim() -ne "$name $Expected") { throw "Unexpected $name version: $output" }
            Write-Host $output
        }
    }
    function Assert-Running {
        Write-Host 'Get-Service ling; verify LocalService; ling-cli --lang en-US -c PING'
        (Get-Service ling).WaitForStatus('Running', [TimeSpan]::FromSeconds(30))
        if ((Get-CimInstance Win32_Service -Filter "Name='ling'").StartName -ne 'NT AUTHORITY\LocalService') { throw 'Service is not running as LocalService' }
        for ($attempt = 0; $attempt -lt 30; $attempt++) {
            & "$installed/bin/ling-cli.exe" --lang en-US -c PING
            if ($LASTEXITCODE -eq 0) { return }
            Start-Sleep -Seconds 1
        }
        throw 'Installed service failed PING'
    }
    function Install-Ling([string]$Package = $installer, [string]$Expected = $Version) {
        Get-FileHash -Algorithm SHA256 -LiteralPath $Package | Select-Object Path, Algorithm, Hash | ConvertTo-Json | Out-Host
        Invoke-Installer $Package @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/DIR="' + $installed + '"'))
        Assert-Running
        Assert-Versions $Expected
    }
    function Assert-Preserved {
        if ((Get-FileHash -LiteralPath "$data/ling.toml").Hash -ne $configHash) { throw 'Configuration was replaced' }
        if (-not (Test-Path -LiteralPath "$data/state/preserve-me")) { throw 'State marker was removed' }
        # Windows 仅支持 Fast；该标记不表示已经验证 Reliable Journal。
        # Windows supports Fast only; this marker does not claim Reliable journal coverage.
        Write-Host 'Configuration hash and state marker preserved; Reliable Journal is unsupported on Windows.'
    }
    function Uninstall-Ling {
        Invoke-Installer "$installed/unins000.exe" @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART')
        if (Get-Service ling -ErrorAction SilentlyContinue) { throw 'Uninstall left service registered' }
        Assert-Preserved
        if ([Environment]::GetEnvironmentVariable('Path', 'Machine') -ne $originalPath) { throw 'Uninstall changed original PATH entries' }
    }

    Install-Ling
    Write-Host 'Stop-Service ling'
    Stop-Service ling
    (Get-Service ling).WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30))
    Write-Host 'Start-Service ling'
    Start-Service ling
    Assert-Running
    Write-Host 'Restart-Service ling'
    Restart-Service ling
    Assert-Running
    if ((Get-Content -Raw -LiteralPath "$installed/LICENSE") -ne (Get-Content -Raw -LiteralPath LICENSE)) { throw 'Installed MIT license differs from source' }
    $notices = Get-Content -Raw -LiteralPath "$installed/THIRD-PARTY-NOTICES.txt"
    if ($notices -notmatch '(?m)^ring ' -or $notices -notmatch '(?m)^Rust standard library ') { throw 'Installed dependency notices are incomplete' }
    if (-not (Test-Path -LiteralPath "$installed/docs/WinSW-LICENSE.txt")) { throw 'WinSW license is missing' }
    if ([Environment]::GetEnvironmentVariable('Path', 'Machine') -notlike "*$installed\bin*") { throw 'Installer did not add PATH entry' }
    Add-Content -LiteralPath "$data/ling.toml" -Value '# preserved during package upgrade'
    $configHash = (Get-FileHash -LiteralPath "$data/ling.toml").Hash
    Write-Host "config_sha256=$configHash"
    New-Item -ItemType File -Path "$data/state/preserve-me" -Force | Out-Null
    Install-Ling
    Assert-Preserved
    Uninstall-Ling
    if ($PreviousVersion) {
        $previous = (Resolve-Path "$PreviousDist/ling_$($PreviousVersion)_windows_x86_64_setup.exe").Path
        Install-Ling $previous $PreviousVersion
        Assert-Preserved
        Install-Ling
        Assert-Preserved
        Uninstall-Ling
        if ($UpdateVersion) {
            if ($UpdateVersion -ne $Version) { throw 'UPDATE_VERSION must equal the candidate version.' }
            Install-Ling $previous $PreviousVersion
            Write-Host "$installed/bin/ling.exe --lang en-US update --version $UpdateVersion"
            $updateOutput = & "$installed/bin/ling.exe" --lang en-US update --version $UpdateVersion
            if ($LASTEXITCODE -ne 0) { throw 'ling update failed before the helper completed.' }
            $updateOutput | Out-Host
            $resultLine = $updateOutput | Where-Object { $_ -match '^Result file: ' } | Select-Object -Last 1
            if (-not $resultLine) { throw 'ling update did not provide a helper result path.' }
            $resultPath = $resultLine.Substring('Result file: '.Length).Trim()
            $deadline = [DateTime]::UtcNow.AddMinutes(5)
            do {
                $result = Get-Content -Raw -LiteralPath $resultPath | ConvertFrom-Json
                if ($result.status -eq 'success') { break }
                if ($result.status -ne 'scheduled') { $result | ConvertTo-Json | Out-Host; throw 'The update helper reported failure or an unknown outcome.' }
                if ([DateTime]::UtcNow -ge $deadline) { throw "The update helper did not finish; inspect $resultPath before retrying." }
                Start-Sleep -Seconds 1
            } while ($true)
            $result | ConvertTo-Json | Out-Host
            Assert-Running
            Assert-Versions $Version
            Assert-Preserved
            Uninstall-Ling
        } else {
            Write-Host 'ling update native upgrade: 未执行 / not executed (requires a published UPDATE_VERSION).'
        }
    } else {
        Write-Host 'Previous-version package upgrade and ling update: 未执行 / not executed (PREVIOUS_VERSION is unset).'
    }
    # 显式跳过服务，且保留预先存在的 PATH 条目。
    # Skip service registration explicitly and preserve a pre-existing PATH entry.
    $existingPath = $originalPath.TrimEnd(';') + ";$installed\bin"
    [Environment]::SetEnvironmentVariable('Path', $existingPath, 'Machine')
    try {
        Invoke-Installer $installer @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS=""', ('/DIR="' + $installed + '"'))
        if (Get-Service ling -ErrorAction SilentlyContinue) { throw 'Skipped installation registered a service' }
        Invoke-Installer "$installed/unins000.exe" @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART')
        Assert-Preserved
        if ([Environment]::GetEnvironmentVariable('Path', 'Machine') -ne $existingPath) { throw 'Uninstall removed pre-existing PATH entry' }
    } finally {
        [Environment]::SetEnvironmentVariable('Path', $originalPath, 'Machine')
    }
    Write-Host 'native_acceptance_exit_code=0'
} catch {
    Write-Host "native_acceptance_exit_code=1; error=$($_.Exception.Message)"
    throw
} finally {
    Stop-Transcript | Out-Null
}
