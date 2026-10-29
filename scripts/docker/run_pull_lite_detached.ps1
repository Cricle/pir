#requires -Version 5.1
# Detached wrapper for pull_lite_all.ps1. Launched via WMI Win32_Process.Create
# so it survives the ZCode shell/task lifetime. Writes console output to
# logs/lane-docker-lite/console-pull.log and a heartbeat/pid file.
$ErrorActionPreference = 'Continue'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$logDir = Join-Path $root 'logs\lane-docker-lite'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$log = Join-Path $logDir 'console-pull.log'
$pidFile = Join-Path $logDir 'runner.pid'
$started = Get-Date
"$PID $($started.ToString('o'))" | Set-Content -Path $pidFile -Encoding ASCII
"[{0:yyyy-MM-dd HH:mm:ss}] detached runner start pid=$PID" | Add-Content -Path $log -Encoding UTF8
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'pull_lite_all.ps1') `
    -MaxParallel 2 -MaxRetries 8 -RetryDelaySec 30 -AttemptTimeoutMin 60 *>> $log
$code = $LASTEXITCODE
"[{0:yyyy-MM-dd HH:mm:ss}] detached runner exit code=$code" -f (Get-Date) | Add-Content -Path $log -Encoding UTF8
Remove-Item -Force $pidFile -ErrorAction SilentlyContinue
exit $code
