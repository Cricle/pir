#requires -Version 5.1
<#
.SYNOPSIS
  PGO helper for pir: self-test, instrumented build, profile collection, and
  profile-use build.

.DESCRIPTION
  This script never touches the repository's `target/` directory. All build
  output defaults to `<repo>/target-pgo/`.

  Modes (pick exactly one):
    -SelfTest   Compile a hello-world with rustc, collect a profile, rebuild
                with -Cprofile-use, and print size/time before/after. This is
                the fastest way to prove the local toolchain can do PGO.
    -Generate   Build pir with -Cprofile-generate, run the selected workload,
                then merge all *.profraw into <ProfileDir>\merged.profdata.
    -Use        Build pir with -Cprofile-use=<ProfileDir>\merged.profdata and
                copy the binary to <OutDir>\pir-pgo.exe.

  -Generate/-Use only print the plan by default. Add -Execute to run it.
  -SelfTest always runs (it only builds a temporary hello-world).

  Toolchain notes:
    * gnu = stable-x86_64-pc-windows-gnu. The rustup GNU target does NOT ship
      libprofiler_builtins, so -Cprofile-generate fails with E0463 on a stock
      toolchain. Pass -ProfilerRuntime <libprofiler_rt.a> to use the
      experimental -Zno-profiler-runtime path (requires RUSTC_BOOTSTRAP=1).
    * msvc = stable-x86_64-pc-windows-msvc. It ships profiler_builtins; use
      this in CI (windows-latest with VS Build Tools / Windows SDK).

.EXAMPLE
  # Prove the toolchain with the self-built GNU runtime
  .\scripts\pgo\build-pgo.ps1 -SelfTest -ProfilerRuntime .\logs\lane-pgo\tmp\pgo-hello\profrt-gnu\libprofiler_rt.a

.EXAMPLE
  # Print (do not run) the GNU experimental profile-generation plan
  .\scripts\pgo\build-pgo.ps1 -Generate -Workload soak -ProfilerRuntime C:\path\libprofiler_rt.a

.EXAMPLE
  # Execute profile-generation with the hermetic 100-turn soak
  .\scripts\pgo\build-pgo.ps1 -Generate -Workload soak -ProfilerRuntime C:\path\libprofiler_rt.a -Execute

.EXAMPLE
  # Consume an existing merged profile
  .\scripts\pgo\build-pgo.ps1 -Use -ProfileDir target-pgo\pgo\profiles -Execute
#>
[CmdletBinding()]
param(
    [switch]$SelfTest,
    [switch]$Generate,
    [switch]$Use,

    [string]$PirExe = '',
    [string]$ProfileDir = '',
    [ValidateSet('smoke', 'print', 'soak', 'tui', 'all')]
    [string]$Workload = 'smoke',
    [string]$OutDir = '',

    [ValidateSet('gnu', 'msvc')]
    [string]$Toolchain = 'gnu',
    [string]$MingwBin = '',
    [string]$ProfilerRuntime = '',
    [string]$Model = '',

    [switch]$Execute
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Paths and shared helpers
# ---------------------------------------------------------------------------

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$TargetRoot = Join-Path $RepoRoot 'target-pgo'
$PlanDir = Join-Path $TargetRoot 'pgo'
if ($ProfileDir -eq '') { $ProfileDir = Join-Path $PlanDir 'profiles' }
if ($OutDir -eq '') { $OutDir = Join-Path $PlanDir 'out' }
if ($PirExe -eq '') { $PirExe = Join-Path $TargetRoot 'release\pir.exe' }
$ProfileDir = [IO.Path]::GetFullPath($ProfileDir)
$OutDir = [IO.Path]::GetFullPath($OutDir)
$PirExe = [IO.Path]::GetFullPath($PirExe)
$MergedProfile = Join-Path $ProfileDir 'merged.profdata'

function Assert-NotRepoTarget {
    param([string]$Path)
    $full = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $repoTarget = [IO.Path]::GetFullPath((Join-Path $RepoRoot 'target')).TrimEnd('\')
    if ($full.Equals($repoTarget, [StringComparison]::OrdinalIgnoreCase) -or
        $full.StartsWith($repoTarget + '\', [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to write under the repository target/ directory: $Path. Use target-pgo/."
    }
}

function Format-Command {
    param([string]$Exe, [string[]]$Arguments)
    $parts = @($Exe) + @($Arguments)
    ($parts | ForEach-Object {
        if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
    }) -join ' '
}

function New-EncodedRustFlags {
    param([string[]]$Flags)
    # CARGO_ENCODED_RUSTFLAGS uses the unit separator (0x1f), not spaces.
    return ($Flags -join [char]0x1f)
}

function Resolve-Python {
    $candidates = @()
    if ($env:PYTHON) { $candidates += $env:PYTHON }
    $candidates += (Join-Path $RepoRoot '..\tools\python312\python.exe')
    $candidates += (Join-Path $RepoRoot '..\tools\python312\python')
    $cmd = Get-Command python -ErrorAction SilentlyContinue
    if ($cmd) { $candidates += $cmd.Source }
    foreach ($c in $candidates) {
        if ($c -and (Test-Path $c)) { return (Resolve-Path $c).Path }
    }
    return 'python'
}

function Get-ToolchainInfo {
    param([string]$Kind, [string]$Mingw)
    $triple = if ($Kind -eq 'msvc') { 'x86_64-pc-windows-msvc' } else { 'x86_64-pc-windows-gnu' }
    $toolchain = "stable-$triple"
    $env:RUSTUP_TOOLCHAIN = $toolchain

    if ($Kind -eq 'gnu') {
        $mingwCandidates = @($Mingw, $env:MINGW64_BIN, 'C:\wp\empty\tools\mingw\mingw64\bin')
        $mingwFound = $null
        foreach ($candidate in $mingwCandidates) {
            if ($candidate -and (Test-Path (Join-Path $candidate 'gcc.exe'))) {
                $mingwFound = (Resolve-Path $candidate).Path
                break
            }
        }
        if ($mingwFound) {
            $env:PATH = $mingwFound + ';' + $env:PATH
        } else {
            Write-Warning "mingw64 gcc not found (pass -MingwBin). rust-lld may still link, but C dependencies need gcc on PATH."
        }
    }

    $rustc = (Get-Command rustc).Source
    $versionText = (& $rustc -vV) -join "`n"
    if ($LASTEXITCODE -ne 0) { throw "rustc -vV failed under $toolchain" }
    $llvm = [regex]::Match($versionText, 'LLVM version:\s*([0-9.]+)').Groups[1].Value
    if (-not $llvm) { throw "Could not parse LLVM version from rustc -vV" }

    $sysroot = (& $rustc --print sysroot).Trim()
    $libDir = Join-Path $sysroot "lib\rustlib\$triple\lib"
    $binDir = Join-Path $sysroot "lib\rustlib\$triple\bin"
    $profdata = Join-Path $binDir 'llvm-profdata.exe'
    $profdataMissing = $false
    if (-not (Test-Path $profdata)) {
        $onPath = Get-Command llvm-profdata -ErrorAction SilentlyContinue
        if ($onPath) {
            $profdata = $onPath.Source
        } else {
            $profdata = 'llvm-profdata'
            $profdataMissing = $true
            Write-Warning "llvm-profdata not found. Run: rustup component add llvm-tools-$triple"
        }
    }
    if (-not $profdataMissing) {
        $profdataVersion = (& $profdata --version) -join "`n"
        if ($profdataVersion -notmatch [regex]::Escape($llvm)) {
            Write-Warning "llvm-profdata version does not contain rustc LLVM version $llvm. Profiles may be incompatible."
        }
    }

    $profiler = @(Get-ChildItem -Path $libDir -Filter 'libprofiler_builtins-*.rlib' -ErrorAction SilentlyContinue)
    $profilerPath = if ($profiler.Count -gt 0) { $profiler[0].FullName } else { $null }

    return [pscustomobject]@{
        Kind            = $Kind
        Triple          = $triple
        Toolchain       = $toolchain
        Rustc           = $rustc
        LlvmVersion     = $llvm
        Sysroot         = $sysroot
        LibDir          = $libDir
        LlvmProfdata    = $profdata
        ProfdataMissing = $profdataMissing
        ProfilerBuiltins = $profilerPath
    }
}

# ---------------------------------------------------------------------------
# Plan construction / execution
# ---------------------------------------------------------------------------

$script:Plan = New-Object System.Collections.ArrayList

function Add-PlanStep {
    param(
        [string]$Name,
        [string]$Exe,
        [string[]]$Arguments,
        [string]$WorkingDir = $RepoRoot,
        [System.Collections.IDictionary]$Environment = @{}
    )
    [void]$script:Plan.Add([pscustomobject]@{
        Name        = $Name
        Exe         = $Exe
        Arguments   = $Arguments
        WorkingDir  = $WorkingDir
        Environment = $Environment
    })
}

function Show-Plan {
    Write-Host ''
    Write-Host "PGO plan (all artifacts stay under $TargetRoot):" -ForegroundColor Cyan
    foreach ($step in $script:Plan) {
        Write-Host ''
        Write-Host ("[{0}]  cwd={1}" -f $step.Name, $step.WorkingDir) -ForegroundColor Yellow
        foreach ($key in $step.Environment.Keys) {
            $value = [string]$step.Environment[$key]
            if ($key -eq 'CARGO_ENCODED_RUSTFLAGS') {
                $value = $value -replace [char]0x1f, ' <US> '
            }
            Write-Host ("  `$env:{0} = {1}" -f $key, $value)
        }
        Write-Host ("  " + (Format-Command $step.Exe $step.Arguments))
    }
    Write-Host ''
}

function Invoke-NativeStep {
    param($Step)
    $saved = @{}
    foreach ($key in $Step.Environment.Keys) {
        $saved[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
        [Environment]::SetEnvironmentVariable($key, [string]$Step.Environment[$key], 'Process')
    }
    Push-Location $Step.WorkingDir
    try {
        Write-Host ''
        Write-Host ("==> {0}" -f $Step.Name) -ForegroundColor Cyan
        Write-Host ("    " + (Format-Command $Step.Exe $Step.Arguments))
        & $Step.Exe @($Step.Arguments)
        if ($LASTEXITCODE -ne 0) {
            throw "Step '$($Step.Name)' failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
        foreach ($key in $saved.Keys) {
            [Environment]::SetEnvironmentVariable($key, $saved[$key], 'Process')
        }
    }
}

function Save-PlanText {
    param([string]$Path)
    New-Item -ItemType Directory -Force -Path (Split-Path $Path -Parent) | Out-Null
    $lines = New-Object System.Collections.ArrayList
    [void]$lines.Add("# Generated by scripts/pgo/build-pgo.ps1; review before running.")
    foreach ($step in $script:Plan) {
        [void]$lines.Add("")
        [void]$lines.Add("# step: $($step.Name) (cwd: $($step.WorkingDir))")
        foreach ($key in $step.Environment.Keys) {
            if ($key -eq 'CARGO_ENCODED_RUSTFLAGS') {
                [void]$lines.Add("# NOTE: the value below uses 0x1F (unit separator) between flags.")
            }
            [void]$lines.Add("`$env:$key = '$($step.Environment[$key])'")
        }
        [void]$lines.Add((Format-Command $step.Exe $step.Arguments))
    }
    Set-Content -Path $Path -Value ($lines -join "`r`n") -Encoding UTF8
}

function Clear-ProfileDir {
    if (-not (Test-Path $ProfileDir)) {
        New-Item -ItemType Directory -Force -Path $ProfileDir | Out-Null
        return
    }
    Get-ChildItem -Path $ProfileDir -Filter '*.profraw' -File -ErrorAction SilentlyContinue |
        Remove-Item -Force
    $merged = Join-Path $ProfileDir 'merged.profdata'
    if (Test-Path $merged) { Remove-Item $merged -Force }
}

# ---------------------------------------------------------------------------
# Workload steps
# ---------------------------------------------------------------------------

function Add-WorkloadSteps {
    param($ToolchainInfo)
    $profEnv = @{
        LLVM_PROFILE_FILE = (Join-Path $ProfileDir 'pir-%p-%m.profraw')
        PI_OFFLINE = '1'
        PIR_NO_UPDATE_CHECK = '1'
    }

    switch ($Workload) {
        'smoke' {
            Add-PlanStep -Name 'workload-version' -Exe $PirExe -Arguments @('--version') -Environment $profEnv
            Add-PlanStep -Name 'workload-help' -Exe $PirExe -Arguments @('--help') -Environment $profEnv
        }
        'print' {
            $args = @('print', '--mode', 'json')
            if ($Model) { $args = @('--model', $Model) + $args }
            $args += 'Reply with the single word: pgo'
            Add-PlanStep -Name 'workload-print' -Exe $PirExe -Arguments $args -Environment $profEnv
        }
        'soak' {
            $python = Resolve-Python
            $logDir = Join-Path $PlanDir 'soak-logs'
            Add-PlanStep -Name 'workload-soak-100' -Exe $python -Arguments @(
                (Join-Path $RepoRoot 'scripts\soak\soak_headless.py'),
                '--pir', $PirExe,
                '--turns', '100',
                '--interval', '10',
                '--log', $logDir
            ) -Environment $profEnv
        }
        'tui' {
            $python = Resolve-Python
            Add-PlanStep -Name 'workload-tui-startup' -Exe $python -Arguments @(
                'run_battery.py', '--pir', $PirExe, '--only', 's01*'
            ) -WorkingDir (Join-Path $RepoRoot 'scripts\tui_conpty') -Environment $profEnv
        }
        'all' {
            Add-PlanStep -Name 'workload-version' -Exe $PirExe -Arguments @('--version') -Environment $profEnv
            if ($Model) {
                Add-PlanStep -Name 'workload-print' -Exe $PirExe -Arguments @('--model', $Model, 'print', '--mode', 'json', 'Reply with the single word: pgo') -Environment $profEnv
            }
            $python = Resolve-Python
            Add-PlanStep -Name 'workload-soak-100' -Exe $python -Arguments @(
                (Join-Path $RepoRoot 'scripts\soak\soak_headless.py'),
                '--pir', $PirExe,
                '--turns', '100',
                '--interval', '10',
                '--log', (Join-Path $PlanDir 'soak-logs')
            ) -Environment $profEnv
            Add-PlanStep -Name 'workload-tui-startup' -Exe $python -Arguments @(
                'run_battery.py', '--pir', $PirExe, '--only', 's01*'
            ) -WorkingDir (Join-Path $RepoRoot 'scripts\tui_conpty') -Environment $profEnv
        }
    }
}

# ---------------------------------------------------------------------------
# Modes
# ---------------------------------------------------------------------------

$modes = @()
if ($SelfTest) { $modes += 'SelfTest' }
if ($Generate) { $modes += 'Generate' }
if ($Use) { $modes += 'Use' }
if ($modes.Count -ne 1) {
    Write-Host 'Pick exactly one mode: -SelfTest, -Generate or -Use.' -ForegroundColor Red
    exit 2
}

Assert-NotRepoTarget -Path $ProfileDir
Assert-NotRepoTarget -Path $OutDir
Assert-NotRepoTarget -Path $TargetRoot

$tc = Get-ToolchainInfo -Kind $Toolchain -Mingw $MingwBin
Write-Host ("toolchain: {0} (rustc {1}, LLVM {2})" -f $tc.Toolchain, (& $tc.Rustc --version), $tc.LlvmVersion) -ForegroundColor Green
if ($tc.ProfdataMissing) {
    Write-Host ("llvm-profdata: MISSING (rustup component add llvm-tools-{0})" -f $tc.Triple) -ForegroundColor Yellow
} else {
    Write-Host ("llvm-profdata: {0}" -f $tc.LlvmProfdata)
}
if ($tc.ProfilerBuiltins) {
    Write-Host ("profiler_builtins: {0}" -f $tc.ProfilerBuiltins)
} else {
    Write-Host 'profiler_builtins: MISSING for this target' -ForegroundColor Yellow
}

if ($SelfTest) {
    if ($tc.ProfdataMissing) {
        throw "llvm-profdata not found. Run: rustup component add llvm-tools-$($tc.Triple)"
    }
    $selfDir = Join-Path $TargetRoot 'pgo-selftest'
    New-Item -ItemType Directory -Force -Path $selfDir | Out-Null
    $hello = Join-Path $selfDir 'hello.rs'
    @'
use std::hint::black_box;

#[inline(never)]
fn mix(x: u64) -> u64 {
    x.wrapping_mul(6364136223846793005).rotate_left(17) ^ 0x9e37_79b9_7f4a_7c15
}

#[inline(never)]
fn rare_path(v: u64) -> u64 { v.wrapping_mul(31).rotate_left(7) & 0xffff }

#[inline(never)]
fn common_path(v: u64) -> u64 { v.wrapping_add(1) }

fn main() {
    let args: Vec<String> = std::env::args().collect();
    println!("hello, pgo");
    let n: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60_000_000);
    let mut acc = 0u64;
    for i in 0..n {
        let r = mix(i);
        if r % 16 == 0 {
            acc = acc.wrapping_add(rare_path(r));
        } else {
            acc = acc.wrapping_add(common_path(r & 0xff));
        }
    }
    println!("checksum={}", black_box(acc));
}
'@ | Set-Content -Path $hello -Encoding ASCII

    $common = @(
        '--target', $tc.Triple,
        '-C', 'opt-level=3',
        '-C', 'codegen-units=1',
        '-C', 'lto=fat',
        '-C', 'linker=rust-lld',
        '-C', 'strip=symbols'
    )
    $runtimeArgs = @()
    $runtimeEnv = @{}
    if ($Toolchain -eq 'gnu') {
        if (-not $ProfilerRuntime) {
            throw "GNU stable has no profiler_builtins (E0463). Pass -ProfilerRuntime <libprofiler_rt.a> or use -Toolchain msvc."
        }
        if (-not (Test-Path $ProfilerRuntime)) { throw "ProfilerRuntime not found: $ProfilerRuntime" }
        $runtimeArgs = @('-Zno-profiler-runtime', "-Clink-arg=$([IO.Path]::GetFullPath($ProfilerRuntime))")
        $runtimeEnv = @{ RUSTC_BOOTSTRAP = '1' }
    }

    $profDir = Join-Path $selfDir 'profraw'
    New-Item -ItemType Directory -Force -Path $profDir | Out-Null
    Get-ChildItem $profDir -Filter '*.profraw' -File -ErrorAction SilentlyContinue | Remove-Item -Force

    $baseExe = Join-Path $selfDir 'hello-base.exe'
    $genExe = Join-Path $selfDir 'hello-gen.exe'
    $useExe = Join-Path $selfDir 'hello-use.exe'

    Write-Host '==> self-test: baseline build'
    & $tc.Rustc @common $hello -o $baseExe
    if ($LASTEXITCODE -ne 0) { throw 'baseline hello build failed' }

    Write-Host '==> self-test: instrumented build'
    $genArgs = $common + @("-C", "profile-generate=$profDir", '-C', 'llvm-args=-pgo-warn-missing-function') + $runtimeArgs + @($hello, '-o', $genExe)
    $oldBootstrap = $env:RUSTC_BOOTSTRAP
    foreach ($key in $runtimeEnv.Keys) { [Environment]::SetEnvironmentVariable($key, $runtimeEnv[$key], 'Process') }
    try {
        & $tc.Rustc @genArgs
        if ($LASTEXITCODE -ne 0) { throw 'instrumented hello build failed' }
    } finally {
        $env:RUSTC_BOOTSTRAP = $oldBootstrap
    }

    Write-Host '==> self-test: profile workload'
    $oldProfileFile = $env:LLVM_PROFILE_FILE
    $env:LLVM_PROFILE_FILE = Join-Path $profDir 'run-%p.profraw'
    try {
        foreach ($n in @('1000000', '60000000', '60000000')) {
            & $genExe $n | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "instrumented hello run failed (n=$n)" }
        }
    } finally {
        $env:LLVM_PROFILE_FILE = $oldProfileFile
    }

    $profraws = @(Get-ChildItem $profDir -Filter '*.profraw' -File)
    if ($profraws.Count -eq 0) { throw 'no .profraw files were produced' }
    Write-Host ("==> self-test: merge {0} profile(s)" -f $profraws.Count)
    $merged = Join-Path $selfDir 'merged.profdata'
    & $tc.LlvmProfdata merge -o $merged $profDir
    if ($LASTEXITCODE -ne 0) { throw 'llvm-profdata merge failed' }

    Write-Host '==> self-test: profile-use build'
    & $tc.Rustc @common "-C" "profile-use=$merged" '-C' 'llvm-args=-pgo-warn-missing-function' $hello -o $useExe
    if ($LASTEXITCODE -ne 0) { throw 'profile-use hello build failed' }

    function Get-MedianMs {
        param([string]$Exe, [string]$Arg, [int]$Runs = 7)
        $times = New-Object System.Collections.ArrayList
        for ($i = 0; $i -lt ($Runs + 2); $i++) {
            $sw = [Diagnostics.Stopwatch]::StartNew()
            & $Exe $Arg | Out-Null
            $sw.Stop()
            if ($i -ge 2) { [void]$times.Add($sw.Elapsed.TotalMilliseconds) }
        }
        $sorted = @($times | Sort-Object)
        return $sorted[[int]($sorted.Count / 2)]
    }

    $baseMs = Get-MedianMs -Exe $baseExe -Arg '60000000'
    $useMs = Get-MedianMs -Exe $useExe -Arg '60000000'
    $baseSize = (Get-Item $baseExe).Length
    $useSize = (Get-Item $useExe).Length
    $speedup = if ($useMs -gt 0) { ($baseMs / $useMs - 1) * 100 } else { 0 }

    Write-Host ''
    Write-Host 'self-test result (60,000,000 iterations, fat LTO, median of 7):' -ForegroundColor Cyan
    Write-Host ("  baseline : {0,10:N0} bytes  {1,9:N1} ms" -f $baseSize, $baseMs)
    Write-Host ("  profile-use: {0,8:N0} bytes  {1,9:N1} ms  ({2:+0.00;-0.00}%)" -f $useSize, $useMs, $speedup)
    Write-Host ("  instrumented binary (for reference): {0:N0} bytes" -f (Get-Item $genExe).Length)
    exit 0
}

if ($Generate) {
    $blocked = $null
    if ($Toolchain -eq 'gnu' -and -not $tc.ProfilerBuiltins -and -not $ProfilerRuntime) {
        $blocked = @(
            'GNU stable-x86_64-pc-windows-gnu does not ship libprofiler_builtins:',
            '  error[E0463]: can''t find crate for `profiler_builtins`',
            'Options:',
            '  1) -Toolchain msvc (CI / VS Build Tools required)',
            '  2) -ProfilerRuntime <libprofiler_rt.a> (experimental -Zno-profiler-runtime path)'
        ) -join [Environment]::NewLine
    }
    if ($ProfilerRuntime -and -not (Test-Path $ProfilerRuntime)) {
        throw "ProfilerRuntime not found: $ProfilerRuntime"
    }
    New-Item -ItemType Directory -Force -Path $ProfileDir | Out-Null
    # Whole-graph flags: pir's code lives in the library crate as well as the
    # bin crate, so `cargo rustc --bin` would leave most of it uninstrumented.
    $rustFlags = @('-C', "profile-generate=$ProfileDir")
    $envs = [ordered]@{
        RUSTUP_TOOLCHAIN = $tc.Toolchain
        CARGO_TARGET_DIR = $TargetRoot
        CARGO_INCREMENTAL = '0'
    }
    if ($Toolchain -eq 'gnu' -and $ProfilerRuntime) {
        $rustFlags += @('-Zno-profiler-runtime', "-Clink-arg=$([IO.Path]::GetFullPath($ProfilerRuntime))")
        $envs['RUSTC_BOOTSTRAP'] = '1'
    }
    $envs['CARGO_ENCODED_RUSTFLAGS'] = New-EncodedRustFlags $rustFlags
    Add-PlanStep -Name 'generate-build' -Exe 'cargo' -Arguments @('build', '--release', '--bin', 'pir') -Environment $envs
    Add-WorkloadSteps -ToolchainInfo $tc
    Add-PlanStep -Name 'merge-profile' -Exe $tc.LlvmProfdata -Arguments @('merge', '-o', $MergedProfile, $ProfileDir)

    if (-not $Execute) {
        Show-Plan
        Save-PlanText -Path (Join-Path $PlanDir 'generate-plan.txt')
        if ($blocked) {
            Write-Host ''
            Write-Host $blocked -ForegroundColor Yellow
        }
        Write-Host "Dry run: plan saved to $(Join-Path $PlanDir 'generate-plan.txt'). Add -Execute to run." -ForegroundColor Yellow
        exit 0
    }
    if ($blocked) { throw $blocked }
    if ($tc.ProfdataMissing) {
        throw "llvm-profdata not found. Run: rustup component add llvm-tools-$($tc.Triple)"
    }

    Clear-ProfileDir
    foreach ($step in $script:Plan) { Invoke-NativeStep -Step $step }
    if (-not (Test-Path $MergedProfile)) { throw "merged profile missing: $MergedProfile" }
    Write-Host ("PGO profile ready: {0} ({1:N0} bytes)" -f $MergedProfile, (Get-Item $MergedProfile).Length) -ForegroundColor Green
    exit 0
}

if ($Use) {
    if (-not (Test-Path $MergedProfile)) {
        throw "Merged profile not found: $MergedProfile. Run -Generate first (or pass -ProfileDir)."
    }
    $useFlags = @('-C', "profile-use=$MergedProfile", '-C', 'llvm-args=-pgo-warn-missing-function')
    Add-PlanStep -Name 'use-build' -Exe 'cargo' -Arguments @('build', '--release', '--bin', 'pir') -Environment ([ordered]@{
        RUSTUP_TOOLCHAIN = $tc.Toolchain
        CARGO_TARGET_DIR = $TargetRoot
        CARGO_INCREMENTAL = '0'
        CARGO_ENCODED_RUSTFLAGS = (New-EncodedRustFlags $useFlags)
    })

    if (-not $Execute) {
        Show-Plan
        Save-PlanText -Path (Join-Path $PlanDir 'use-plan.txt')
        Write-Host "Dry run: plan saved to $(Join-Path $PlanDir 'use-plan.txt'). Add -Execute to run." -ForegroundColor Yellow
        exit 0
    }

    $built = Join-Path $TargetRoot 'release\pir.exe'
    $before = if (Test-Path $built) { (Get-Item $built).Length } else { $null }
    foreach ($step in $script:Plan) { Invoke-NativeStep -Step $step }
    if (-not (Test-Path $built)) { throw "build did not produce $built" }
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $dest = Join-Path $OutDir 'pir-pgo.exe'
    Copy-Item -Path $built -Destination $dest -Force
    $after = (Get-Item $dest).Length
    Write-Host ("PGO binary: {0} ({1:N0} bytes)" -f $dest, $after) -ForegroundColor Green
    if ($before) { Write-Host ("previous target-pgo binary: {0:N0} bytes" -f $before) }
    exit 0
}
