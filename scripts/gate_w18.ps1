# scripts/gate_w18.ps1 — run the W18 gate sequence in order, stop at the first red.
#
# Why a script and not a list of commands: the gate order is the point. A red
# row must stop the sequence (GATE-README §1), and the only reliable way to
# enforce that while working unattended for 24 h is to make the machine do it.
#
# Every row appends to  logs/w18/gate-<row>.log  and prints one
# "[<row>][OK|FAILED|SKIPPED] <detail>" line. The run stops at the first FAILED
# unless -KeepGoing is passed.
#
# Usage:
#   powershell -NoProfile -File scripts/gate_w18.ps1 -Only check-tests,fmt
#   powershell -NoProfile -File scripts/gate_w18.ps1 -From fmt
#
# The target dir is target-exp-shared: it is the ONE warm dir (756 rlib,
# `check --lib` 2.11 s warm). Do not point this at a cold dir — see docs/STATUS.md §5.
param(
    [string]$TargetDir = "C:/Users/XPENG_USER/Desktop/github/pir/target-exp-shared",
    [string[]]$Only = @(),
    [string]$From = "",
    [switch]$KeepGoing
)
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
$logDir = Join-Path $root 'logs/w18'
if (-not (Test-Path $logDir)) { New-Item -ItemType Directory -Path $logDir | Out-Null }
$env:CARGO_TARGET_DIR = $TargetDir

$rows = @(
    @{ Name = 'check-lib';   Cmd = { cargo check --lib } ;                                         Note = 'lib type-checks' },
    @{ Name = 'check-tests'; Cmd = { cargo check --tests } ;                                       Note = 'test tree type-checks' },
    @{ Name = 'fmt';         Cmd = { cargo fmt --all -- --check } ;                                Note = 'formatting is committed' },
    @{ Name = 'clippy';      Cmd = { cargo clippy --all-targets --all-features -- -D warnings } ;  Note = 'zero warnings' },
    @{ Name = 'nodflt';      Cmd = { cargo check --lib --no-default-features } ;                   Note = 'optional features are optional' },
    @{ Name = 'doctests';    Cmd = { cargo test --doc } ;                                          Note = 'doc examples compile+run' }
)

$seen = $false
$anyFailed = $false
foreach ($row in $rows) {
    $n = $row.Name
    if ($From -and -not $seen) { if ($n -ne $From) { continue } else { $seen = $true } }
    if ($Only.Count -gt 0 -and ($Only -notcontains $n)) { continue }

    $log = Join-Path $logDir ("gate-" + $n + ".log")
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    Push-Location $root
    & $row.Cmd *>&1 | Tee-Object -FilePath $log | Out-Null
    $code = $LASTEXITCODE
    Pop-Location
    $sw.Stop()
    $secs = [int]$sw.Elapsed.TotalSeconds

    # A row's exit code is the whole verdict. Do not scrape the log for
    # "warning:" — clippy without -D would pass while printing them, and this
    # script exists to be able to go red.
    if ($code -eq 0) {
        Write-Output ("[{0}][OK] {1} ({2}s)" -f $n, $row.Note, $secs)
    } else {
        $anyFailed = $true
        $firstErr = (Select-String -Path $log -Pattern '^(error|warning: unused)' -ErrorAction SilentlyContinue | Select-Object -First 3)
        $detail = if ($firstErr) { ($firstErr | ForEach-Object { $_.Line }) -join ' ; ' } else { 'no error line captured' }
        Write-Output ("[{0}][FAILED] exit={1} ({2}s) :: {3}" -f $n, $code, $secs, $detail)
        if (-not $KeepGoing) {
            Write-Output "[gate_w18][STOPPED] first red row: $n — later rows prove nothing until it is green"
            exit 1
        }
    }
}
if ($anyFailed) { exit 1 } else { exit 0 }
