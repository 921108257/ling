# 父进程退出后应用已验证的更新，不强杀占用文件的其他进程。
# Apply verified updates after the parent exits; never kill other processes holding files.
param([Parameter(Mandatory=$true)][string]$PlanPath)
$ErrorActionPreference = 'Stop'
$workspace = Split-Path -Parent $PlanPath
$resultPath = Join-Path $workspace 'result.json'
$phase = 'prepare'
$failureCode = 'LOCAL_UPDATE_INSTALL'
$updateLock = $null
$records = @()
$rollback = $true
$committed = $false
$installerStarted = $false
$installerPid = $null
$nativeMarker = $null

# 结果原子替换，消息只含固定状态与错误码，不含用户路径/异常文本。
# Replace results atomically; include only fixed states/codes, never paths or exception text.
function Write-Result([string]$Status, [string]$Code, $ExitCode = $null) {
    $result = @{ status = $Status; code = $Code; phase = $script:phase; exit_code = $ExitCode; rollback_complete = $script:rollback; helper_pid = $PID; installer_pid = $script:installerPid; installer_started = $script:installerStarted; updated_at_unix_ms = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() }
    $temporary = $resultPath + '.tmp'
    [IO.File]::WriteAllText($temporary, ($result | ConvertTo-Json -Compress) + "`n", (New-Object Text.UTF8Encoding($false)))
    if ([IO.File]::Exists($resultPath)) {
        # Windows PowerShell 5.1 会把 null 字符串转换为空路径，使用显式备份路径。
        # Windows PowerShell 5.1 converts a null string to an empty path; use an explicit backup path.
        $backup = $resultPath + '.previous'
        [IO.File]::Replace($temporary, $resultPath, $backup)
        Remove-Item -LiteralPath $backup -ErrorAction SilentlyContinue
    }
    else { [IO.File]::Move($temporary, $resultPath) }
}

# 固定摘要只比较字节，不运行归档或二进制。
# Verify exact bytes without executing archives or binaries.
function Assert-Hash([string]$Path, [string]$Expected, [string]$Failure = 'LOCAL_UPDATE_CHECKSUM') {
    # 使用系统 .NET，避免继承 PowerShell 7 模块路径时缺失 5.1 的哈希命令。
    # Use system .NET so inherited PowerShell 7 module paths cannot hide the 5.1 hashing command.
    $hash = [Security.Cryptography.SHA256]::Create()
    $source = [IO.File]::OpenRead($Path)
    try { $actual = ([BitConverter]::ToString($hash.ComputeHash($source))).Replace('-', '') }
    finally { $source.Dispose(); $hash.Dispose() }
    if ($actual -ne $Expected) {
        $script:failureCode = $Failure
        throw 'hash mismatch'
    }
}

try {
    $plan = Get-Content -LiteralPath $PlanPath -Raw -Encoding UTF8 | ConvertFrom-Json
    $phase = 'wait-parent'
    if ($plan.parent_pid -gt 0) {
        Write-Result 'scheduled' ''
        $parent = Get-Process -Id $plan.parent_pid -ErrorAction SilentlyContinue
        if ($parent -and -not $parent.WaitForExit(60000)) {
            $failureCode = 'LOCAL_UPDATE_BUSY'
            throw 'parent still running'
        }
    }
    # 先互斥再比较安装快照，旧计划不能覆盖其他更新已提交的程序。
    # Lock before comparing the installation snapshot so stale plans cannot overwrite another committed update.
    $phase = 'update-lock'
    $failureCode = 'LOCAL_UPDATE_BUSY'
    $updateLock = [IO.File]::Open($plan.lock_path, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    if ($plan.installer) {
        $nativeMarker = $plan.lock_path + '.pending.json'
        if ([IO.File]::Exists($nativeMarker)) {
            # 帮助进程超时或被终止后，安装器可能继续；禁止用新计划覆盖未知状态。
            # After helper timeout or termination the installer may continue; never overwrite an unresolved install.
            $phase = 'installer-pending'
            $rollback = $null
            $installerStarted = $null
            Write-Result 'unknown' 'LOCAL_UPDATE_PENDING'
            exit 1
        }
    }
    $phase = 'current-check'
    Assert-Hash $plan.current $plan.expected_current_sha256 'LOCAL_UPDATE_BUSY'
    $phase = 'verify'
    $failureCode = 'LOCAL_UPDATE_INSTALL'
    if ($plan.installer) {
        Assert-Hash $plan.installer.source $plan.installer.sha256
        $phase = 'installer-start'
        # 原生安装器管理服务与已有配置；UAC 只由标准 RunAs 提示。
        # The native installer manages services/configuration; only standard RunAs requests UAC.
        $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
        $principal = New-Object Security.Principal.WindowsPrincipal($identity)
        $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/DIR="' + $plan.installer.install_dir + '"'))
        $start = @{ FilePath = $plan.installer.source; ArgumentList = $arguments; PassThru = $true; WindowStyle = 'Hidden' }
        if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { $start.Verb = 'RunAs' }
        Write-Result 'scheduled' ''
        # 先记录启动意图，覆盖助手在启动后、记录 PID 前中止的窗口。
        # Persist launch intent first, covering helper termination between starting and recording the PID.
        [IO.File]::WriteAllText($nativeMarker, (@{ status = 'launching'; helper_pid = $PID; installer_pid = $null; updated_at_unix_ms = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() } | ConvertTo-Json -Compress))
        $installer = Start-Process @start
        $installerStarted = $true
        $installerPid = $installer.Id
        # 只有 Start-Process 成功返回后，安装内容才可能已经改变。
        # Only a successful Start-Process return makes installation changes possible.
        $rollback = $null
        [IO.File]::WriteAllText($nativeMarker, (@{ status = 'running'; helper_pid = $PID; installer_pid = $installerPid; installer_started_at_utc = $installer.StartTime.ToUniversalTime().ToString('o'); updated_at_unix_ms = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() } | ConvertTo-Json -Compress))
        $phase = 'installer'
        Write-Result 'scheduled' ''
        $installerDeadline = [DateTime]::UtcNow.AddMinutes(10)
        while (-not $installer.WaitForExit(5000)) {
            if ([DateTime]::UtcNow -ge $installerDeadline) {
                # 有界等待后保留启动记录与工作目录；新助手也会拒绝直接重试。
                # Bound waiting while preserving launch records and staging; another helper also rejects retries.
                $phase = 'installer-timeout'
                Write-Result 'unknown' 'LOCAL_UPDATE_PENDING'
                exit 1
            }
            Write-Result 'scheduled' ''
        }
        $installer.Refresh()
        if ($installer.ExitCode -ne 0) { Write-Result 'unknown' 'LOCAL_UPDATE_INSTALL' $installer.ExitCode; exit 1 }
        Remove-Item -LiteralPath $nativeMarker
    } else {
        $directory = Split-Path -Parent $plan.binaries[0].target
        $phase = 'portable-stage'
        $stage = Join-Path $directory ('.ling-update-' + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $stage | Out-Null
        foreach ($binary in $plan.binaries) {
            Assert-Hash $binary.source $binary.sha256
            $name = Split-Path -Leaf $binary.target
            $staged = Join-Path $stage ($name + '.new')
            Copy-Item -LiteralPath $binary.source -Destination $staged
            Assert-Hash $staged $binary.sha256
            $records += @{ target = $binary.target; staged = $staged; backup = (Join-Path $stage ($name + '.old')); backed_up = $false; installed = $false }
        }
        $phase = 'portable-replace'
        $failureCode = 'LOCAL_UPDATE_BUSY'
        foreach ($record in $records) {
            if (Test-Path -LiteralPath $record.target) {
                Move-Item -LiteralPath $record.target -Destination $record.backup
                $record.backed_up = $true
            }
            Move-Item -LiteralPath $record.staged -Destination $record.target
            $record.installed = $true
        }
        $committed = $true
        $phase = 'portable-cleanup'
        # 只清理已知文件，不递归删除计算得到的目录。
        # Remove known files only; never recursively delete a computed directory.
        # 替换成功后清理失败只留下备份，不能在备份已部分删除时尝试回滚。
        # Cleanup failures after replacement leave backups; never roll back after partially deleting backups.
        foreach ($record in $records) { if ($record.backed_up) { Remove-Item -LiteralPath $record.backup -ErrorAction SilentlyContinue } }
        Remove-Item -LiteralPath $stage -ErrorAction SilentlyContinue
    }
    $phase = 'complete'
    Write-Result 'success' '' 0
} catch {
    # UAC 取消或创建进程失败没有启动安装器，旧安装仍完整；清除本次启动意图。
    # UAC cancellation or process-creation failure never started an installer; clear this launch intent.
    if ($nativeMarker -and -not $installerStarted -and $null -ne $rollback) {
        Remove-Item -LiteralPath $nativeMarker -ErrorAction SilentlyContinue
    }
    # 任一替换失败逆序回滚；回滚未完成时保留备份并明确状态未知。
    # Roll back replacements in reverse order; retain backups and report unknown state if rollback fails.
    for ($index = $records.Count - 1; -not $committed -and $index -ge 0; $index--) {
        $record = $records[$index]
        try {
            if ($record.installed) { Remove-Item -LiteralPath $record.target }
            if ($record.backed_up) { Move-Item -LiteralPath $record.backup -Destination $record.target }
        } catch { $rollback = $false }
    }
    if ($committed -or $null -eq $rollback) { Write-Result 'unknown' 'LOCAL_UPDATE_INSTALL' }
    elseif ($rollback) { Write-Result 'failed' $failureCode }
    else { Write-Result 'unknown' 'LOCAL_UPDATE_ROLLBACK' }
    exit 1
} finally {
    if ($updateLock) { $updateLock.Dispose() }
}
