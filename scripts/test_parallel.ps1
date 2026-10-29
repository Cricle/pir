# scripts/test_parallel.ps1 — LOCAL sharded test runner.
#
# THIS IS A LOCAL SPEED TOOL, NOT A CI REPLACEMENT. CI runs
# `cargo nextest run --profile ci` in eight shards on eight separate runners
# (.github/workflows/ci.yml), each with its own target dir and cache. This
# script runs every shard on ONE machine against ONE target dir; it cannot
# reproduce CI's per-shard isolation, and a green run here does not prove the
# CI run (the runner image, toolchain and feature flags still differ).
#
# TARGET DIRECTORY DISCIPLINE
#   - The default is `target-alt` (gitignored, owned by the local loop).
#   - Do NOT point this at a `target-lane-*` directory: those belong to other
#     agents/lanes. Cargo takes an exclusive build-directory lock and this
#     runner starts several cargo processes at once, so sharing a lane dir
#     would serialize (or fight) that lane's builds. The script refuses a
#     `target-lane-*` path (even for -ListOnly).
#   - The default `target/` is reserved by the eval harness; passing it prints
#     a warning (it is allowed, but do not do it while the harness is using it).
#
# USAGE
#   powershell -NoProfile -File scripts/test_parallel.ps1 -ListOnly
#       Print the shard plan and the coverage check; compile nothing.
#   powershell -NoProfile -File scripts/test_parallel.ps1
#       Warm-build once, then run all targets in N shards.
#   powershell -NoProfile -File scripts/test_parallel.ps1 -Shards 4
#   powershell -NoProfile -File scripts/test_parallel.ps1 -Test g_ai,g_tui
#   powershell -NoProfile -File scripts/test_parallel.ps1 -Test g_agent -Test g_tools
#   powershell -NoProfile -File scripts/test_parallel.ps1 -UseNextest
#   powershell -NoProfile -File scripts/test_parallel.ps1 -NoFailFast -TimeoutMinutes 45
#   powershell -NoProfile -File scripts/test_parallel.ps1 -LogDir logs/my-run
#   powershell -NoProfile -File scripts/test_parallel.ps1 -NoBuild   # reuse binaries
#
# PARAMETERS
#   -Shards N          number of shards; default min(logical CPUs, 8)
#   -TargetDir <path>  build dir; default target-alt (relative to repo root)
#   -Test <name>       only these cargo test targets (repeatable / comma list);
#                      `pir` means the lib unit target (`cargo test --lib`)
#   -ListOnly          print the plan + coverage check, do not build or run
#   -NoFailFast        let every shard finish after a failure (default: stop
#                      and kill the remaining shards as soon as one fails)
#   -TimeoutMinutes N  per-process bound (warm build and each shard); 0 = none
#   -LogDir <path>     exact log output dir; default
#                      logs/test-parallel/<yyyyMMdd-HHmmss>
#   -UseNextest        force nextest (error if cargo-nextest is absent);
#                      otherwise nextest is auto-detected and used if present
#   -NoNextest         force plain `cargo test` even when nextest is installed
#   -Weighted          shard by a deterministic weight (1 + test-fn-count/25,
#                      including #[path]-included tests/suites/*.rs) instead of
#                      alphabetical round-robin
#   -NoBuild           skip the single warm-up `cargo test --no-run`
#   -TestThreads N     libtest threads per shard (default: auto, see below);
#                      sets NEXTEST_TEST_THREADS in nextest mode
#
# HOW IT WORKS
#   1. Targets come from `cargo metadata --no-deps` (authoritative: explicit
#      [[test]] sections would be included). If that fails, the script falls
#      back to scanning tests/*.rs. `tests/suites/**` are NOT targets: they are
#      modules pulled in by the g_*.rs harnesses via #[path = "suites/..."].
#   2. Targets are sorted by ordinal (byte) name, the same order bash's
#      `tests/*.rs` glob uses, and assigned:
#        default  -> index % Shards (stable, count-balanced)
#        -Weighted-> longest-processing-time first (deterministic greedy)
#   3. One `cargo test --no-run` builds every selected target first. Without
#      this, N concurrent cargo processes serialize on cargo's target-dir lock
#      during linking; with it, the link work runs once (inside cargo, which
#      parallelizes with -j) and the shards only re-check freshness.
#   4. Each shard is one independent process:
#        cargo test --test a --test b ... --no-fail-fast -- --test-threads=N
#      or, when nextest is used:
#        cargo nextest run --locked --profile local --test a --test b ...
#      (NEXTEST_TEST_THREADS=N is exported for the nextest shards).
#      Writing logs/test-parallel/<timestamp>/shard-N.log (stdout+stderr
#      merged after the process exits; raw streams are removed).
#   5. The summary reports per-target pass/fail, wall time, failing targets and
#      the log paths. Exit 1 if anything failed (or no test ran).
#
# LIMITS
#   - Doctests are not run: `cargo test --doc` is a separate gate (CI's
#     `doctest` job; scripts/prepush.sh step 5).
#   - The bin unit-test harness (`cargo test --bins`) is not selected here.
#     The CI shards now select it on shard 1; src/ has no #[test] today, so a
#     local full run and CI currently execute the same test functions.
#   - Real-timer suites can flake when all shards saturate the CPU; re-run a
#     failing shard alone (e.g. -Test <name> -NoBuild) before believing it.
#   - The per-shard command runs all test binaries in that shard, but the
#     runner-level fail-fast can stop other shards early by design.

[CmdletBinding()]
param(
    [int]$Shards = 0,
    [string]$TargetDir = "",
    [string[]]$Test = @(),
    [switch]$ListOnly,
    [switch]$NoFailFast,
    [int]$TimeoutMinutes = 0,
    [string]$LogDir = "",
    [switch]$UseNextest,
    [switch]$NoNextest,
    [switch]$Weighted,
    [switch]$NoBuild,
    [int]$TestThreads = 0
)

$ErrorActionPreference = "Stop"

# `powershell -File script.ps1 -Test a,b` hands the list over as ONE string
# (-File mode does not parse PS array literals); split it back apart.
$Test = @($Test | ForEach-Object { $_ -split ',' } | Where-Object { $_ -ne '' } | ForEach-Object { $_.Trim() })

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Resolve-RepoPath {
    param([string]$Path)
    if ([IO.Path]::IsPathRooted($Path)) { return [IO.Path]::GetFullPath($Path) }
    return [IO.Path]::GetFullPath((Join-Path $root $Path))
}

if ($TimeoutMinutes -lt 0) { Write-Host "[test_parallel] -TimeoutMinutes must be >= 0"; exit 1 }
if ($TestThreads -lt 0) { Write-Host "[test_parallel] -TestThreads must be >= 0"; exit 1 }

$targetDirAbs = ""
if ($TargetDir -ne "") { $targetDirAbs = Resolve-RepoPath $TargetDir } else { $targetDirAbs = Join-Path $root "target-alt" }
$targetLeaf = Split-Path -Leaf $targetDirAbs
# Match the whole path, not just the leaf: `target-lane-d\sub` is still a lane
# build dir and must be refused.
if ($targetDirAbs -match '(?i)[\\/]target-lane-[^\\/]+([\\/]|$)') {
    Write-Host "[test_parallel] refusing target dir '$targetDirAbs': target-lane-* belongs to another lane."
    Write-Host "[test_parallel] use the default target-alt (or another private dir), and do not share a lane's build dir."
    exit 1
}
if ($targetLeaf -eq "target") {
    Write-Warning "[test_parallel] -TargetDir is the default 'target/' owned by the eval harness; prefer target-alt."
}

# Every child process inherits these. Setting CARGO_TARGET_DIR (instead of
# passing --target-dir) keeps the cargo command lines short and means the
# shard processes cannot silently disagree about the build directory.
$env:CARGO_TARGET_DIR = $targetDirAbs
if (-not $env:RUST_MIN_STACK) { $env:RUST_MIN_STACK = "8388608" }
# Redirected cargo/libtest output is not a TTY; pin color off so the logs are
# parseable and diffable even if a terminal is attached somewhere.
$env:CARGO_TERM_COLOR = "never"

# --- nextest detection ------------------------------------------------------
$nextestExe = Get-Command cargo-nextest -ErrorAction SilentlyContinue
$nextestProfile = ""
if (Test-Path (Join-Path $root ".config\nextest.toml")) {
    if (Select-String -Path (Join-Path $root ".config\nextest.toml") -Pattern '^\s*\[profile\.local\]\s*$' -Quiet) {
        $nextestProfile = "local"
    }
}
$useNextest = $false
$nextestForced = $false
if ($NoNextest -and $UseNextest) {
    Write-Host "[test_parallel] -UseNextest and -NoNextest are mutually exclusive"
    exit 1
}
if ($UseNextest) {
    if (-not $nextestExe) {
        Write-Host "[test_parallel] -UseNextest was passed but cargo-nextest is not on PATH."
        Write-Host "[test_parallel] install it with: cargo install cargo-nextest --locked"
        exit 1
    }
    $useNextest = $true
    $nextestForced = $true
} elseif (-not $NoNextest) {
    $useNextest = [bool]$nextestExe
}

# --- target discovery ------------------------------------------------------
function Get-OrdinalKey {
    param([string]$Text)
    if ($null -eq $Text) { return "" }
    $sb = New-Object System.Text.StringBuilder
    foreach ($ch in $Text.ToCharArray()) { [void]$sb.Append(([int][char]$ch).ToString("X4")) }
    return $sb.ToString()
}

function New-TargetRecord {
    param([string]$Name, [string]$SrcPath, [bool]$IsLib)
    return [pscustomobject]@{
        Name    = $Name
        SrcPath = $SrcPath
        IsLib   = $IsLib
        Key     = (Get-OrdinalKey $Name) + $(if ($IsLib) { "#lib" } else { "#test" })
        Weight  = 1.0
    }
}

function Get-TargetsFromMetadata {
    $cargoCmd = Get-Command cargo -ErrorAction SilentlyContinue
    if (-not $cargoCmd) { return $null }
    $prevEAP = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $raw = & cargo metadata --no-deps --format-version 1 2>$null
    $code = $LASTEXITCODE
    $ErrorActionPreference = $prevEAP
    if ($code -ne 0 -or -not $raw) { return $null }
    try { $meta = (($raw | Out-String) | ConvertFrom-Json) } catch { return $null }
    if (-not $meta -or -not $meta.packages) { return $null }
    $pkg = @($meta.packages | Where-Object { $_.name -eq "pir-sdk" })
    if ($pkg.Count -eq 0) { $pkg = @($meta.packages | Select-Object -First 1) }
    $pkg = $pkg[0]
    if (-not $pkg) { return $null }
    $tests = New-Object System.Collections.ArrayList
    $lib = $null
    foreach ($t in @($pkg.targets)) {
        $kinds = @($t.kind)
        if ($kinds -contains "test") {
            [void]$tests.Add((New-TargetRecord -Name $t.name -SrcPath $t.src_path -IsLib $false))
        } elseif ($kinds -contains "lib") {
            $lib = New-TargetRecord -Name $t.name -SrcPath $t.src_path -IsLib $true
        }
    }
    if ($tests.Count -eq 0) { return $null }
    return [pscustomobject]@{
        Tests  = @($tests)
        Lib    = $lib
        Source = "cargo metadata"
    }
}

function Get-TargetsFromScan {
    $tests = New-Object System.Collections.ArrayList
    foreach ($f in Get-ChildItem -Path (Join-Path $root "tests\*.rs") -File -ErrorAction SilentlyContinue) {
        [void]$tests.Add((New-TargetRecord -Name ([IO.Path]::GetFileNameWithoutExtension($f.Name)) -SrcPath $f.FullName -IsLib $false))
    }
    $lib = $null
    $libPath = Join-Path $root "src\lib.rs"
    if (Test-Path $libPath) { $lib = New-TargetRecord -Name "pir" -SrcPath $libPath -IsLib $true }
    return [pscustomobject]@{ Tests = @($tests); Lib = $lib; Source = "tests/*.rs scan" }
}

$discovered = Get-TargetsFromMetadata
if ($null -eq $discovered) {
    Write-Warning "[test_parallel] cargo metadata unavailable/failed; falling back to a tests/*.rs scan."
    $discovered = Get-TargetsFromScan
}
$allTargets = New-Object System.Collections.ArrayList
foreach ($t in $discovered.Tests) { [void]$allTargets.Add($t) }
if ($discovered.Lib) { [void]$allTargets.Add($discovered.Lib) }
$allTargets = @($allTargets)

if ($Test.Count -gt 0) {
    $known = @($allTargets | ForEach-Object { $_.Name })
    $unknown = @($Test | Where-Object { $known -notcontains $_ })
    if ($unknown.Count -gt 0) {
        # A suite module is not a target; point at the merged harness instead
        # of letting cargo fail with "no test target named ...".
        $suiteOwner = @{}
        foreach ($f in Get-ChildItem -Path (Join-Path $root "tests\*.rs") -File -ErrorAction SilentlyContinue) {
            $text = Get-Content $f.FullName -Raw
            foreach ($m in [regex]::Matches($text, '#\[path\s*=\s*"suites/([A-Za-z0-9_\-/]+)\.rs"')) {
                $suiteOwner[$m.Groups[1].Value] = [IO.Path]::GetFileNameWithoutExtension($f.Name)
            }
        }
        foreach ($u in $unknown) {
            $owner = $suiteOwner[$u]
            if ($owner) {
                Write-Host "[test_parallel] '$u' is a tests/suites module included by '$owner'; use -Test $owner"
            } else {
                Write-Host "[test_parallel] unknown test target '$u'"
            }
        }
        Write-Host ("[test_parallel] known targets: {0} test target(s){1}" -f $discovered.Tests.Count, $(if ($discovered.Lib) { " + lib" } else { "" }))
        exit 1
    }
    $selected = @($allTargets | Where-Object { $Test -contains $_.Name })
} else {
    $selected = $allTargets
}
if ($selected.Count -eq 0) { Write-Host "[test_parallel] no targets selected"; exit 1 }

# --- weight (only used by -Weighted) --------------------------------------
function Get-TargetWeight {
    param($Target)
    $testCount = 0
    $paths = New-Object System.Collections.ArrayList
    if ($Target.SrcPath -and (Test-Path $Target.SrcPath)) { [void]$paths.Add($Target.SrcPath) }
    # g_*.rs harnesses pull their cases in with #[path = "suites/x.rs"]; count
    # those files too so a 700-test merged harness outweighs a 3-test file.
    foreach ($p in @($paths)) {
        if (-not (Test-Path $p)) { continue }
        $text = Get-Content $p -Raw
        foreach ($m in [regex]::Matches($text, '#\[path\s*=\s*"([^"]+\.rs)"\s*\]')) {
            $rel = $m.Groups[1].Value -replace '/', [IO.Path]::DirectorySeparatorChar
            $candidate = Join-Path (Split-Path $p -Parent) $rel
            if ((Test-Path $candidate) -and ($paths -notcontains $candidate)) { [void]$paths.Add($candidate) }
        }
    }
    foreach ($p in $paths) {
        $text = Get-Content $p -Raw
        $testCount += ([regex]::Matches($text, '#\[(?:tokio::)?test\b')).Count
        $testCount += ([regex]::Matches($text, '#\[rstest\b')).Count
    }
    # One unit of weight per target approximates the ~5.5s link each binary
    # pays; 25 test functions per extra unit approximates run time. Both terms
    # are documented heuristics, not measurements; the assignment itself is
    # deterministic for a fixed tree.
    return (1.0 + ($testCount / 25.0))
}

# --- shard assignment ------------------------------------------------------
if ($Shards -le 0) { $Shards = [Math]::Min([Environment]::ProcessorCount, 8) }
if ($Shards -lt 1) { $Shards = 1 }
if ($Shards -gt $selected.Count) { $Shards = [Math]::Max(1, $selected.Count) }
# Each shard runs one test binary at a time, and libtest/nextest would each
# default to every CPU. N shards x num-cpus is N-times oversubscribed and makes
# real-timer suites flake, so cap the per-shard thread count (auto: at least 4,
# otherwise logical CPUs / shards -- ~16-32 runnable tests in total on this
# 16-core box). -TestThreads overrides it.
if ($TestThreads -le 0) {
    $TestThreads = [Math]::Max(4, [Math]::Floor([Environment]::ProcessorCount / $Shards))
}

$buckets = New-Object System.Collections.ArrayList
for ($i = 0; $i -lt $Shards; $i++) { [void]$buckets.Add((New-Object System.Collections.ArrayList)) }

$byKey = @($selected | Sort-Object -Property Key)
if ($Weighted) {
    foreach ($t in $byKey) { $t.Weight = Get-TargetWeight $t }
    # LPT: heaviest first; equal weights fall back to the ordinal key so the
    # assignment is reproducible regardless of Sort-Object stability.
    $byWeight = @($byKey | Sort-Object -Property @{ Expression = { $_.Weight }; Descending = $true }, @{ Expression = { $_.Key }; Descending = $false })
    $loads = [double[]]::new($Shards)
    foreach ($t in $byWeight) {
        $best = 0
        for ($s = 1; $s -lt $Shards; $s++) { if ($loads[$s] -lt $loads[$best]) { $best = $s } }
        [void]$buckets[$best].Add($t)
        $loads[$best] += $t.Weight
    }
} else {
    for ($i = 0; $i -lt $byKey.Count; $i++) {
        [void]$buckets[($i % $Shards)].Add($byKey[$i])
    }
}
for ($s = 0; $s -lt $Shards; $s++) {
    $sortedBucket = @($buckets[$s] | Sort-Object -Property Key)
    $buckets[$s] = New-Object System.Collections.ArrayList
    foreach ($t in $sortedBucket) { [void]$buckets[$s].Add($t) }
}

# --- plan + coverage check -------------------------------------------------
$assigned = @()
foreach ($b in $buckets) { $assigned += @($b) }
$assignedKeys = @($assigned | ForEach-Object { $_.Key })
$uniqueKeys = @($assignedKeys | Sort-Object -Unique)
$duplicateCount = $assignedKeys.Count - $uniqueKeys.Count
$missing = @($selected | Where-Object { $assignedKeys -notcontains $_.Key })

function Write-WrappedList {
    param([string[]]$Items, [string]$Indent = "  ", [int]$Width = 110)
    $line = $Indent
    foreach ($item in $Items) {
        if (($line.Length + $item.Length + 2) -gt $Width -and $line.Length -gt $Indent.Length) {
            Write-Host $line
            $line = $Indent
        }
        $line += $item + "  "
    }
    if ($line.Trim().Length -gt 0) { Write-Host $line.TrimEnd() }
}

$runnerLabel = if ($useNextest) { "cargo nextest run" + $(if ($nextestProfile) { " --profile $nextestProfile" } else { "" }) } else { "cargo test" }
Write-Host ""
Write-Host "=== test_parallel plan ==="
Write-Host ("runner:     {0}{1}" -f $runnerLabel, $(if ($useNextest) { $(if ($nextestForced) { " (forced)" } else { " (auto-detected)" }) } else { "" }))
Write-Host ("targets:    {0} selected of {1} runnable ({2} tests/*.rs{3}) from {4}" -f $selected.Count, $allTargets.Count, $discovered.Tests.Count, $(if ($discovered.Lib) { " + lib unit target" } else { "" }), $discovered.Source)
Write-Host ("shards:     {0} (logical CPUs: {1})" -f $Shards, [Environment]::ProcessorCount)
Write-Host ("threads:    {0} per shard" -f $TestThreads)
Write-Host ("strategy:   {0}" -f $(if ($Weighted) { "weighted LPT (1 + test-fns/25)" } else { "ordinal alphabetical + round-robin" }))
Write-Host ("target dir: {0}" -f $targetDirAbs)
Write-Host ""
for ($s = 0; $s -lt $Shards; $s++) {
    $items = @($buckets[$s] | ForEach-Object { if ($_.IsLib) { "$($_.Name) (lib)" } else { $_.Name } })
    if ($Weighted) {
        $w = 0.0
        foreach ($t in $buckets[$s]) { $w += $t.Weight }
        Write-Host ("shard {0}: {1} target(s), weight {2:N1}" -f ($s + 1), $items.Count, $w)
    } else {
        Write-Host ("shard {0}: {1} target(s)" -f ($s + 1), $items.Count)
    }
    Write-WrappedList -Items $items
}
Write-Host ""
Write-Host ("coverage: {0}/{1} selected targets assigned; {2} duplicate(s), {3} missing" -f $uniqueKeys.Count, $selected.Count, $duplicateCount, $missing.Count)
if ($selected.Count -eq $allTargets.Count) {
    Write-Host ("          full plan covers {0} runnable target(s) ({1} tests/*.rs + {2} lib)" -f $allTargets.Count, $discovered.Tests.Count, $(if ($discovered.Lib) { 1 } else { 0 }))
}
if ($duplicateCount -ne 0 -or $missing.Count -ne 0) {
    Write-Host "[test_parallel] INTERNAL PLAN ERROR: shard assignment is not a partition"
    exit 1
}
Write-Host ""

if ($ListOnly) { exit 0 }

# --- execution helpers -----------------------------------------------------
$stamp = (Get-Date).ToString("yyyyMMdd-HHmmss")
if ($LogDir -ne "") {
    $logDirAbs = Resolve-RepoPath $LogDir
} else {
    $logDirAbs = Join-Path $root ("logs\test-parallel\" + $stamp)
}
New-Item -ItemType Directory -Force -Path $logDirAbs | Out-Null

$totalStopwatch = [System.Diagnostics.Stopwatch]::StartNew()

function Merge-ProcessLog {
    param([string]$StdoutPath, [string]$StderrPath, [string]$DestPath)
    $err = ""
    $out = ""
    if (Test-Path $StderrPath) { $err = [IO.File]::ReadAllText($StderrPath) }
    if (Test-Path $StdoutPath) { $out = [IO.File]::ReadAllText($StdoutPath) }
    $text = ""
    if ($err) { $text += $err }
    if ($out) { $text += $out }
    [IO.File]::WriteAllText($DestPath, $text, (New-Object System.Text.UTF8Encoding($false)))
    Remove-Item -Path $StdoutPath, $StderrPath -Force -ErrorAction SilentlyContinue
}

function Stop-ProcessTree {
    param([int]$ProcessId)
    $taskkill = Get-Command taskkill.exe -ErrorAction SilentlyContinue
    if ($taskkill) {
        & taskkill.exe /PID $ProcessId /T /F 2>$null | Out-Null
    } else {
        Stop-Process -Id $ProcessId -Force -ErrorAction SilentlyContinue
    }
}

function ConvertTo-ArgToken {
    param([string]$Arg)
    if ($Arg -eq "") { return '""' }
    if ($Arg -notmatch '[\s"]') { return $Arg }
    return '"' + ($Arg -replace '(\\*)"', '$1$1\"') + '"'
}

# Start-Process -PassThru does not populate ExitCode for a process that has
# already exited by the time PowerShell looks at it (measured on PS 5.1 with a
# short-lived child: $p.ExitCode was empty, which would make every fast shard
# look failed). System.Diagnostics.Process + async stream copies gives a
# reliable ExitCode and no pipe deadlock while still letting N shards run.
function Start-LoggedProcess {
    param([string]$FilePath, [string[]]$Arguments, [string]$StdoutPath, [string]$StderrPath)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $FilePath
    $psi.Arguments = (($Arguments | ForEach-Object { ConvertTo-ArgToken $_ }) -join " ")
    $psi.WorkingDirectory = $root
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $outFs = [IO.File]::Create($StdoutPath)
    $errFs = [IO.File]::Create($StderrPath)
    $proc = New-Object System.Diagnostics.Process
    $proc.StartInfo = $psi
    try {
        [void]$proc.Start()
        $outTask = $proc.StandardOutput.BaseStream.CopyToAsync($outFs)
        $errTask = $proc.StandardError.BaseStream.CopyToAsync($errFs)
    } catch {
        $outFs.Close(); $errFs.Close()
        throw
    }
    return [pscustomobject]@{ Proc = $proc; OutTask = $outTask; ErrTask = $errTask; OutFs = $outFs; ErrFs = $errFs }
}

function Complete-LoggedProcess {
    param($Job)
    $Job.Proc.WaitForExit()
    # Bounded: a wedged descendant that survived the process-tree kill could
    # keep the pipe open forever. Ten seconds is far more than a normal drain.
    [void]$Job.OutTask.Wait(10000)
    [void]$Job.ErrTask.Wait(10000)
    try { $Job.OutFs.Flush(); $Job.ErrFs.Flush() } catch { }
    $Job.OutFs.Close(); $Job.ErrFs.Close()
}

function Invoke-LoggedProcess {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [string]$LogPath,
        [string]$Label
    )
    $stdout = $LogPath + ".stdout.tmp"
    $stderr = $LogPath + ".stderr.tmp"
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $job = Start-LoggedProcess -FilePath $FilePath -Arguments $Arguments -StdoutPath $stdout -StderrPath $stderr
    $timedOut = $false
    while (-not $job.Proc.HasExited) {
        if ($TimeoutMinutes -gt 0 -and $sw.Elapsed.TotalMinutes -ge $TimeoutMinutes) {
            Write-Host ("[test_parallel] {0}: timeout after {1} minute(s); killing process tree" -f $Label, $TimeoutMinutes)
            Stop-ProcessTree -ProcessId $job.Proc.Id
            $timedOut = $true
            break
        }
        Start-Sleep -Milliseconds 200
    }
    Complete-LoggedProcess $job
    $sw.Stop()
    Merge-ProcessLog -StdoutPath $stdout -StderrPath $stderr -DestPath $LogPath
    $exitCode = if ($timedOut) { -1 } elseif ($null -ne $job.Proc.ExitCode) { $job.Proc.ExitCode } else { -1 }
    return [pscustomobject]@{
        ExitCode = $exitCode
        TimedOut = $timedOut
        Seconds  = [Math]::Round($sw.Elapsed.TotalSeconds, 2)
        LogPath  = $LogPath
    }
}

function Get-Selectors {
    param($Targets)
    $argsOut = New-Object System.Collections.ArrayList
    foreach ($t in @($Targets | Sort-Object -Property Key)) {
        if ($t.IsLib) { [void]$argsOut.Add("--lib") }
        else { [void]$argsOut.Add("--test"); [void]$argsOut.Add($t.Name) }
    }
    return @($argsOut)
}

$cargoExe = (Get-Command cargo).Source
$allSelectors = Get-Selectors $selected

# --- phase 1: one warm build ----------------------------------------------
$buildSeconds = 0.0
if (-not $NoBuild) {
    $buildArgs = @("test", "--no-run", "--locked") + $allSelectors
    Write-Host "[test_parallel] warm build: cargo test --no-run over $($selected.Count) target(s)..."
    $build = Invoke-LoggedProcess -FilePath $cargoExe -Arguments $buildArgs -LogPath (Join-Path $logDirAbs "build.log") -Label "build"
    $buildSeconds = $build.Seconds
    if ($build.ExitCode -ne 0) {
        Write-Host ""
        Write-Host ("[test_parallel] BUILD FAILED (exit {0}) after {1:N1}s; no shard was started" -f $build.ExitCode, $build.Seconds)
        Write-Host ("[test_parallel] log: {0}" -f $build.LogPath)
        if (Test-Path $build.LogPath) {
            Get-Content $build.LogPath -Tail 40 | ForEach-Object { Write-Host ("  " + $_) }
        }
        exit 1
    }
    Write-Host ("[test_parallel] warm build OK in {0:N1}s ({1})" -f $build.Seconds, $build.LogPath)
} else {
    Write-Host "[test_parallel] -NoBuild: reusing binaries already in $targetDirAbs"
}

# --- phase 2: shards -------------------------------------------------------
if ($useNextest) { $env:NEXTEST_TEST_THREADS = "$TestThreads" }
$shardRuns = New-Object System.Collections.ArrayList
for ($s = 0; $s -lt $Shards; $s++) {
    $selectors = Get-Selectors $buckets[$s]
    if ($selectors.Count -eq 0) { continue }
    if ($useNextest) {
        $cargoArgs = @("nextest", "run", "--locked")
        if ($nextestProfile -ne "") { $cargoArgs += @("--profile", $nextestProfile) }
        $cargoArgs += $selectors
    } else {
        $cargoArgs = @("test") + $selectors + @("--locked", "--no-fail-fast", "--", "--test-threads=$TestThreads")
    }
    $shardLog = Join-Path $logDirAbs ("shard-{0}.log" -f ($s + 1))
    $stdout = $shardLog + ".stdout.tmp"
    $stderr = $shardLog + ".stderr.tmp"
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $job = Start-LoggedProcess -FilePath $cargoExe -Arguments $cargoArgs -StdoutPath $stdout -StderrPath $stderr
    [void]$shardRuns.Add([pscustomobject]@{
        Index    = $s + 1
        Targets  = @($buckets[$s])
        Proc     = $job.Proc
        Job      = $job
        Timer    = $sw
        Stdout   = $stdout
        Stderr   = $stderr
        LogPath  = $shardLog
        ExitCode = $null
        TimedOut = $false
        Stopped  = $false
        Seconds  = 0.0
    })
}

Write-Host ("[test_parallel] started {0} shard process(es); logs in {1}" -f $shardRuns.Count, $logDirAbs)

$running = New-Object System.Collections.ArrayList
foreach ($r in $shardRuns) { [void]$running.Add($r) }
while ($running.Count -gt 0) {
    $i = 0
    $failTriggered = $false
    while ($i -lt $running.Count) {
        $r = $running[$i]
        if ($r.Proc.HasExited) {
            Complete-LoggedProcess $r.Job
            $r.Timer.Stop()
            Merge-ProcessLog -StdoutPath $r.Stdout -StderrPath $r.Stderr -DestPath $r.LogPath
            $r.ExitCode = if ($null -ne $r.Proc.ExitCode) { $r.Proc.ExitCode } else { -1 }
            $r.Seconds = [Math]::Round($r.Timer.Elapsed.TotalSeconds, 2)
            $running.RemoveAt($i)
            if ($r.ExitCode -ne 0 -and -not $NoFailFast -and $running.Count -gt 0 -and -not $failTriggered) {
                $failTriggered = $true
                Write-Host ("[test_parallel] shard {0} failed (exit {1}); stopping the remaining {2} shard(s) (-NoFailFast to disable)" -f $r.Index, $r.ExitCode, $running.Count)
                foreach ($left in @($running)) {
                    Stop-ProcessTree -ProcessId $left.Proc.Id
                    Complete-LoggedProcess $left.Job
                    $left.Timer.Stop()
                    Merge-ProcessLog -StdoutPath $left.Stdout -StderrPath $left.Stderr -DestPath $left.LogPath
                    $left.ExitCode = if ($null -ne $left.Proc.ExitCode) { $left.Proc.ExitCode } else { -1 }
                    $left.Seconds = [Math]::Round($left.Timer.Elapsed.TotalSeconds, 2)
                    $left.Stopped = $true
                }
                $running.Clear()
            }
        } elseif ($TimeoutMinutes -gt 0 -and $r.Timer.Elapsed.TotalMinutes -ge $TimeoutMinutes) {
            Write-Host ("[test_parallel] shard {0}: timeout after {1} minute(s); killing process tree" -f $r.Index, $TimeoutMinutes)
            Stop-ProcessTree -ProcessId $r.Proc.Id
            Complete-LoggedProcess $r.Job
            $r.Timer.Stop()
            Merge-ProcessLog -StdoutPath $r.Stdout -StderrPath $r.Stderr -DestPath $r.LogPath
            $r.ExitCode = -1
            $r.TimedOut = $true
            $r.Seconds = [Math]::Round($r.Timer.Elapsed.TotalSeconds, 2)
            $running.RemoveAt($i)
        } else {
            $i++
        }
    }
    if ($running.Count -gt 0) { Start-Sleep -Milliseconds 150 }
}
$runSeconds = [Math]::Round($totalStopwatch.Elapsed.TotalSeconds - $buildSeconds, 2)

# --- phase 3: per-target results ------------------------------------------
# Cargo/libtest and nextest print different shapes. Both parsers map a result
# back to the shard's declared target list, so a crash leaves a target marked
# "no result" instead of silently green.
function Get-CargoTestResults {
    param([string]$LogPath, $Targets)
    $states = @{}
    foreach ($t in $Targets) {
        $states[$t.Key] = [pscustomobject]@{ Target = $t; Seen = $false; Ok = $false; Passed = 0; Failed = 0; Ignored = 0; NoResult = $true }
    }
    $byName = @{}
    foreach ($t in $Targets) { $byName[$t.Name] = $states[$t.Key] }
    $libState = $null
    foreach ($t in $Targets) { if ($t.IsLib) { $libState = $states[$t.Key] } }
    $current = $null
    foreach ($line in @(Get-Content -Path $LogPath -ErrorAction SilentlyContinue)) {
        if ($line -match '^\s*Running (unittests )?(.+?)\s+\(') {
            $isUnit = [bool]$Matches[1]
            $file = $Matches[2]
            if ($isUnit) {
                $current = $libState
            } else {
                $name = [IO.Path]::GetFileNameWithoutExtension($file)
                if ($byName.ContainsKey($name)) { $current = $byName[$name] } else { $current = $null }
            }
        } elseif ($line -match '^test result:\s+(ok|FAILED)\.\s+(\d+) passed;\s+(\d+) failed;\s+(\d+) ignored') {
            if ($null -ne $current) {
                $current.Seen = $true
                $current.NoResult = $false
                $current.Passed = [int]$Matches[2]
                $current.Failed = [int]$Matches[3]
                $current.Ignored = [int]$Matches[4]
                $current.Ok = ($Matches[1] -eq "ok") -and ($current.Failed -eq 0)
                $current = $null
            }
        }
    }
    return @($states.Values)
}

function Get-NextestResults {
    param([string]$LogPath, $Targets, [int]$ShardExit)
    $states = @{}
    foreach ($t in $Targets) {
        $states[$t.Key] = [pscustomobject]@{ Target = $t; Seen = $false; Ok = $false; Passed = 0; Failed = 0; Ignored = 0; NoResult = $true }
    }
    $byName = @{}
    foreach ($t in $Targets) { $byName[$t.Name] = $states[$t.Key] }
    $summarySeen = $false
    $summaryPassed = 0
    $summaryFailed = 0
    $summarySkipped = 0
    $mappedFailures = 0
    foreach ($line in @(Get-Content -Path $LogPath -ErrorAction SilentlyContinue)) {
        if ($line -match '^\s*(FAIL|TIMEOUT|LEAK)\s+\[\s*[0-9.]+s?\]\s+([^:]+)::') {
            $name = $Matches[2]
            if ($byName.ContainsKey($name)) {
                $st = $byName[$name]
                $st.Seen = $true
                $st.Ok = $false
                $st.NoResult = $false
                $st.Failed = $st.Failed + 1
                $mappedFailures++
            }
        } elseif ($line -match '^\s*PASS\s+\[\s*[0-9.]+s?\]\s+([^:]+)::') {
            $name = $Matches[1]
            if ($byName.ContainsKey($name)) {
                $st = $byName[$name]
                $st.Seen = $true
                $st.NoResult = $false
                $st.Passed = $st.Passed + 1
                $st.Ok = $true
            }
        } elseif ($line -match '^Summary\s+\[[^\]]*\]\s+(\d+) tests run:\s+(\d+) passed,\s+(\d+) failed,\s+(\d+) skipped') {
            $summarySeen = $true
            $summaryPassed = [int]$Matches[2]
            $summaryFailed = [int]$Matches[3]
            $summarySkipped = [int]$Matches[4]
        }
    }
    foreach ($st in $states.Values) {
        if ($summarySeen) {
            if (-not $st.Seen) {
                # A target with no PASS/FAIL line but a complete summary ran
                # and passed (nextest's status level can hide passing tests).
                $st.Ok = $true
                $st.NoResult = $false
            }
        } elseif ($ShardExit -eq 0) {
            $st.Ok = $true
            $st.NoResult = $false
        }
    }
    if ($ShardExit -ne 0 -and (-not $summarySeen -or ($summaryFailed -gt 0 -and $mappedFailures -eq 0))) {
        # Failed before a summary, or the summary counts failures that no
        # FAIL line could be attributed to (binary-id format change): refuse
        # to guess which target is green.
        foreach ($st in $states.Values) { $st.Ok = $false; $st.NoResult = $true }
    }
    return [pscustomobject]@{
        States = @($states.Values); SummarySeen = $summarySeen
        SummaryPassed = $summaryPassed; SummaryFailed = $summaryFailed; SummarySkipped = $summarySkipped
    }
}

$perTarget = New-Object System.Collections.ArrayList
$nextestSummary = [pscustomobject]@{ Seen = $false; Passed = 0; Failed = 0; Skipped = 0 }
foreach ($r in $shardRuns) {
    if (-not (Test-Path $r.LogPath)) { continue }
    if ($useNextest) {
        $nr = Get-NextestResults -LogPath $r.LogPath -Targets $r.Targets -ShardExit $r.ExitCode
        if ($nr.SummarySeen) {
            $nextestSummary.Seen = $true
            $nextestSummary.Passed += $nr.SummaryPassed
            $nextestSummary.Failed += $nr.SummaryFailed
            $nextestSummary.Skipped += $nr.SummarySkipped
        }
        foreach ($st in $nr.States) {
            if ($r.Stopped) { $st.Ok = $false; $st.NoResult = $true }
            [void]$perTarget.Add([pscustomobject]@{
                Target = $st.Target.Name; Lib = $st.Target.IsLib; Shard = $r.Index
                Ok = $st.Ok; NoResult = $st.NoResult; Passed = $st.Passed; Failed = $st.Failed; Ignored = $st.Ignored
            })
        }
    } else {
        foreach ($st in (Get-CargoTestResults -LogPath $r.LogPath -Targets $r.Targets)) {
            if ($r.Stopped) { $st.Ok = $false; $st.NoResult = $true }
            [void]$perTarget.Add([pscustomobject]@{
                Target = $st.Target.Name; Lib = $st.Target.IsLib; Shard = $r.Index
                Ok = $st.Ok; NoResult = $st.NoResult; Passed = $st.Passed; Failed = $st.Failed; Ignored = $st.Ignored
            })
        }
    }
}

$passedTargets = @($perTarget | Where-Object { $_.Ok })
$failedTargets = @($perTarget | Where-Object { -not $_.Ok -and -not $_.NoResult })
$noResultTargets = @($perTarget | Where-Object { $_.NoResult })

# Safety net: a shard that exited non-zero but whose log yielded no failure
# and no unaccounted target must never be reported green (this is what a
# parser/output-format change looks like). Timed-out shards keep any results
# parsed before the kill; the non-zero exit still fails the run below.
foreach ($r in @($shardRuns | Where-Object { $_.ExitCode -ne 0 -and -not $_.Stopped -and -not $_.TimedOut })) {
    $own = @($perTarget | Where-Object { $_.Shard -eq $r.Index })
    $accounted = @($own | Where-Object { -not $_.Ok -or $_.NoResult })
    if ($accounted.Count -eq 0) {
        foreach ($t in $own) { $t.Ok = $false; $t.NoResult = $true }
    }
}
$failedTargets = @($perTarget | Where-Object { -not $_.Ok -and -not $_.NoResult })
$noResultTargets = @($perTarget | Where-Object { $_.NoResult })
$passedTargets = @($perTarget | Where-Object { $_.Ok })
$shardFailure = @($shardRuns | Where-Object { $_.ExitCode -ne 0 }).Count -gt 0

$passedTests = 0; $failedTests = 0; $ignoredTests = 0
foreach ($t in $perTarget) { $passedTests += $t.Passed; $failedTests += $t.Failed; $ignoredTests += $t.Ignored }
# nextest's per-test PASS lines can be hidden by status-level; its Summary is
# the authoritative test count, so prefer it when it was printed.
if ($useNextest -and $nextestSummary.Seen) {
    $passedTests = $nextestSummary.Passed
    $failedTests = $nextestSummary.Failed
    $ignoredTests = $nextestSummary.Skipped
}

# --- phase 4: report -------------------------------------------------------
Write-Host ""
Write-Host "=== summary ==="
Write-Host ("runner:   {0}" -f $runnerLabel)
Write-Host ("targets:  {0} passed, {1} failed, {2} no-result ({3} selected)" -f $passedTargets.Count, $failedTargets.Count, $noResultTargets.Count, $perTarget.Count)
Write-Host ("tests:    {0} passed, {1} failed, {2} ignored (from test-result lines)" -f $passedTests, $failedTests, $ignoredTests)
Write-Host ("timing:   warm build {0:N1}s + shards {1:N1}s = {2:N1}s wall" -f $buildSeconds, $runSeconds, $totalStopwatch.Elapsed.TotalSeconds)
Write-Host ("logs:     {0}" -f $logDirAbs)
for ($s = 0; $s -lt $Shards; $s++) {
    $r = @($shardRuns | Where-Object { $_.Index -eq ($s + 1) })[0]
    if ($r) {
        $state = if ($r.Stopped) { "STOPPED (fail-fast)" } elseif ($r.TimedOut) { "TIMEOUT" } elseif ($r.ExitCode -eq 0) { "ok" } else { "FAILED" }
        Write-Host ("  shard {0}: {1,3} target(s)  {2,8:N1}s  exit {3}  {4}  {5}" -f $r.Index, @($r.Targets).Count, $r.Seconds, $r.ExitCode, $state, $r.LogPath)
    }
}
if ($failedTargets.Count -gt 0 -or $noResultTargets.Count -gt 0) {
    Write-Host ""
    Write-Host "=== failing / unaccounted targets ==="
    foreach ($t in @($failedTargets + $noResultTargets)) {
        $label = if ($t.NoResult) { "NO RESULT" } else { "FAILED" }
        Write-Host ("  [{0}] {1} (shard {2}, {3} passed / {4} failed)" -f $label, $t.Target, $t.Shard, $t.Passed, $t.Failed)
    }
    Write-Host ""
    Write-Host "  Re-run one target alone, e.g.:"
    Write-Host ("    powershell -NoProfile -File scripts/test_parallel.ps1 -Test <name> -NoBuild")
    Write-Host "  or inspect the shard log(s) listed above."
}

$summary = [pscustomobject]@{
    when         = (Get-Date).ToString("s")
    runner       = $runnerLabel
    targetDir    = $targetDirAbs
    shards       = $Shards
    testThreads  = $TestThreads
    strategy     = $(if ($Weighted) { "weighted" } else { "round-robin" })
    buildSeconds = $buildSeconds
    runSeconds   = $runSeconds
    wallSeconds  = [Math]::Round($totalStopwatch.Elapsed.TotalSeconds, 2)
    totals       = [pscustomobject]@{
        selected = $perTarget.Count
        passed   = $passedTargets.Count
        failed   = $failedTargets.Count
        noResult = $noResultTargets.Count
        testsPassed  = $passedTests
        testsFailed  = $failedTests
        testsIgnored = $ignoredTests
    }
    failedTargets = @(($failedTargets + $noResultTargets) | ForEach-Object {
        [pscustomobject]@{ target = $_.Target; shard = $_.Shard; noResult = $_.NoResult }
    })
    shardsDetail = @($shardRuns | ForEach-Object {
        [pscustomobject]@{
            shard = $_.Index; targets = @($_.Targets | ForEach-Object { $_.Name })
            exitCode = $_.ExitCode; seconds = $_.Seconds; timedOut = $_.TimedOut; stopped = $_.Stopped; log = $_.LogPath
        }
    })
    targets = @($perTarget | ForEach-Object {
        [pscustomobject]@{ target = $_.Target; lib = $_.Lib; shard = $_.Shard; ok = $_.Ok; noResult = $_.NoResult; passed = $_.Passed; failed = $_.Failed; ignored = $_.Ignored }
    })
    logDir = $logDirAbs
}
$summaryPath = Join-Path $logDirAbs "summary.json"
[IO.File]::WriteAllText($summaryPath, ($summary | ConvertTo-Json -Depth 8), (New-Object System.Text.UTF8Encoding($false)))
Write-Host ("summary json: {0}" -f $summaryPath)

if ($perTarget.Count -eq 0) {
    Write-Host ""
    Write-Host "[test_parallel] FAILED: no target produced a result (not a green run)"
    exit 1
}
if ($passedTests -eq 0 -and $failedTests -eq 0) {
    Write-Host ""
    Write-Host "[test_parallel] WARNING: every harness reported 0 tests; treating as failure"
    exit 1
}
if ($failedTargets.Count -gt 0 -or $noResultTargets.Count -gt 0 -or $shardFailure) { exit 1 }
exit 0
