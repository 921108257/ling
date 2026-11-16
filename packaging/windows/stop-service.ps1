# 已停止时无操作；其他状态等待明确停止后才升级或卸载。
# A stopped service is a no-op; await a definite stop before upgrade or removal.
$ErrorActionPreference = 'Stop'
$service = Get-Service -Name ling -ErrorAction Stop
if ($service.Status -ne 'Stopped') {
    Stop-Service -Name ling -ErrorAction Stop
    $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30))
}
