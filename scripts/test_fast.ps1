# scripts/test_fast.ps1 — parallel test-binary runner.
#
# `cargo test --all-targets` runs each test binary sequentially, so the run
# phase costs the SUM of every suite (~3-4 min here, dominated by a handful
# of 10-15s suites). This script builds once (`cargo test --no-run`), then
# executes the same test binaries CONCURRENTLY, which cuts the wall clock to
# roughly the slowest single suite. Same binaries, same tests, no coverage
# change; output is captured per suite and aggregated.
#
# Usage:
#   powershell -NoProfile -File scripts/test_fast.ps1                 # all suites
#   powershell -NoProfile -File scripts/test_fast.ps1 -Concurrency 4  # cap
#   powershell -NoProfile -File scripts/test_fast.ps1 -Filter g_ai    # run only matching suites
#   powershell -NoProfile -File scripts/test_fast.ps1 -Tests g_ai,node_shims   # exact targets
#   powershell -NoProfile -File scripts/test_fast.ps1 -RetryFailed    # last run's failures
#   powershell -NoProfile -File scripts/test_fast.ps1 -List           # print the plan, don't run
#
# Notes:
#   - Honours CARGO_TARGET_DIR; defaults to target-alt (repo discipline:
#     the default `target/` is locked by the eval harness pir.exe builds).
#   - Doctests are not covered (rusdoc runs them itself); run
#     `cargo test --doc` when touching doc examples.
#   - Concurrency oversubscription can make real-timer tests touchy; drop
#     -Concurrency if a timing test flakes.
#   - -NoBuild skips the cargo build and reuses the latest binaries (fails
#     if none exist yet); use it when only re-running, not after editing.
#     Combine with -Tests to run a subset from an already-built target dir.
#   - -Tests takes cargo target names (the file stem of tests/<name>.rs, or
#     "pir" for the lib unit tests) and passes `--test <name>` through to
#     cargo, so only those binaries are (re)built and linked. This is the
#     lever that keeps a one-suite edit from re-linking all ~280 cargo test
#     targets (as of 2026-10-29: 281 integration targets = 9 merged g_*
#     harnesses + 272 standalone tests/*.rs; the lib unit target has no
#     tests and the bin unit harness is a separate cargo target).
#   - CI does not use this script: it runs `cargo nextest run --profile ci`
#     (.config/nextest.toml) as one Linux+Windows matrix job, in eight
#     sequential groups per OS. This
#     script exists for the local loop and for re-running a failing subset.
#   - -RetryFailed re-runs only the suites that failed last time, reading
#     `logs/test_fast/last-run.json` (written by every run, including this
#     one). No argument means "the previous run".
#   - Every run writes logs/test_fast/last-run.json (UTF-8, no BOM): per-suite
#     pass/fail/exit code, wall seconds, the flaky targets and the failing
#     target names. `failed` counts only suites that failed BOTH the parallel
#     pass and their serial retry; `failedParallel` keeps the raw parallel
#     count so a flaky run is never reported as a red run (or vice versa).
#   - Exit codes: 0 only when every suite passed (a load-flaky suite that
#     passes alone is a pass, and is reported as such); 1 when a real failure,
#     a failed build, or a run that executed zero tests occurred.
#   - -Filter narrows which test binaries are EXECUTED: a case-insensitive
#     substring/glob match on the cargo target name ("g_ai" runs g_ai; "g_*"
#     runs every g_ suite). It does not shrink the BUILD — cargo has no
#     name-filtered build — so use -Tests when link time is the problem.
#   - A test binary that never terminates is not bounded by this script (it has
#     no per-suite timeout); Ctrl+C stops the run. Use the foreman's gate
#     scripts when an unbounded hang must fail rather than wait.
param(
    [int]$Concurrency = 0,
    [int]$Threads = 0,
    [string]$Filter = "",
    [string[]]$Tests = @(),
    [switch]$NoBuild,
    [switch]$RetryFailed,
    [switch]$List
)

$ErrorActionPreference = "Stop"
# `powershell -File script.ps1 -Tests a,b` hands the list over as ONE string
# (‑File mode does not parse PS array literals), so split it back apart.
$Tests = @($Tests | ForEach-Object { $_ -split ',' } | Where-Object { $_ -ne '' } | ForEach-Object { $_.Trim() })
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
if (-not $env:CARGO_TARGET_DIR) {
    $env:CARGO_TARGET_DIR = (Join-Path $root "target-alt")
}
# Deep in-process nesting (the subagent builtin runs a child agent loop on the
# same stack chain) exceeds the 2 MiB default test-thread stack in debug
# builds; libtest honors RUST_MIN_STACK for its thread spawns. 8 MiB gives
# headroom without a real cost (untouched pages are never committed).
if (-not $env:RUST_MIN_STACK) {
    $env:RUST_MIN_STACK = "8388608"
}
$stateDir = Join-Path $root "logs\test_fast"
$statePath = Join-Path $stateDir "last-run.json"
$deps = Join-Path $env:CARGO_TARGET_DIR "debug\deps"

# -RetryFailed: pull the failing target list from the previous run's state.
if ($RetryFailed) {
    if (-not (Test-Path $statePath)) {
        Write-Error "[test_fast] -RetryFailed: no previous run state at $statePath"
        exit 1
    }
    $prev = Get-Content $statePath -Raw | ConvertFrom-Json
    $prevFailed = @($prev.failedTargets | Where-Object { $_ -and ($_ -is [string]) })
    if ($prevFailed.Count -eq 0) {
        Write-Host "[test_fast] -RetryFailed: previous run had no real failures; nothing to do."
        exit 0
    }
    Write-Host ("[test_fast] -RetryFailed: re-running {0} suite(s) from {1}" -f $prevFailed.Count, $statePath)
    $Tests = @($Tests + $prevFailed) | Sort-Object -Unique
}

# Resolve a cargo target name ("g_ai", "node_shims", "pir") to the newest
# matching test executable already present in the target dir. Used by
# -NoBuild, where cargo is not asked to enumerate the artifacts.
function Resolve-TestExe {
    param([string]$Name)
    $hit = Get-ChildItem -Path (Join-Path $deps ($Name + "-*.exe")) -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($hit) { return $hit.FullName }
    return $null
}

# Collapse a target dir's per-fingerprint duplicates to the newest
# `<target>-<16-hex>.exe` per target. `pir.exe` (the binary, not a harness) and
# any other unhashed artifact are dropped. Returns full paths, name-sorted.
# Shared by -List and -NoBuild so both report the same plan.
function Get-TestExeSet {
    param($Paths)
    # -NoBuild scans target/debug/deps, which keeps stale harnesses from
    # deleted tests (e.g. an old `ci_standalone.exe`). Running those used to
    # hang the whole sweep after the current suites had finished. Keep only
    # targets that still have a source file under tests/ (plus the lib target).
    $currentTargets = @(Get-ChildItem -Path (Join-Path $root "tests\*.rs") -ErrorAction SilentlyContinue |
        ForEach-Object { $_.BaseName })
    $byTarget = @{}
    foreach ($e in @($Paths | Where-Object { $_ })) {
        $leaf = Split-Path -Leaf $e
        if ($leaf -notmatch '-[0-9a-f]{16}\.exe$') { continue }
        $t = $leaf -replace '-[0-9a-f]{16}\.exe$', ''
        # `pir-<hash>.exe` in deps is either the real CLI artifact or the
        # (currently empty) lib unit harness; neither is a suite for a
        # full-sweep run, and running the CLI with --test-threads hangs it.
        if ($t -eq "pir") { continue }
        if ($currentTargets -notcontains $t) { continue }
        $item = Get-Item $e -ErrorAction SilentlyContinue
        if (-not $item) { continue }
        if (-not $byTarget.ContainsKey($t) -or $item.LastWriteTime -gt $byTarget[$t].LastWriteTime) {
            $byTarget[$t] = $item
        }
    }
    return @($byTarget.Values | Sort-Object Name | ForEach-Object { $_.FullName })
}

# -Filter: case-insensitive substring or glob match on the cargo target name.
# An empty plan in gives an empty plan out (never a pipeline binding error on
# $null, which an empty -List run produces).
function Select-ByFilter {
    param($Paths, [string]$Pattern)
    $list = @($Paths | Where-Object { $_ })
    if ($Pattern -eq "") { return $list }
    return @($list | Where-Object {
            $t = (Split-Path -Leaf $_) -replace '-[0-9a-f]{16}\.exe$', '' -replace '\.exe$', ''
            ($t.IndexOf($Pattern, [StringComparison]::OrdinalIgnoreCase) -ge 0) -or ($t -like $Pattern)
        })
}

$phase = [System.Diagnostics.Stopwatch]::StartNew()
$exes = @()
if ($Tests.Count -gt 0) {
    Write-Host ("[test_fast] target subset: {0}" -f ($Tests -join ", "))
}
if ($List) {
    # -List is a dry run: it must work before a build exists, so a missing deps
    # dir is an empty plan, never a Get-ChildItem error.
    $present = @()
    if (Test-Path $deps) {
        $present = @(Get-ChildItem -Path (Join-Path $deps "*.exe") -ErrorAction SilentlyContinue |
            ForEach-Object { $_.FullName })
    }
    if ($Tests.Count -gt 0) {
        foreach ($t in $Tests) {
            $e = Resolve-TestExe $t
            if ($e) { Write-Host ("  {0,-28} {1}" -f $t, $e) } else { Write-Host ("  {0,-28} (not built yet)" -f $t) }
        }
    } else {
        $plan = Select-ByFilter (Get-TestExeSet $present) $Filter
        $suffix = if ($Filter -ne "") { " matching '$Filter'" } else { "" }
        Write-Host ("[test_fast] -List: {0} test binary/target(s){1} in {2}" -f $plan.Count, $suffix, $deps)
        foreach ($p in $plan) { Write-Host ("  " + (Split-Path -Leaf $p)) }
        if ($present.Count -eq 0) {
            Write-Host ("[test_fast] -List: nothing built in {0} yet (run once without -List)" -f $deps)
        }
    }
    exit 0
}
if (-not $NoBuild) {
    Write-Host "[test_fast] building test binaries (cargo test --no-run)..."
    $buildArgs = @("test", "--no-run", "--message-format=json")
    # `--lib` for the crate's own unit tests (its test target is named after
    # the lib, and cargo rejects `--test pir` for it on some versions).
    foreach ($t in $Tests) {
        if ($t -eq "pir") { $buildArgs += "--lib" } else { $buildArgs += @("--test", $t) }
    }
    # Native stderr (cargo progress) trips $ErrorActionPreference=Stop in PS 5.1;
    # relax it around the build and let the JSON filter drop non-JSON lines.
    $prevEAP = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    # Keep the last non-artifact lines (cargo/rustc diagnostics) so a failed
    # build explains itself. Before this, a mid-edit tree produced only
    # "BUILD FAILED (cargo exit 101)" with the actual error discarded.
    $buildHead = New-Object System.Collections.ArrayList
    $buildTail = New-Object System.Collections.ArrayList
    $buildLineCount = 0
    function Add-BuildLine {
        param([string]$Text)
        $script:buildLineCount++
        if ($buildHead.Count -lt 5) { [void]$buildHead.Add($Text); return }
        if ($buildTail.Count -ge 30) { $buildTail.RemoveAt(0) }
        [void]$buildTail.Add($Text)
    }
    cargo @buildArgs 2>&1 | ForEach-Object {
        # PS 5.1 turns native stderr lines into ErrorRecord objects when 2>&1
        # is used; stringify first or the diagnostics are silently dropped.
        $line = if ($_ -is [string]) { $_ } else { $_.ToString() }
        # Compiler diagnostics are JSON too; keep their rendered text.
        if ($line -match '"reason":"compiler-message"') {
            try {
                $m = $line | ConvertFrom-Json
                if ($m.message.rendered) {
                    foreach ($msgLine in ($m.message.rendered -split "`r?`n")) { Add-BuildLine $msgLine }
                }
            } catch { }
            return
        }
        # Only artifact lines can carry an executable; parsing every JSON line
        # with ConvertFrom-Json costs seconds on a 280-target build.
        if ($line -notmatch '"executable"') {
            if ($line -notmatch '^\s*\{') { Add-BuildLine $line }
            return
        }
        try { $m = $line | ConvertFrom-Json } catch { return }
        if ($m.reason -eq 'compiler-artifact' -and $m.profile.test -and $m.executable) {
            $exes += $m.executable
        }
    }
    # A failed build still streams the artifacts compiled BEFORE the failure;
    # without this check the runner would silently execute a PARTIAL suite and
    # report it as the full one (measured: a mid-edit tree yielded 117 of ~189
    # binaries and a "3282 -> 2268 passed" drop that looked like lost coverage).
    $buildExit = $LASTEXITCODE
    $ErrorActionPreference = $prevEAP
    if ($buildExit -ne 0) {
        Write-Host ""
        Write-Host "[test_fast] BUILD FAILED (cargo exit $buildExit); refusing to run a partial suite"
        if ($buildHead.Count -gt 0) {
            Write-Host "[test_fast] build output (first lines / last lines):"
            foreach ($line in $buildHead) { Write-Host ("  " + $line) }
            $shown = $buildHead.Count + $buildTail.Count
            if ($buildLineCount -gt $shown) {
                Write-Host ("  ... {0} line(s) omitted ..." -f ($buildLineCount - $shown))
            }
            foreach ($line in $buildTail) { Write-Host ("  " + $line) }
        }
        exit 1
    }
} else {
    Write-Host "[test_fast] -NoBuild: reusing binaries already in the target dir..."
    $missing = @()
    if ($Tests.Count -gt 0) {
        foreach ($t in $Tests) {
            $e = Resolve-TestExe $t
            if ($e) { $exes += $e } else { $missing += $t }
        }
        if ($missing.Count -gt 0) {
            # A requested target with no binary is not "skipped": running the
            # rest would report a green run for a suite that never executed.
            Write-Host ""
            Write-Host ("[test_fast] -NoBuild: no executable for {0} in {1}" -f ($missing -join ", "), $deps)
            Write-Host "[test_fast] build them first (drop -NoBuild), or drop those names from -Tests"
            exit 1
        }
    } else {
        $all = @()
        if (Test-Path $deps) {
            $all = @(Get-ChildItem -Path (Join-Path $deps "*.exe") -ErrorAction SilentlyContinue |
                ForEach-Object { $_.FullName })
        }
        # A target dir accumulates one executable per fingerprint generation
        # (RUSTFLAGS/profile changes leave the old ones behind), and the plain
        # `pir.exe` next to them is the binary, not a test harness. Keep only
        # the newest `<target>-<hash>.exe` per target so each suite runs once.
        $exes = Get-TestExeSet $all
    }
}
$exes = @(Select-ByFilter ($exes | Sort-Object -Unique) $Filter)
if ($exes.Count -eq 0 -and $Filter -ne "") {
    Write-Host ("[test_fast] -Filter '{0}' matched no test target in {1}" -f $Filter, $deps)
    exit 1
}
if (-not $exes) { Write-Error "[test_fast] no test executables found"; exit 1 }
$buildSecs = $phase.Elapsed.TotalSeconds

# Pre-warm the product binary ONCE, after the build and before the parallel
# phase. Several suites spawn it per test (brush_embed's `pir __brush`, the
# cli/rpc/e2e suites' `CARGO_BIN_EXE_pir`), and Windows charges the first
# execution of the freshly linked ~300 MB debug exe to whichever suite hits it
# first: measured 2.77 s cold vs 0.013 s warm for `pir --version` on this host,
# and brush_embed's six spawns cost 21.5 s in a cold test_fast run vs 0.4 s
# warm. Paying that once here keeps it out of every suite's wall time. A warm
# binary makes this a no-op (~10 ms).
$pirExe = Join-Path $env:CARGO_TARGET_DIR "debug\pir.exe"
if (Test-Path $pirExe) {
    $warm = [System.Diagnostics.Stopwatch]::StartNew()
    $warmOut = Join-Path $env:TEMP ("pir-test-fast-warm-" + $PID + ".out")
    $warmErr = Join-Path $env:TEMP ("pir-test-fast-warm-" + $PID + ".err")
    try {
        Start-Process -FilePath $pirExe -ArgumentList "--version" -Wait -NoNewWindow `
            -RedirectStandardOutput $warmOut -RedirectStandardError $warmErr | Out-Null
    } catch {
        # A missing/unrunnable binary is the suite's failure to report, not
        # the runner's: never turn a warm-up hiccup into a build failure.
    } finally {
        Remove-Item -Force -ErrorAction SilentlyContinue $warmOut, $warmErr
    }
    Write-Host ("[test_fast] pre-warmed pir.exe in {0:N2}s" -f $warm.Elapsed.TotalSeconds)
}

if ($Concurrency -le 0) {
    # Keep total load near the core count: too much oversubscription makes
    # real-timer suites (bash spawns, HTTP idle timeouts) flake, which the
    # serial retry then has to sort out anyway.
    $Concurrency = [Math]::Max(2, [Math]::Min(6, [Math]::Floor([Environment]::ProcessorCount / 3)))
}
if ($Threads -le 0) { $Threads = 4 }

# Per-suite test threads. -Threads is the CAP: the merged g_* harnesses and
# mcp_integration are the only targets with enough tests to use 4 libtest
# workers (877 tests in g_agent, 68 process-spawning tests in
# mcp_integration). Every standalone tests/*.rs binary has a handful of tests,
# where 4 workers only add scheduler pressure. With 16 concurrent suites,
# 16 x 4 workers oversubscribed this 16-CPU host 4x (measured 171 s full run);
# one worker for the standalone suites measured 127 s, and giving the six
# long suites their 4 workers brought the parallel phase to 108 s
# (logs/lane-test-mock/*.log).
function Get-SuiteThreads {
    param([string]$Exe)
    $leaf = Split-Path -Leaf $Exe
    $target = $leaf -replace '-[0-9a-f]{16}\.exe$', '' -replace '\.exe$', ''
    # The merged harnesses already have the most tests; more than 4 libtest
    # workers was measured slower on this host (8 workers: g_agent 103 s,
    # mcp_integration 84 s, full parallel phase 105 s vs 96 s at 4), because
    # their wall clock is dominated by ~330 MB mock-child spawns, not CPU.
    if ($target -like 'g_*' -or $target -eq 'pir' -or $target -eq 'mcp_integration') {
        return $Threads
    }
    return 1
}

# Longest-processing-time-first: the previous run's per-suite seconds (the
# same last-run.json -RetryFailed reads) order the queue so the 60-120 s
# suites start first instead of at their alphabetical position. Unknown
# targets (first run, new suite) go first so they cannot become a tail.
$priorSecs = @{}
if (Test-Path $statePath) {
    try {
        $prevState = Get-Content $statePath -Raw | ConvertFrom-Json
        foreach ($s in @($prevState.suites)) {
            if ($s -and $s.target) { $priorSecs[[string]$s.target] = [double]$s.secs }
        }
    } catch { }
}
$exes = @($exes | Sort-Object @{ Expression = {
            $leaf = Split-Path -Leaf $_
            $t = $leaf -replace '-[0-9a-f]{16}\.exe$', '' -replace '\.exe$', ''
            if ($priorSecs.ContainsKey($t)) { - [double]$priorSecs[$t] } else { -1000000.0 }
        } }, Name)

Write-Host ("[test_fast] {0} test binaries, {1} concurrent, {2} threads each (merged g_* + mcp_integration; standalone suites use 1)" -f $exes.Count, $Concurrency, $Threads)

$logDir = Join-Path $env:TEMP ("pir-test-fast-" + $PID)
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

$running = New-Object System.Collections.ArrayList
$results = New-Object System.Collections.ArrayList

function Complete-Finished {
    param($running, $results)
    $i = 0
    while ($i -lt $running.Count) {
        if ($running[$i].Proc.HasExited) {
            # An orphaned grandchild (a test-spawned daemon) can keep the
            # redirected log handle open after the harness exits; an unbounded
            # WaitForExit() then hangs the whole sweep. Bound the drain and
            # keep the real exit code even when the handle lags.
            if (-not $running[$i].Proc.WaitForExit(5000)) {
                Write-Host ("[test_fast] warning: {0} exited but its log handle is still held; continuing" -f (Split-Path -Leaf $running[$i].Exe))
            }
            $exitCode = -1
            if ($running[$i].ExitFile -and (Test-Path $running[$i].ExitFile)) {
                try { $exitCode = [int]((Get-Content -Raw $running[$i].ExitFile).Trim()) } catch { }
            }
            if ($exitCode -lt 0) {
                try { $exitCode = $running[$i].Proc.ExitCode } catch { }
            }
            [void]$results.Add([pscustomobject]@{
                Exe  = $running[$i].Exe
                Log  = $running[$i].Log
                Name = (Split-Path -Leaf $running[$i].Exe)
                Exit = $exitCode
            })
            $running.RemoveAt($i)
        } else { $i++ }
    }
}

foreach ($exe in $exes) {
    while ($running.Count -ge $Concurrency) {
        Complete-Finished $running $results
        if ($running.Count -ge $Concurrency) { Start-Sleep -Milliseconds 40 }
    }
    $log = Join-Path $logDir ((Split-Path -Leaf $exe) + ".log")
    $suiteThreads = Get-SuiteThreads $exe
    # Start-Process with -RedirectStandardOutput blocks for tens of seconds on
    # a cold 300 MB test binary (Defender scans the image during CreateProcess),
    # which serializes the dispatcher and makes the nominal -Concurrency a
    # fiction. Launch through cmd.exe and redirect inside the child instead:
    # Process.Start returns immediately, while the shell waits for the test and
    # records its exit code in a sidecar file for Complete-Finished.
    $exitFile = $log + ".exit"
    Remove-Item -Force -ErrorAction SilentlyContinue $exitFile
    $cmdLine = '""{0}" --test-threads={1} > "{2}" 2> "{3}" & echo %ERRORLEVEL% > "{4}""' -f `
        $exe, $suiteThreads, $log, ($log + ".err"), $exitFile
    $proc = Start-Process -FilePath $env:ComSpec `
        -ArgumentList @('/d', '/s', '/c', $cmdLine) `
        -NoNewWindow -PassThru
    [void]$running.Add([pscustomobject]@{ Proc = $proc; Exe = $exe; Log = $log; ExitFile = $exitFile })
}
while ($running.Count -gt 0) {
    Complete-Finished $running $results
    if ($running.Count -gt 0) { Start-Sleep -Milliseconds 40 }
}
$parallelSecs = $phase.Elapsed.TotalSeconds - $buildSecs

# --- aggregate -------------------------------------------------------------
function Read-SuiteResult {
    param($logPath, $exitCode = $null)
    $r = [pscustomobject]@{ Passed = 0; Failed = 0; Secs = 0.0; Ok = $false; Empty = $false }
    if (Test-Path $logPath) {
        $line = Select-String -Path $logPath -Pattern '^test result:' | Select-Object -Last 1
        if ($line) {
            if ($line.Line -match '(\d+) passed') { $r.Passed = [int]$Matches[1] }
            if ($line.Line -match '(\d+) failed') { $r.Failed = [int]$Matches[1] }
            if ($line.Line -match 'finished in ([\d.]+)s') { $r.Secs = [double]$Matches[1] }
            # A binary whose harness ran ZERO tests (a bin's unit-test harness, or
            # every test ignored) is EMPTY — neither green nor failed. It is
            # counted separately so "0 passed" can never inflate the green total.
            # The exit code is checked too: a binary that dies before printing a
            # summary leaves Passed/Failed at 0 and used to look like "0 failed".
            $r.Empty = ($r.Passed -eq 0 -and $r.Failed -eq 0)
            $r.Ok = ($r.Failed -eq 0) -and (($null -eq $exitCode) -or ($exitCode -eq 0))
        }
    }
    return $r
}

$totalPass = 0
$totalFail = 0
$emptySuites = 0
$suites = @()
$failedRuns = @()
foreach ($r in $results) {
    $sr = Read-SuiteResult $r.Log $r.Exit
    $totalPass += $sr.Passed
    $totalFail += $sr.Failed
    if (-not $sr.Ok) { $failedRuns += $r }
    elseif ($sr.Empty) { $emptySuites += 1 }
    $suites += [pscustomobject]@{ Name = $r.Name; Passed = $sr.Passed; Failed = $sr.Failed; Secs = $sr.Secs }
}

Write-Host ""
Write-Host "=== slowest suites (parallel phase) ==="
$suites | Sort-Object Secs -Descending | Select-Object -First 10 | ForEach-Object {
    Write-Host ("  {0,7:N2}s  {1,4} passed  {2,3} failed  {3}" -f $_.Secs, $_.Passed, $_.Failed, $_.Name)
}

# Failed suites under parallel load are re-verified SEQUENTIALLY: real-timer
# tests (bash spawns, HTTP servers, idle timeouts) can flake when every suite
# saturates the CPU at once. A suite that passes alone is reported as
# load-flaky, not failed; only a suite failing alone fails the run.
#
# The retry runs ONE libtest worker: the point of the retry is to remove the
# load, and 16 workers still run a dozen tests (and their child processes) at
# once on this host. Measured: `plugin_message_tool_ui::
# native_send_message_while_busy_steers_the_run` failed under a 16-worker
# retry and passed with one; the retry is a correctness gate, not a race.
$realFailures = @()
if ($failedRuns.Count -gt 0) {
    Write-Host ""
    Write-Host ("[test_fast] {0} suite(s) failed in the parallel phase; re-running each alone..." -f $failedRuns.Count)
    foreach ($r in $failedRuns) {
        $retryLog = $r.Log + ".retry"
        $proc = Start-Process -FilePath $r.Exe `
            -ArgumentList "--test-threads=1" `
            -NoNewWindow -PassThru `
            -RedirectStandardOutput $retryLog `
            -RedirectStandardError ($retryLog + ".err")
        if (-not $proc.WaitForExit(180000)) {
            Write-Host ("  [FAILED]     {0}: serial retry did not exit within 180s" -f $r.Name)
            $realFailures += [pscustomobject]@{ Run = $r; Result = ([pscustomobject]@{ Passed = 0; Failed = 0; Secs = 0.0; Ok = $false; Empty = $false }) }
            continue
        }
        $sr = Read-SuiteResult $retryLog $proc.ExitCode
        if ($sr.Ok) {
            Write-Host ("  [load-flaky] {0}: {1} passed on serial retry ({2:N2}s)" -f $r.Name, $sr.Passed, $sr.Secs)
        } else {
            # A crashed harness prints no summary: report the exit code so
            # "0 failed on serial retry" is not mistaken for an empty suite.
            $detail = if ($sr.Passed -eq 0 -and $sr.Failed -eq 0) { "no harness summary, exit $($proc.ExitCode)" } else { "$($sr.Failed) failed" }
            Write-Host ("  [FAILED]     {0}: {1} on serial retry" -f $r.Name, $detail)
            $realFailures += [pscustomobject]@{ Run = $r; Result = $sr }
        }
    }
}

if ($realFailures.Count -gt 0) {
    Write-Host ""
    Write-Host "=== REAL FAILURES ==="
    foreach ($f in $realFailures) {
        Write-Host ("--- {0} ---" -f $f.Run.Name)
        $log = $f.Run.Log + ".retry"
        if (Test-Path $log) {
            Select-String -Path $log -Pattern 'FAILED|panicked at|^failures:' | Select-Object -First 20 | ForEach-Object { Write-Host ("  " + $_.Line) }
        }
    }
}

Write-Host ""
Write-Host ("[test_fast] total: {0} passed, {1} failed (parallel), {2} real failure(s) after serial retry across {3} suites ({4} empty harnesses ran 0 tests and are NOT counted as passed)" -f $totalPass, $totalFail, $realFailures.Count, $suites.Count, $emptySuites)
Write-Host ("[test_fast] timing: build {0:N1}s + parallel {1:N1}s = {2:N1}s wall (logs: {3})" -f $buildSecs, $parallelSecs, $phase.Elapsed.TotalSeconds, $logDir)

# --- persist state (for -RetryFailed / scripts/test_select.ps1) ------------
# Written by every run that reaches the execution phase (a build failure exits
# above and deliberately leaves the previous state intact, so -RetryFailed
# keeps meaning "the last run that actually ran").
New-Item -ItemType Directory -Force -Path $stateDir | Out-Null
$toTarget = { param($n) ($n -replace '-[0-9a-f]{16}\.exe$', '') -replace '\.exe$', '' }
$realNames = @($realFailures | ForEach-Object { $_.Run.Name })
$state = [pscustomobject]@{
    when          = (Get-Date).ToString("s")
    targetDir     = $env:CARGO_TARGET_DIR
    buildSecs     = [Math]::Round($buildSecs, 2)
    runSecs       = [Math]::Round($parallelSecs, 2)
    totalSecs     = [Math]::Round($phase.Elapsed.TotalSeconds, 2)
    passed        = $totalPass
    # `failed` = suites that failed both the parallel pass AND their serial
    # retry (what -RetryFailed consumes); `failedParallel` keeps the raw
    # parallel number so a load-flaky run is distinguishable from a red one.
    failed        = $realFailures.Count
    failedParallel = $failedRuns.Count
    emptySuites   = $emptySuites
    exitCode      = $(if ($totalPass -eq 0 -or $realFailures.Count -gt 0) { 1 } else { 0 })
    flaky         = @($failedRuns | Where-Object { $realNames -notcontains $_.Name } | ForEach-Object { & $toTarget $_.Name })
    failedTargets = @($realFailures | ForEach-Object { & $toTarget $_.Run.Name })
    suites        = @($suites | Sort-Object Secs -Descending | ForEach-Object {
            [pscustomobject]@{ target = (& $toTarget $_.Name); passed = $_.Passed; failed = $_.Failed; secs = $_.Secs }
        })
    logDir        = $logDir
}
# UTF-8 WITHOUT BOM: Set-Content -Encoding UTF8 emits a BOM under Windows
# PowerShell 5.1, which makes the file unreadable by Python's json.load and by
# strict JSON parsers (measured: json.loads on the BOM-prefixed file fails).
$stateJson = $state | ConvertTo-Json -Depth 5
[IO.File]::WriteAllText($statePath, $stateJson, (New-Object System.Text.UTF8Encoding($false)))

if ($totalPass -eq 0) {
    Write-Host ""
    Write-Host "[test_fast] FAILED: no test executed (every suite reported 0 passed); this is not a green run"
    exit 1
}
if ($realFailures.Count -gt 0) { exit 1 } else { exit 0 }
