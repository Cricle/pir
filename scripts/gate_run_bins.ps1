# scripts/gate_run_bins.ps1 — run a list of test binaries concurrently (ad-hoc gate helper).
#
# Input:  a text file, one absolute path to a test executable per line
#         (default <repo>/test_bins_gate.txt; blank lines ignored).
# Output: <repo>/test_logs/<bin>.log per binary, plus test_gate_results.csv,
#         plus one "[<bin>] <summary>" line per binary on stdout.
#
# Exit codes:
#   0  every binary ran, printed a libtest summary, and exited 0
#   1  at least one binary failed, timed out, or printed no harness summary
#   2  the input list is missing or names no runnable file
#
# A binary fails when ANY of: its process exit code is non-zero, its output has
# a "FAILED." summary, or it printed no "test result:" line at all (a crash or
# stack overflow must never look green). Timeouts are reported as failures.
#
# Caveat: Start-Job + Stop-Job ends the job's PowerShell, but a hung native
# child can outlive it on Windows. Prefer scripts/test_fast.ps1 for real
# overnight gates; this helper is for an explicit bin list.
param(
    [string]$ListFile = "",
    [int]$Concurrency = 8,
    [int]$TimeoutSec = 120
)
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($ListFile)) { $ListFile = Join-Path $root 'test_bins_gate.txt' }
if (-not (Test-Path $ListFile)) {
    Write-Host "gate_run_bins: input list not found: $ListFile" -ForegroundColor Red
    exit 2
}
$bins = @(Get-Content $ListFile | Where-Object { $_ -and (Test-Path $_) })
if ($bins.Count -eq 0) {
    Write-Host "gate_run_bins: $ListFile names no existing executable" -ForegroundColor Red
    exit 2
}
if (-not (Test-Path (Join-Path $root 'test_logs'))) { New-Item -ItemType Directory -Path (Join-Path $root 'test_logs') | Out-Null }
$logDir = Join-Path $root 'test_logs'
$pending = New-Object System.Collections.Queue
foreach ($b in $bins) { $pending.Enqueue($b) }
$running = @{}
$results = @()
function Get-BinName([string]$p) { [System.IO.Path]::GetFileNameWithoutExtension($p) }

while ($pending.Count -gt 0 -or $running.Count -gt 0) {
    # launch
    while ($pending.Count -gt 0 -and $running.Count -lt $Concurrency) {
        $b = $pending.Dequeue()
        $name = Get-BinName $b
        $job = Start-Job -ScriptBlock {
            param($bin, $logDir, $name)
            $out = & $bin --test-threads=4 2>&1
            $code = $LASTEXITCODE
            $out | Out-File -Encoding utf8 (Join-Path $logDir ($name + '.log'))
            return [pscustomobject]@{ Out = $out; Code = $code }
        } -ArgumentList $b, $logDir, $name
        $running[$name] = @{ Job = $job; Bin = $b; Start = (Get-Date) }
    }
    # poll
    Start-Sleep -Milliseconds 800
    foreach ($name in @() + $running.Keys) {
        $r = $running[$name]
        $job = $r.Job
        if ($job.State -in 'Completed','Failed','Stopped') {
            $received = @(Receive-Job $job 2>$null)
            Remove-Job $job -Force
            $payload = $received | Where-Object { $_ -is [pscustomobject] -and $_.PSObject.Properties['Code'] } | Select-Object -Last 1
            $out = if ($payload) { @($payload.Out) } else { $received }
            $code = if ($payload) { $payload.Code } else { $null }
            $summary = (($out | Select-String -Pattern 'test result:').Line -join ' | ')
            if (-not $summary) { $summary = 'NO_RESULT' }
            $failedLint = @($out | Select-String -Pattern 'FAILED\.').Count
            # Non-zero exit and a missing summary are failures even when the
            # text has no "FAILED." line; exit code is the primary signal.
            $bad = ($failedLint -gt 0) -or ($summary -eq 'NO_RESULT') -or ($null -ne $code -and $code -ne 0) -or ($job.State -eq 'Failed')
            $results += [pscustomobject]@{ Name = $name; State = $job.State; ExitCode = $code; Summary = $summary; Failed = $bad }
            Write-Output ("[$name] $summary (exit=$code)")
            $running.Remove($name) | Out-Null
        } elseif (((Get-Date) - $r.Start).TotalSeconds -gt $TimeoutSec) {
            Stop-Job $job
            $out = Receive-Job $job 2>$null
            Remove-Job $job -Force
            $out | Out-File -Encoding utf8 (Join-Path $logDir ($name + '.timeout.log'))
            $results += [pscustomobject]@{ Name = $name; State = 'TIMEOUT'; ExitCode = $null; Summary = "TIMEOUT_$($TimeoutSec)s"; Failed = $true }
            Write-Output ("[$name] TIMEOUT_${TimeoutSec}s")
            $running.Remove($name) | Out-Null
        }
    }
}
$results | Export-Csv -Path (Join-Path $root 'test_gate_results.csv') -NoTypeInformation -Encoding utf8
$failed = @($results | Where-Object { $_.Failed })
Write-Output ("TOTAL_BINS=" + $results.Count + " FAILED_BINS=" + $failed.Count)
if ($failed.Count -gt 0) {
    $failed | ForEach-Object { Write-Output ("FAIL: " + $_.Name + " :: " + $_.Summary) }
    exit 1
}
exit 0
