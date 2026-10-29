# scripts/gate_extract_failures.ps1 — read *.serial.log files from
# gate_run_serial.ps1 and print one line per panic, grouped by binary.
#
# Input:  <repo>/test_logs/*.serial.log (produced by gate_run_serial.ps1).
# Output: stdout, one "=== <bin>" header per binary that had panics, and
#         "  <test> @ <location> :: <message>" per panic.
#
# Exit codes:
#   0  the logs were scanned (a clean tree with no panics is a SUCCESS here;
#      this is a report, not a gate)
#   2  <repo>/test_logs does not exist (nothing was scanned — say so instead
#      of printing nothing and looking clean)
param(
    [string]$LogDir = ""
)
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($LogDir)) { $LogDir = Join-Path $root 'test_logs' }
if (-not (Test-Path $LogDir)) {
    Write-Host "gate_extract_failures: no log dir at $LogDir (run gate_run_serial.ps1 first)" -ForegroundColor Red
    exit 2
}
$found = 0
Get-ChildItem $LogDir -Filter '*.serial.log' | ForEach-Object {
    $bin = $_.Name -replace '\.serial\.log$', ''
    $lines = Get-Content $_.FullName
    $out = @()
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($lines[$i] -match 'panicked at') {
            $msg = if ($i + 1 -lt $lines.Count) { $lines[$i + 1] } else { '' }
            $tname = ''
            for ($j = $i; $j -ge 0; $j--) {
                if ($lines[$j] -match '^test (\S+) \.\.\. FAILED') { $tname = $matches[1]; break }
            }
            $loc = ($lines[$i] -replace "^thread '[^']*' \(\d+\) panicked at ", '')
            $out += ("  {0} @ {1} :: {2}" -f $tname, $loc, $msg.Trim())
        }
    }
    if ($out) {
        $script:found += $out.Count
        Write-Output ("=== $bin")
        $out | ForEach-Object { Write-Output $_ }
    }
}
if ($script:found -eq 0) { Write-Output "no panics in $LogDir" }
exit 0
