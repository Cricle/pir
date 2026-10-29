# scripts/gate_run_serial.ps1 — re-run a list of test binaries ONE AT A TIME.
#
# Input:  a text file, one absolute path to a test executable per line
#         (default <repo>/test_gate_failed.txt; blank lines ignored).
# Output: <repo>/test_logs/<bin>.serial.log per binary, plus one
#         "[<bin>][OK|FAILED|TIMEOUT_<n>] <summary>" line per binary.
#
# Exit codes:
#   0  every binary printed a libtest summary and exited 0
#   1  at least one binary failed, timed out, or printed no harness summary
#   2  the input list is missing or names no runnable file
#
# Why serial: this is the confirmation leg for a busy-gate failure list — a
# suite that passes alone was load-flaky, not broken. The distinction only
# holds if this script can go red, so a crash, a timeout and a missing summary
# are all failures here too.
param(
    [string]$ListFile = "",
    [int]$TimeoutSec = 300
)
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($ListFile)) { $ListFile = Join-Path $root 'test_gate_failed.txt' }
$logDir = Join-Path $root 'test_logs'
if (-not (Test-Path $logDir)) { New-Item -ItemType Directory -Path $logDir | Out-Null }
if (-not (Test-Path $ListFile)) {
    Write-Host "gate_run_serial: input list not found: $ListFile" -ForegroundColor Red
    exit 2
}
$bins = @(Get-Content $ListFile | Where-Object { $_ -and (Test-Path $_) })
if ($bins.Count -eq 0) {
    Write-Host "gate_run_serial: $ListFile names no existing executable" -ForegroundColor Red
    exit 2
}
$anyFailed = $false
foreach ($b in $bins) {
    $name = [System.IO.Path]::GetFileNameWithoutExtension($b)
    $job = Start-Job -ScriptBlock {
        param($bin, $logDir, $name, $root)
        Set-Location $root
        $out = & $bin 2>&1
        $code = $LASTEXITCODE
        $out | Out-File -Encoding utf8 (Join-Path $logDir ($name + '.serial.log'))
        return [pscustomobject]@{ Out = $out; Code = $code }
    } -ArgumentList $b, $logDir, $name, $root
    if (Wait-Job $job -Timeout $TimeoutSec) {
        $received = @(Receive-Job $job 2>$null)
        Remove-Job $job -Force
        $payload = $received | Where-Object { $_ -is [pscustomobject] -and $_.PSObject.Properties['Code'] } | Select-Object -Last 1
        $out = if ($payload) { @($payload.Out) } else { $received }
        $code = if ($payload) { $payload.Code } else { $null }
        $summary = (($out | Select-String -Pattern 'test result:').Line -join ' | ')
        if (-not $summary) { $summary = 'NO_RESULT' }
        # A crashed binary can produce no harness summary at all; NO_RESULT must
        # never be reported as OK (a stack overflow / early abort would otherwise
        # be silently green). Exit code is checked for the same reason.
        $bad = ($summary -eq 'NO_RESULT') -or (@($out | Select-String -Pattern 'FAILED\.').Count -gt 0) -or ($null -ne $code -and $code -ne 0)
        if ($bad) { $anyFailed = $true }
        $verdict = if ($bad) { 'FAILED' } else { 'OK' }
        Write-Output ("[$name][$verdict] $summary (exit=$code)")
    } else {
        Stop-Job $job
        Remove-Job $job -Force
        $anyFailed = $true
        Write-Output ("[$name][TIMEOUT_$TimeoutSec]")
    }
}
if ($anyFailed) { exit 1 } else { exit 0 }
