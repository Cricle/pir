# scripts/acc_final.ps1 — the frozen-tree acceptance batch (foreman gate).
#
# Runs the three static gates the wave report records, in order, each into its
# own log under logs/acc-final/<stamp>/, then prints a one-line-per-gate
# summary. The full suite run (scripts/test_fast.ps1) is deliberately NOT part
# of this script: it is launched separately so the gates can be read before the
# long run starts.
#
# The gates are:
#   1. fmt     cargo fmt --all -- --check            (formatting is a gate)
#   2. clippy  cargo clippy --all-targets --all-features -- -D warnings
#   3. build   cargo build --all-targets --all-features
#
# Usage:
#   powershell -NoProfile -File scripts/acc_final.ps1
#   powershell -NoProfile -File scripts/acc_final.ps1 -TargetDir D:\shared-target
#
# Semantics:
#   * Every gate runs even after an earlier one failed, so one invocation
#     collects all the evidence; the EXIT CODE is the first failing gate's.
#   * Exit 0 only when all three gates returned 0.
#   * Logs are UTF-8 (no BOM) with stdout and stderr split into <gate>.log and
#     <gate>.log.err: `cargo` narrates on stderr, so a single-stream redirect
#     loses the errors that matter (and PowerShell 5.1's `*>` writes UTF-16LE,
#     which grep/tail cannot read).
#   * summary.txt records RC and wall seconds per gate so a later report can
#     cite them without rerunning.
param(
    [string]$TargetDir = ""
)

$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
if ([string]::IsNullOrWhiteSpace($TargetDir)) {
    # Repo discipline: lane/eval builds live in target-alt; `target/` belongs to
    # the eval harness. Default there instead of an absolute path baked into
    # this file (the previous default was this machine's checkout path).
    $TargetDir = Join-Path $root "target-alt"
}
$env:CARGO_TARGET_DIR = $TargetDir
$env:CARGO_INCREMENTAL = "0"
$env:RUST_MIN_STACK = "8388608"

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$out = Join-Path $root "logs/acc-final/$stamp"
New-Item -ItemType Directory -Force -Path $out | Out-Null

$gates = @(
    @{ name = "fmt";   args = @("fmt", "--all", "--", "--check") },
    @{ name = "clippy"; args = @("clippy", "--all-targets", "--all-features", "--", "-D", "warnings") },
    @{ name = "build"; args = @("build", "--all-targets", "--all-features") }
)

$summary = @()
$firstFailure = 0
foreach ($gate in $gates) {
    $log = Join-Path $out ("{0}.log" -f $gate.name)
    $errLog = $log + ".err"
    $started = Get-Date
    Write-Host ("== {0}: cargo {1}" -f $gate.name, ($gate.args -join " "))
    # Start-Process (not `& cargo ... *> $log`): it captures the real exit code
    # and writes plain UTF-8, and it keeps stderr in its own file. `cargo` is
    # resolved from PATH; a missing cargo is reported as a gate failure instead
    # of leaving $LASTEXITCODE at the previous gate's value.
    $code = 127
    try {
        $proc = Start-Process -FilePath "cargo" -ArgumentList @($gate.args) `
            -NoNewWindow -PassThru -Wait `
            -RedirectStandardOutput $log -RedirectStandardError $errLog `
            -ErrorAction Stop
        $code = $proc.ExitCode
    } catch {
        "cargo could not be started: $($_.Exception.Message)" |
            Set-Content -Path $errLog -Encoding utf8
        Write-Host ("   ERROR: cargo could not be started: {0}" -f $_.Exception.Message)
    }
    $secs = [int]((Get-Date) - $started).TotalSeconds
    $summary += "{0}: RC={1} ({2}s) log={3}" -f $gate.name, $code, $secs, $log
    if ($code -ne 0 -and $firstFailure -eq 0) {
        $firstFailure = $code
        Write-Host ("   FAILED — see {0} (stderr: {1})" -f $log, $errLog)
    } else {
        Write-Host ("   RC={0} ({1}s)" -f $code, $secs)
    }
}

$summary | Set-Content (Join-Path $out "summary.txt") -Encoding utf8
Write-Host ""
Write-Host "== acc-final gates =="
$summary | ForEach-Object { Write-Host $_ }
Write-Host ""
if ($firstFailure -eq 0) {
    Write-Host("all three static gates green; artifacts in {0}" -f $out)
} else {
    Write-Host("STATIC GATES RED (first failure RC={0}); artifacts in {1}" -f $firstFailure, $out)
}
Write-Host ("next: powershell -NoProfile -File scripts/test_fast.ps1   (log it; it is the long one)")
exit $firstFailure
