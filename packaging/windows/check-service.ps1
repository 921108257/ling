# 确认服务已运行且没有立即崩溃；Wait for running status and reject immediate failure.
$ErrorActionPreference = 'Stop'
$service = Get-Service -Name ling -ErrorAction Stop
$service.WaitForStatus([System.ServiceProcess.ServiceControllerStatus]::Running, [TimeSpan]::FromSeconds(15))
Start-Sleep -Seconds 2
if ((Get-Service -Name ling).Status -ne 'Running') { throw 'ling service did not remain running' }
