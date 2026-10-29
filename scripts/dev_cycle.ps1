# scripts/dev_cycle.ps1 — the one-command local debug loop.
#
# Steps, in this order:
#   fmt     cargo fmt --all -- --check
#   clippy  cargo clippy --lib --bins -- -D warnings
#   build   cargo build --bin pir
#   test    scripts/test_select.ps1 -Since <min>   (affected subset)
#
# Why these commands:
#   * clippy is deliberately `--lib --bins`, not `--all-targets`: --all-targets
#     checks all ~280 test targets and turns a ~35 s edit check into minutes.
#     The full static gate remains scripts/acc_final.ps1 (foreman).
#   * build is `--bin pir`, the artifact the local loop actually runs.
#   * test defaults to test_select.ps1, which maps the changed files to the
#     affected suites plus the `cli` smoke target; `-Tests` bypasses the
#     mapping for an explicit cargo target list (test_fast.ps1 -Tests).
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/dev_cycle.ps1
#   ... -Only fmt,clippy            # run a subset of the steps
#   ... -Only test                  # fastest inner loop (measured 48.5 s for a
#                                   # one-file edit; cargo test builds the bin)
#   ... -NoTest                     # static part only
#   ... -Tests g_ai,g_tui           # exact cargo test targets, no mapping
#   ... -Since 60                   # changed-file window for test_select
#   ... -Files src/tui/renderers.rs # explicit changed files for test_select
#   ... -TargetDir target-lane-d    # build dir (default: CARGO_TARGET_DIR or target-alt)
#   ... -Jobs 8                     # CARGO_BUILD_JOBS for every cargo step
#   ... -Concurrency 4 -Threads 4   # forwarded to test_select/test_fast
#   ... -List                       # print the plan, run nothing
#
# Semantics:
#   * Each step is timed with a Stopwatch and printed as one line.
#   * The first failure stops the run; the exit code is that step's exit code
#     (0 only when every executed step passed). Nothing is reported green
#     after a skipped step.
#   * A step at or over -SlowSec (default 60) prints a WARNING with a concrete
#     acceleration suggestion for that step.
#   * Per-step stdout/stderr go to logs/dev-cycle/<stamp>/<step>.log(.err);
#     summary.txt and last-run.json (UTF-8, no BOM) record the same numbers.
#   * Never touches the foreman's target/: the default is target-alt, exactly
#     like the other local scripts, and CARGO_TARGET_DIR always wins.
param(
    [string[]]$Only = @(),
    [switch]$NoTest,
    [string[]]$Tests = @(),
    [string[]]$Files = @(),
    [int]$Since = 30,
    [string]$TargetDir = "",
    [int]$Jobs = 0,
    [int]$Concurrency = 0,
    [int]$Threads = 0,
    [int]$SlowSec = 60,
    [switch]$List
)

$ErrorActionPreference = "Stop"
# `powershell -File script.ps1 -Only fmt,clippy` hands the list over as ONE
# string (-File mode does not parse PS array literals); split it back apart.
$Only = @($Only | ForEach-Object { $_ -split ',' } | Where-Object { $_ -ne '' } | ForEach-Object { $_.Trim().ToLowerInvariant() })
$Tests = @($Tests | ForEach-Object { $_ -split ',' } | Where-Object { $_ -ne '' } | ForEach-Object { $_.Trim() })
$Files = @($Files | ForEach-Object { $_ -split ',' } | Where-Object { $_ -ne '' } | ForEach-Object { $_.Trim() })

if ($SlowSec -lt 1) { Write-Host "[dev_cycle] -SlowSec must be >= 1"; exit 2 }

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if ([string]::IsNullOrWhiteSpace($TargetDir)) {
    if ($env:CARGO_TARGET_DIR) { $TargetDir = $env:CARGO_TARGET_DIR }
    else { $TargetDir = Join-Path $root "target-alt" }
}
$env:CARGO_TARGET_DIR = $TargetDir
# Redirected output is not a TTY; pin color off so logs stay diffable.
$env:CARGO_TERM_COLOR = "never"
if ($Jobs -gt 0) { $env:CARGO_BUILD_JOBS = "$Jobs" }
if (-not $env:RUST_MIN_STACK) { $env:RUST_MIN_STACK = "8388608" }

$cargoExe = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargoExe) { Write-Host "[dev_cycle] cargo is not on PATH"; exit 127 }
$psExe = (Get-Command powershell -ErrorAction SilentlyContinue).Source
if (-not $psExe) { Write-Host "[dev_cycle] powershell is not on PATH"; exit 127 }

# --- step plan -------------------------------------------------------------
if ($Tests.Count -gt 0 -and $Files.Count -gt 0) {
    Write-Host "[dev_cycle] -Tests and -Files are mutually exclusive"; exit 2
}
$testArgs = @()
if ($Tests.Count -gt 0) {
    $testArgs = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
        (Join-Path $PSScriptRoot "test_fast.ps1"), "-Tests", ($Tests -join ","))
} elseif ($Files.Count -gt 0) {
    $testArgs = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
        (Join-Path $PSScriptRoot "test_select.ps1"), "-Files", ($Files -join ","))
} else {
    $testArgs = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
        (Join-Path $PSScriptRoot "test_select.ps1"), "-Since", "$Since")
}
if ($Concurrency -gt 0) { $testArgs += @("-Concurrency", "$Concurrency") }
if ($Threads -gt 0) { $testArgs += @("-Threads", "$Threads") }

$steps = @(
    [pscustomobject]@{
        Name       = "fmt"
        File       = $cargoExe
        Args       = @("fmt", "--all", "--", "--check")
        Suggestion = "rustfmt parses every .rs file; 5-20 s is normal on this tree. If this is slow, check the log for a diff first (a red fmt is a content problem, not a speed problem) and avoid changing rustfmt.toml."
    },
    [pscustomobject]@{
        Name       = "clippy"
        File       = $cargoExe
        Args       = @("clippy", "--lib", "--bins", "--", "-D", "warnings")
        Suggestion = "A cold clippy cache re-checks every dependency (measured 6m37s once). Keep one warm CARGO_TARGET_DIR per lane; --lib --bins already skips the ~280 test targets. If this stays slow, check that no other cargo process is writing the same target dir."
    },
    [pscustomobject]@{
        Name       = "build"
        File       = $cargoExe
        Args       = @("build", "--bin", "pir")
        Suggestion = "One src edit forces the whole pir-sdk crate through codegen (~20-35 s; debug=0 and rust-lld are already set). When the test step includes the cli smoke target, cargo test relinks pir.exe anyway: measured 48.5 s for -Only test vs 94.9 s for clippy+build+test on a one-file edit. Use -Only clippy,test for the inner loop; keep build for -NoTest runs or when no selected test uses the binary."
    },
    [pscustomobject]@{
        Name       = "test"
        File       = $psExe
        Args       = $testArgs
        Suggestion = "test_select builds the affected targets only. Narrow it with a smaller -Since, avoid -Broad/-All, or pass -Tests <one suite>. The remaining fixed cost is linking/loading ~300 MB test binaries; the empty lib harness is skipped."
    }
)

if ($Only.Count -gt 0) {
    $known = @("fmt", "clippy", "build", "test")
    $bad = @($Only | Where-Object { $known -notcontains $_ })
    if ($bad.Count -gt 0) {
        Write-Host ("[dev_cycle] unknown -Only step(s): {0}; known: {1}" -f ($bad -join ", "), ($known -join ", "))
        exit 2
    }
    $steps = @($steps | Where-Object { $Only -contains $_.Name })
}
if ($NoTest) {
    if ($Tests.Count -gt 0) { Write-Host "[dev_cycle] -Tests and -NoTest are mutually exclusive"; exit 2 }
    if ($Only -contains "test") { Write-Host "[dev_cycle] -Only test and -NoTest are mutually exclusive"; exit 2 }
    $steps = @($steps | Where-Object { $_.Name -ne "test" })
}
if ($steps.Count -eq 0) { Write-Host "[dev_cycle] no steps selected"; exit 2 }

function Format-Arg {
    param([string]$Arg)
    if ($Arg -match '\s') { return '"' + $Arg + '"' }
    return $Arg
}
function Format-Cmd {
    param($Step)
    $leaf = Split-Path -Leaf $Step.File
    return ($leaf + " " + (($Step.Args | ForEach-Object { Format-Arg $_ }) -join " "))
}

Write-Host ("[dev_cycle] target dir: {0}" -f $env:CARGO_TARGET_DIR)
Write-Host ("[dev_cycle] steps: {0} (slow warning at {1}s)" -f (($steps | ForEach-Object { $_.Name }) -join " -> "), $SlowSec)
foreach ($s in $steps) { Write-Host ("  {0,-7} {1}" -f $s.Name, (Format-Cmd $s)) }
if ($List) { exit 0 }

# --- run -------------------------------------------------------------------
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$outDir = Join-Path $root ("logs/dev-cycle/" + $stamp)
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$results = New-Object System.Collections.ArrayList
$firstFailure = 0
$total = [System.Diagnostics.Stopwatch]::StartNew()
foreach ($step in $steps) {
    $stdout = Join-Path $outDir ($step.Name + ".log")
    $stderr = Join-Path $outDir ($step.Name + ".err")
    Write-Host ""
    Write-Host ("== {0}: {1}" -f $step.Name, (Format-Cmd $step))
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $code = 127
    try {
        $argList = @($step.Args | ForEach-Object { Format-Arg $_ })
        $proc = Start-Process -FilePath $step.File -ArgumentList $argList `
            -NoNewWindow -PassThru -Wait `
            -RedirectStandardOutput $stdout -RedirectStandardError $stderr `
            -ErrorAction Stop
        $code = $proc.ExitCode
    } catch {
        "could not start $($step.File): $($_.Exception.Message)" |
            Set-Content -Path $stderr -Encoding utf8
    }
    $sw.Stop()
    $secs = [Math]::Round($sw.Elapsed.TotalSeconds, 1)
    [void]$results.Add([pscustomobject]@{
        name = $step.Name; rc = $code; secs = $secs
        log = $stdout; err = $stderr
    })
    if ($code -eq 0) {
        Write-Host ("   ok in {0:N1}s  (log: {1})" -f $secs, $stdout)
    } else {
        Write-Host ("   FAILED rc={0} in {1:N1}s  (log: {2})" -f $code, $secs, $stdout)
        foreach ($tail in @($stdout, $stderr)) {
            if ((Test-Path $tail) -and (Get-Item $tail).Length -gt 0) {
                Write-Host ("   --- tail {0} ---" -f (Split-Path -Leaf $tail))
                Get-Content $tail -Tail 25 | ForEach-Object { Write-Host ("   " + $_) }
            }
        }
    }
    if ($secs -ge $SlowSec) {
        Write-Host ("   WARNING: {0} took {1:N1}s (>= {2}s). {3}" -f $step.Name, $secs, $SlowSec, $step.Suggestion)
    }
    if ($code -ne 0) { $firstFailure = $code; break }
}
$total.Stop()

Write-Host ""
Write-Host "=== dev cycle summary ==="
foreach ($r in $results) {
    $state = if ($r.rc -eq 0) { "ok  " } else { "FAIL" }
    Write-Host ("  {0,-7} {1} {2,7:N1}s  {3}" -f $r.name, $state, $r.secs, $(if ($r.rc -ne 0) { "rc=" + $r.rc } else { "" }))
}
$executed = @($results | ForEach-Object { $_.name })
$skipped = @($steps | Where-Object { $executed -notcontains $_.Name } | ForEach-Object { $_.Name })
if ($skipped.Count -gt 0) { Write-Host ("  skipped: " + ($skipped -join ", ")) }
Write-Host ("  total:   {0:N1}s" -f $total.Elapsed.TotalSeconds)
if ($firstFailure -eq 0) { Write-Host ("dev cycle GREEN ({0} step(s) executed)" -f $results.Count) } else { Write-Host ("dev cycle RED (first failure rc={0})" -f $firstFailure) }

# --- persist summary -------------------------------------------------------
$summaryLines = @($results | ForEach-Object { "{0}: RC={1} ({2:N1}s) log={3}" -f $_.name, $_.rc, $_.secs, $_.log })
$summaryLines += ("total: {0:N1}s  targetDir={1}" -f $total.Elapsed.TotalSeconds, $env:CARGO_TARGET_DIR)
[IO.File]::WriteAllText((Join-Path $outDir "summary.txt"), (($summaryLines -join "`r`n") + "`r`n"), (New-Object System.Text.UTF8Encoding($false)))
$statePath = Join-Path $root "logs/dev-cycle/last-run.json"
$state = [pscustomobject]@{
    when      = (Get-Date).ToString("s")
    targetDir = $env:CARGO_TARGET_DIR
    jobs      = $Jobs
    totalSecs = [Math]::Round($total.Elapsed.TotalSeconds, 1)
    ok        = ($firstFailure -eq 0)
    exitCode  = $firstFailure
    steps     = @($results | ForEach-Object {
            [pscustomobject]@{ name = $_.name; rc = $_.rc; secs = $_.secs; log = $_.log }
        })
    logDir    = $outDir
}
[IO.File]::WriteAllText($statePath, ($state | ConvertTo-Json -Depth 5), (New-Object System.Text.UTF8Encoding($false)))
Write-Host ("summary json: {0}" -f $statePath)

exit $firstFailure
