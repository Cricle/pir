#requires -Version 5.1
<#
.SYNOPSIS
  Constrained-device gate probe: run `pir` inside a Windows Job Object with a
  hard per-process commit limit, a forced CPU affinity, and an optional
  priority class, then measure idle/wake and workload behavior.

.DESCRIPTION
  What this probe measures (and why):

  * Hard memory cap -- CreateJobObjectW + JobObjectExtendedLimitInformation +
    JOB_OBJECT_LIMIT_PROCESS_MEMORY.  The limit is applied to the child while
    it is still CREATE_SUSPENDED, so the cap exists before the first byte of
    Rust startup code runs.  `ProcessMemoryLimit` caps *commit* per process
    (the quantity a 256 MB device actually runs out of), so the probe reports
    both commit (`PrivateUsage` / job `PeakProcessMemoryUsed`) and working set
    (RSS, what the STATUS table quotes).
  * Affinity -- SetProcessAffinityMask on the same process handle, default the
    first logical CPU only (`-Cores 1`).  This is the 1-core device shape.
  * Priority -- optional SetPriorityClass (`-Priority`), so a low-priority
    background lane can be simulated too.
  * Workloads (`-Workload`):
      idle    -- `pir --mode rpc --no-session`; wait for the `hello` reply,
                 then sample the idle steady state for `-IdleSeconds`.
      version -- `pir --version`; whole-process wall time and peak commit.
      print   -- one `pir print` turn against an in-process mock OpenAI
                 provider (no network, no credential), measuring wall time,
                 peak RSS/commit/threads/handles.
      soak    -- one RPC process, `-Turns` sequential prompts against the
                 mock provider; reports per-turn and whole-run peaks.  This is
                 the "one soak round" memory measurement.
  * Idle state, sampled for `-IdleSeconds` (default 15 s):
      - RSS (WorkingSetSize) peak / median / final,
      - commit (PrivateUsage) peak / final,
      - thread count min / max / final,
      - handle count min / max / final,
      - CPU time from two GetProcessTimes snapshots (user + kernel, 100 ns
        units).  Windows has no public per-process wakeup counter; the CPU
        time delta over the idle window is the documented proxy for
        timer/IO wakeups and scheduler activity.  An idle process that wakes
        periodically to poll shows up as non-zero kernel time even when the
        wall-clock CPU percentage rounds to 0.0%.
      - IO operation counters from two GetProcessIoCounters snapshots.  Every
        completed read/write/other operation implies a wakeup, so a non-zero
        delta is direct evidence of IO-driven wakeups; timer-only wakeups are
        still covered by the CPU-time proxy.

  Startup readiness is explicit: for RPC workloads the script writes one
  JSON-line `hello` command to the child's stdin and waits until stdout
  carries the `"pir-rpc-jsonl"` response.  Only then does the idle window
  start, so first-start cost is not mixed into the idle numbers.

  Enforcement proof: `-EnforceProbeMiB N` starts a second child of the same
  binary in a fresh Job Object capped at N MiB.  The child must terminate with
  a non-zero exit code; if it exits 0 or survives the probe window, the job
  limit is not biting and the gate fails.  This is the difference between "we
  set a limit" and "exceeding the limit gives the child a hard error instead of
  being silently ignored".  An exit of 0xC0000409 with a mingw
  "VirtualProtect failed" / Rust "memory allocation ... failed" stderr line is
  recorded as `failureKind = "oom"`.

  Repeatable gate modes:
    -Suite        run the standard matrix (default: idle,print,soak at
                  256/512/1024 MiB, 1 core) by spawning one child run per
                  cell; writes <Label>.suite.json and prints a table.  Exit 0
                  only when every cell passes.
    -SweepCaps    run the same single workload at each listed cap (e.g.
                  "8,12,16,24,32,48,64") to locate the minimum usable
                  budget; writes <Label>.sweep.json and reports the lowest
                  passing cap and the first OOM cap.

  Verdict / exit codes (single run):
    0  PASS  -- job limit verified via QueryInformationJobObject, the workload
               completed as expected, RSS/commit within budget, and (idle
               only) idle CPU below budget; clean shutdown; enforcement probe
               died non-zero when requested.
    1  FAIL  -- any of the above did not hold; the failing checks are printed
               and recorded in the JSON.
    2  CONFIG/LAUNCH error -- binary missing, bad arguments, job object could
               not be created/configured, or the child could not be launched.

  This script only reads the binary; it never writes into the repo's target/
  directory.  All child output and the JSON summary go to -OutDir
  (default: logs/perf/constrained).

.PARAMETER MemoryMiB
  Per-process commit cap in MiB.  The constrained-device row uses 256/512/1024.

.PARAMETER JobMemoryMiB
  Optional whole-job commit cap (all processes in the job). 0 = off.  Useful
  when a run is allowed to spawn tool children and the *device* budget, not
  just the root process, must hold.

.PARAMETER Cores
  Number of logical CPUs to allow (mask = first N bits).  Default 1 = the
  1-core device shape.

.PARAMETER Priority
  Optional SetPriorityClass: Idle, BelowNormal, Normal, AboveNormal, High,
  RealTime.

.PARAMETER EnforceProbeMiB
  If > 0, run the enforcement self-test described above with this cap.
  Recommended 8 (well below any working pir footprint).

.PARAMETER MaxRssMiB
  Working-set budget for the run.  0 = use -MemoryMiB (a process must not
  need more RSS than the device cap it is supposed to run on).  Set it high
  (e.g. 1024) to isolate the Job Object commit floor from the RSS budget.

.PARAMETER MaxIdleCpuPercent
  Maximum idle CPU time as a percentage of one core over the window.  Default
  5.0.  A truly idle host is near 0; this catches polling loops.  Applied to
  `-Workload idle` only.

.EXAMPLE
  powershell -NoProfile -File scripts/perf/constrained_device.ps1 `
      -Pir target/release/pir.exe -MemoryMiB 256 -IdleSeconds 15 -EnforceProbeMiB 8

.EXAMPLE
  powershell -NoProfile -File scripts/perf/constrained_device.ps1 `
      -Pir target/release/pir.exe -Suite -OutDir logs/lane-constrained

.EXAMPLE
  powershell -NoProfile -File scripts/perf/constrained_device.ps1 `
      -Pir target/release/pir.exe -Workload print -SweepCaps 8,12,16,24,32,48,64,96,128,256 `
      -OutDir logs/lane-constrained -Label minprint

.NOTES
  Windows PowerShell 5.1 is the tested contract (same as the other .ps1 gates
  in scripts/GATE-README.md).  Nested job objects require Windows 8+; on
  Windows 7 AssignProcessToJobObject fails when the parent is already in a
  job, and the script reports that as a launch error instead of pretending the
  cap was applied.
#>
[CmdletBinding()]
param(
    [string]$Pir = "target\debug\pir.exe",
    [ValidateRange(4, 65536)]
    [int]$MemoryMiB = 256,
    [ValidateRange(0, 65536)]
    [int]$JobMemoryMiB = 0,
    [ValidateRange(1, 64)]
    [int]$Cores = 1,
    [ValidateSet("", "Idle", "BelowNormal", "Normal", "AboveNormal", "High", "RealTime")]
    [string]$Priority = "",
    [ValidateRange(1, 3600)]
    [int]$IdleSeconds = 15,
    [ValidateRange(20, 5000)]
    [int]$SampleIntervalMs = 250,
    [ValidateRange(1, 300)]
    [int]$StartupTimeoutSeconds = 30,
    [ValidateRange(1, 120)]
    [int]$ShutdownTimeoutSeconds = 15,
    [ValidateRange(0, 65536)]
    [int]$EnforceProbeMiB = 0,
    [ValidateRange(0, 65536)]
    [int]$MaxRssMiB = 0,
    [ValidateRange(0.0, 100.0)]
    [double]$MaxIdleCpuPercent = 5.0,
    [ValidateSet("idle", "print", "soak", "version")]
    [string]$Workload = "idle",
    [ValidateRange(1, 10000)]
    [int]$Turns = 10,
    [ValidateRange(1, 600)]
    [int]$TurnTimeoutSeconds = 120,
    [ValidateRange(1, 600)]
    [int]$RunTimeoutSeconds = 180,
    [string]$Prompt = "say hi",
    [string]$ExpectedText = "",
    [switch]$MockProvider,
    [switch]$Suite,
    [string]$SuiteCaps = "256,512,1024",
    [string]$SuiteWorkloads = "idle,print,soak",
    [string]$SweepCaps = "",
    [string]$AgentDir = "",
    [string]$OutDir = "",
    [string]$Label = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2.0

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
if (-not [System.IO.Path]::IsPathRooted($Pir)) {
    $Pir = Join-Path $repoRoot $Pir
}
$Pir = [System.IO.Path]::GetFullPath($Pir)
if (-not (Test-Path -LiteralPath $Pir -PathType Leaf)) {
    Write-Host "[constrained] ERROR: pir binary not found: $Pir"
    exit 2
}
if ($OutDir -eq "") {
    $OutDir = Join-Path $repoRoot "logs\perf\constrained"
}
if (-not [System.IO.Path]::IsPathRooted($OutDir)) {
    $OutDir = Join-Path $repoRoot $OutDir
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
if ($Label -eq "") {
    $Label = "constrained_" + (Get-Date -Format "yyyyMMdd_HHmmss")
}
if ($ExpectedText -eq "") {
    if ($Workload -eq "version") { $ExpectedText = "0.0.1" } else { $ExpectedText = "ok" }
}

# ---------------------------------------------------------------------------
# Suite / sweep driver: spawn one isolated child run per cell.
# This block runs before Add-Type / child setup so the parent stays cheap.
# ---------------------------------------------------------------------------
function Split-Csv([string]$text) {
    return @($text -split "," | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne "" })
}

if ($Suite -or ($SweepCaps -ne "")) {
    $sweepMode = ($SweepCaps -ne "")
    if ($sweepMode) {
        $caps = @(Split-Csv $SweepCaps)
        $workloads = @($Workload)
    } else {
        $caps = @(Split-Csv $SuiteCaps)
        $workloads = @(Split-Csv $SuiteWorkloads)
    }
    if ($caps.Count -eq 0) {
        Write-Host "[constrained] ERROR: no caps in suite/sweep list"
        exit 2
    }
    $rows = New-Object System.Collections.ArrayList
    foreach ($wl in $workloads) {
        foreach ($capText in $caps) {
            $capInt = 0
            if (-not [int]::TryParse($capText, [ref]$capInt)) {
                Write-Host "[constrained] ERROR: bad cap '$capText'"
                exit 2
            }
            $childLabel = "$Label-$wl-$capInt"
            $consolePath = Join-Path $OutDir "$childLabel.console.txt"
            $jsonFile = Join-Path $OutDir "$childLabel.json"
            $childArgs = @(
                "-Pir", $Pir,
                "-MemoryMiB", "$capInt",
                "-Cores", "$Cores",
                "-IdleSeconds", "$IdleSeconds",
                "-SampleIntervalMs", "$SampleIntervalMs",
                "-StartupTimeoutSeconds", "$StartupTimeoutSeconds",
                "-ShutdownTimeoutSeconds", "$ShutdownTimeoutSeconds",
                "-Workload", $wl,
                "-OutDir", $OutDir,
                "-Label", $childLabel
            )
            if ($wl -ne "idle") {
                $expectedForChild = $ExpectedText
                if ($wl -eq "version" -and $ExpectedText -eq "ok") { $expectedForChild = "0.0.1" }
                $childArgs += @(
                    "-MockProvider",
                    "-Turns", "$Turns",
                    "-TurnTimeoutSeconds", "$TurnTimeoutSeconds",
                    "-RunTimeoutSeconds", "$RunTimeoutSeconds",
                    "-Prompt", $Prompt,
                    "-ExpectedText", $expectedForChild
                )
            }
            if ($EnforceProbeMiB -gt 0 -and $wl -eq "idle") {
                $childArgs += @("-EnforceProbeMiB", "$EnforceProbeMiB")
            }
            if ($MaxRssMiB -gt 0) { $childArgs += @("-MaxRssMiB", "$MaxRssMiB") }
            if ($MaxIdleCpuPercent -ne 5.0) { $childArgs += @("-MaxIdleCpuPercent", "$MaxIdleCpuPercent") }
            if ($Priority -ne "") { $childArgs += @("-Priority", $Priority) }

            Write-Host "[constrained] suite: workload=$wl cap=$capInt MiB ..."
            $childWatch = [System.Diagnostics.Stopwatch]::StartNew()
            & powershell -NoProfile -ExecutionPolicy Bypass -File $PSCommandPath @childArgs *> $consolePath
            $childCode = $LASTEXITCODE
            $childWatch.Stop()

            $row = [ordered]@{
                workload = $wl
                capMiB = $capInt
                verdict = "NO_JSON"
                exitCode = $null
                helloMs = $null
                firstOutputMs = $null
                seconds = $null
                rssPeakMiB = $null
                commitPeakMiB = $null
                processPeakCommitMiB = $null
                threadsMax = $null
                handlesMax = $null
                cpuPercentOfOneCore = $null
                turnsCompleted = $null
                failureKind = ""
                failures = @()
                json = $jsonFile
                console = $consolePath
                wallSeconds = [Math]::Round($childWatch.Elapsed.TotalSeconds, 2)
            }
            if (Test-Path -LiteralPath $jsonFile) {
                try {
                    $doc = Get-Content -LiteralPath $jsonFile -Raw | ConvertFrom-Json
                    $row.verdict = [string]$doc.verdict
                    $row.failureKind = [string]$doc.failureKind
                    $row.failures = @($doc.failures)
                    if ($null -ne $doc.shutdown) { $row.exitCode = $doc.shutdown.exitCode }
                    if ($null -ne $doc.startup) {
                        if ($null -ne $doc.startup.helloMs) { $row.helloMs = $doc.startup.helloMs }
                        if ($null -ne $doc.startup.firstOutputMs) { $row.firstOutputMs = $doc.startup.firstOutputMs }
                    }
                    if ($null -ne $doc.measurement) {
                        $row.seconds = $doc.measurement.seconds
                        $row.rssPeakMiB = $doc.measurement.peakRssMiB
                        $row.commitPeakMiB = $doc.measurement.peakCommitMiB
                        $row.processPeakCommitMiB = $doc.measurement.processPeakCommitMiB
                        $row.threadsMax = $doc.measurement.threadsMax
                        $row.handlesMax = $doc.measurement.handlesMax
                        $row.cpuPercentOfOneCore = $doc.measurement.cpuPercentOfOneCore
                    }
                    if ($null -ne $doc.job -and $null -ne $doc.job.peakProcessCommitMiB) {
                        if ($null -eq $row.commitPeakMiB -or $doc.job.peakProcessCommitMiB -gt $row.commitPeakMiB) {
                            $row.commitPeakMiB = $doc.job.peakProcessCommitMiB
                        }
                    }
                    if ($null -ne $doc.workload -and $null -ne $doc.workload.turnsCompleted) {
                        $row.turnsCompleted = $doc.workload.turnsCompleted
                    }
                } catch {
                    $row.verdict = "BAD_JSON"
                    $row.failures = @($_.Exception.Message)
                }
            } else {
                $row.failures = @("child produced no JSON (exit $childCode); see $consolePath")
            }
            [void]$rows.Add($row)
            Write-Host ("[constrained]   -> {0} (cap {1} MiB, {2}s, peak commit {3} MiB)" -f $row.verdict, $capInt, $row.wallSeconds, $(if ($null -eq $row.commitPeakMiB) { "n/a" } else { "{0:N2}" -f $row.commitPeakMiB }))
        }
    }

    $passCount = @($rows | Where-Object { $_.verdict -eq "PASS" }).Count
    $lowestPass = $null
    foreach ($row in $rows) {
        if ($row.verdict -eq "PASS" -and ($null -eq $lowestPass -or $row.capMiB -lt $lowestPass)) {
            $lowestPass = $row.capMiB
        }
    }
    if ($sweepMode) {
        $verdict = if ($passCount -gt 0) { "PASS" } else { "FAIL" }
    } else {
        $verdict = if ($passCount -eq $rows.Count) { "PASS" } else { "FAIL" }
    }
    $suiteSummary = [ordered]@{
        schema = 1
        kind = if ($sweepMode) { "sweep" } else { "suite" }
        verdict = $verdict
        binary = [ordered]@{ path = $Pir; bytes = (Get-Item -LiteralPath $Pir).Length; sha256 = (Get-FileHash -LiteralPath $Pir -Algorithm SHA256).Hash.ToLowerInvariant() }
        config = [ordered]@{
            caps = @($caps)
            workloads = @($workloads)
            cores = $Cores
            idleSeconds = $IdleSeconds
            turns = $Turns
            prompt = $Prompt
            expectedText = $ExpectedText
        }
        passCount = $passCount
        rowCount = $rows.Count
        lowestPassMiB = $lowestPass
        rows = @($rows)
    }
    $suiteJson = Join-Path $OutDir "$Label.suite.json"
    if ($sweepMode) { $suiteJson = Join-Path $OutDir "$Label.sweep.json" }
    [System.IO.File]::WriteAllText($suiteJson, ($suiteSummary | ConvertTo-Json -Depth 8), (New-Object System.Text.UTF8Encoding($false)))

    Write-Host ""
    Write-Host ("[constrained] {0,-10} {1,6} {2,-8} {3,10} {4,10} {5,10} {6,10} {7,10}" -f "workload", "capMiB", "verdict", "peakCommit", "rssPeak", "helloMs", "firstOutMs", "turns")
    foreach ($row in $rows) {
        Write-Host ("[constrained] {0,-10} {1,6} {2,-8} {3,10} {4,10} {5,10} {6,10} {7,10}" -f $row.workload, $row.capMiB, $row.verdict,
            $(if ($null -eq $row.commitPeakMiB) { "n/a" } else { "{0:N2}" -f $row.commitPeakMiB }),
            $(if ($null -eq $row.rssPeakMiB) { "n/a" } else { "{0:N2}" -f $row.rssPeakMiB }),
            $(if ($null -eq $row.helloMs) { "-" } else { "{0:N1}" -f $row.helloMs }),
            $(if ($null -eq $row.firstOutputMs) { "-" } else { "{0:N1}" -f $row.firstOutputMs }),
            $(if ($null -eq $row.turnsCompleted) { "-" } else { "$($row.turnsCompleted)" }))
    }
    Write-Host "[constrained] suite summary: $suiteJson"
    if ($null -ne $lowestPass) { Write-Host "[constrained] lowest passing cap: $lowestPass MiB" }
    Write-Host "[constrained] VERDICT: $verdict"
    if ($verdict -ne "PASS") {
        foreach ($row in $rows) {
            if ($row.verdict -ne "PASS") {
                Write-Host ("[constrained]   - {0} @ {1} MiB: {2}" -f $row.workload, $row.capMiB, ($row.failures -join "; "))
            }
        }
        exit 1
    }
    exit 0
}

$stdoutPath = Join-Path $OutDir "$Label.stdout.txt"
$stderrPath = Join-Path $OutDir "$Label.stderr.txt"
$jsonPath = Join-Path $OutDir "$Label.json"

$exeItem = Get-Item -LiteralPath $Pir
$exeStamp = [ordered]@{
    path = $Pir
    bytes = $exeItem.Length
    sha256 = (Get-FileHash -LiteralPath $Pir -Algorithm SHA256).Hash.ToLowerInvariant()
    lastWriteTime = $exeItem.LastWriteTime.ToString("o")
}

# ---------------------------------------------------------------------------
# Win32 helpers (PowerShell 5.1 Add-Type -> .NET Framework C#; keep C# 5 syntax)
# ---------------------------------------------------------------------------
$nativeSource = @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

namespace PirGate
{
    [StructLayout(LayoutKind.Sequential)]
    public struct FILETIME
    {
        public uint dwLowDateTime;
        public uint dwHighDateTime;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct SECURITY_ATTRIBUTES
    {
        public int nLength;
        public IntPtr lpSecurityDescriptor;
        public int bInheritHandle;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct JOBOBJECT_BASIC_LIMIT_INFORMATION
    {
        public long PerProcessUserTimeLimit;
        public long PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize;
        public UIntPtr MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass;
        public uint SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct IO_COUNTERS
    {
        public ulong ReadOperationCount;
        public ulong WriteOperationCount;
        public ulong OtherOperationCount;
        public ulong ReadTransferCount;
        public ulong WriteTransferCount;
        public ulong OtherTransferCount;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
    {
        public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
        public IO_COUNTERS IoInfo;
        public UIntPtr ProcessMemoryLimit;
        public UIntPtr JobMemoryLimit;
        public UIntPtr PeakProcessMemoryUsed;
        public UIntPtr PeakJobMemoryUsed;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct PROCESS_MEMORY_COUNTERS_EX
    {
        public uint cb;
        public uint PageFaultCount;
        public UIntPtr PeakWorkingSetSize;
        public UIntPtr WorkingSetSize;
        public UIntPtr QuotaPeakPagedPoolUsage;
        public UIntPtr QuotaPagedPoolUsage;
        public UIntPtr QuotaPeakNonPagedPoolUsage;
        public UIntPtr QuotaNonPagedPoolUsage;
        public UIntPtr PagefileUsage;
        public UIntPtr PeakPagefileUsage;
        public UIntPtr PrivateUsage;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct STARTUPINFO
    {
        public int cb;
        public string lpReserved;
        public string lpDesktop;
        public string lpTitle;
        public int dwX;
        public int dwY;
        public int dwXSize;
        public int dwYSize;
        public int dwXCountChars;
        public int dwYCountChars;
        public int dwFillAttribute;
        public int dwFlags;
        public short wShowWindow;
        public short cbReserved2;
        public IntPtr lpReserved2;
        public IntPtr hStdInput;
        public IntPtr hStdOutput;
        public IntPtr hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct PROCESS_INFORMATION
    {
        public IntPtr hProcess;
        public IntPtr hThread;
        public uint dwProcessId;
        public uint dwThreadId;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct THREADENTRY32
    {
        public uint dwSize;
        public uint cntUsage;
        public uint th32ThreadID;
        public uint th32OwnerProcessID;
        public int tpBasePri;
        public int tpDeltaPri;
        public uint dwFlags;
    }

    public sealed class JobStats
    {
        public ulong ProcessMemoryLimit;
        public ulong JobMemoryLimit;
        public ulong PeakProcessMemoryUsed;
        public ulong PeakJobMemoryUsed;
        public uint LimitFlags;
    }

    public sealed class Snapshot
    {
        public bool Alive;
        public uint ExitCode;
        public ulong UserTime100ns;
        public ulong KernelTime100ns;
        public ulong WorkingSetBytes;
        public ulong PeakWorkingSetBytes;
        public ulong PrivateBytes;
        public ulong PeakPrivateBytes;
        public uint ThreadCount;
        public uint HandleCount;
        public ulong IoReadOps;
        public ulong IoWriteOps;
        public ulong IoOtherOps;
        public bool IoOk;
        public double CrossCpuMs;
        public bool TimesOk;
        public bool MemoryOk;
    }

    public sealed class GateProcess : IDisposable
    {
        private const int JobObjectExtendedLimitInformation = 9;
        private const uint JOB_OBJECT_LIMIT_PROCESS_MEMORY = 0x00000100;
        private const uint JOB_OBJECT_LIMIT_JOB_MEMORY = 0x00000200;
        private const uint JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION = 0x00000400;
        private const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000;
        private const uint CREATE_SUSPENDED = 0x00000004;
        private const uint CREATE_NO_WINDOW = 0x08000000;
        private const uint STARTF_USESTDHANDLES = 0x00000100;
        private const uint HANDLE_FLAG_INHERIT = 0x00000001;
        private const uint GENERIC_WRITE = 0x40000000;
        private const uint FILE_SHARE_READ = 0x00000001;
        private const uint FILE_SHARE_WRITE = 0x00000002;
        private const uint CREATE_ALWAYS = 2;
        private const uint FILE_ATTRIBUTE_NORMAL = 0x00000080;
        private const uint TH32CS_SNAPTHREAD = 0x00000004;

        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        private static extern IntPtr CreateJobObjectW(IntPtr lpJobAttributes, string lpName);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool SetInformationJobObject(IntPtr hJob, int infoClass, IntPtr lpInfo, uint cbInfo);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool QueryInformationJobObject(IntPtr hJob, int infoClass, IntPtr lpInfo, uint cbInfo, IntPtr lpReturnLength);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool AssignProcessToJobObject(IntPtr hJob, IntPtr hProcess);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool TerminateJobObject(IntPtr hJob, uint exitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool TerminateProcess(IntPtr hProcess, uint exitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GetProcessTimes(IntPtr hProcess, out FILETIME creation, out FILETIME exit, out FILETIME kernel, out FILETIME user);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GetProcessHandleCount(IntPtr hProcess, out uint count);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GetProcessIoCounters(IntPtr hProcess, out IO_COUNTERS counters);

        [DllImport("psapi.dll", SetLastError = true)]
        private static extern bool GetProcessMemoryInfo(IntPtr hProcess, out PROCESS_MEMORY_COUNTERS_EX counters, uint cb);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool SetProcessAffinityMask(IntPtr hProcess, UIntPtr mask);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GetProcessAffinityMask(IntPtr hProcess, out UIntPtr processMask, out UIntPtr systemMask);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool SetPriorityClass(IntPtr hProcess, uint priorityClass);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern uint GetPriorityClass(IntPtr hProcess);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GetExitCodeProcess(IntPtr hProcess, out uint exitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern uint WaitForSingleObject(IntPtr hHandle, uint milliseconds);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool CloseHandle(IntPtr hObject);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool CreatePipe(out IntPtr hReadPipe, out IntPtr hWritePipe, ref SECURITY_ATTRIBUTES lpPipeAttributes, uint nSize);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool SetHandleInformation(IntPtr hObject, uint dwMask, uint dwFlags);

        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        private static extern IntPtr CreateFileW(string lpFileName, uint dwDesiredAccess, uint dwShareMode, ref SECURITY_ATTRIBUTES lpSecurityAttributes, uint dwCreationDisposition, uint dwFlagsAndAttributes, IntPtr hTemplateFile);

        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        private static extern bool CreateProcessW(string lpApplicationName, StringBuilder lpCommandLine, IntPtr lpProcessAttributes, IntPtr lpThreadAttributes, bool bInheritHandles, uint dwCreationFlags, IntPtr lpEnvironment, string lpCurrentDirectory, ref STARTUPINFO lpStartupInfo, out PROCESS_INFORMATION lpProcessInformation);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern uint ResumeThread(IntPtr hThread);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool WriteFile(IntPtr hFile, byte[] lpBuffer, uint nNumberOfBytesToWrite, out uint lpNumberOfBytesWritten, IntPtr lpOverlapped);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern IntPtr CreateToolhelp32Snapshot(uint dwFlags, uint th32ProcessID);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool Thread32First(IntPtr hSnapshot, ref THREADENTRY32 lpte);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool Thread32Next(IntPtr hSnapshot, ref THREADENTRY32 lpte);

        public IntPtr ProcessHandle;
        public IntPtr JobHandle;
        public IntPtr StdinWrite;
        public uint Pid;

        private GateProcess() { }

        private static string QuoteArgument(string argument)
        {
            if (argument.Length == 0) return "\"\"";
            if (argument.IndexOfAny(new char[] { ' ', '\t', '"' }) < 0) return argument;
            StringBuilder quoted = new StringBuilder();
            quoted.Append('"');
            int backslashes = 0;
            foreach (char c in argument)
            {
                if (c == '\\')
                {
                    backslashes++;
                }
                else if (c == '"')
                {
                    quoted.Append('\\', backslashes * 2 + 1);
                    quoted.Append('"');
                    backslashes = 0;
                }
                else
                {
                    quoted.Append('\\', backslashes);
                    quoted.Append(c);
                    backslashes = 0;
                }
            }
            quoted.Append('\\', backslashes * 2);
            quoted.Append('"');
            return quoted.ToString();
        }

        public static GateProcess Start(
            string exePath,
            string[] arguments,
            string workingDirectory,
            ulong memoryBytes,
            ulong jobMemoryBytes,
            ulong affinityMask,
            uint priorityClass,
            string stdoutPath,
            string stderrPath)
        {
            IntPtr job = CreateJobObjectW(IntPtr.Zero, null);
            if (job == IntPtr.Zero)
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateJobObjectW failed");
            }

            GateProcess result = new GateProcess();
            result.JobHandle = job;
            IntPtr stdinRead = IntPtr.Zero;
            IntPtr stdoutHandle = IntPtr.Zero;
            IntPtr stderrHandle = IntPtr.Zero;
            PROCESS_INFORMATION pi = new PROCESS_INFORMATION();

            try
            {
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION info = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
                uint flags = JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if (memoryBytes > 0)
                {
                    flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
                    info.ProcessMemoryLimit = new UIntPtr(memoryBytes);
                }
                if (jobMemoryBytes > 0)
                {
                    flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
                    info.JobMemoryLimit = new UIntPtr(jobMemoryBytes);
                }
                info.BasicLimitInformation.LimitFlags = flags;

                IntPtr infoPtr = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION)));
                try
                {
                    Marshal.StructureToPtr(info, infoPtr, false);
                    uint size = (uint)Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION));
                    if (!SetInformationJobObject(job, JobObjectExtendedLimitInformation, infoPtr, size))
                    {
                        throw new Win32Exception(Marshal.GetLastWin32Error(), "SetInformationJobObject(JobObjectExtendedLimitInformation) failed");
                    }
                }
                finally
                {
                    Marshal.FreeHGlobal(infoPtr);
                }

                SECURITY_ATTRIBUTES sa = new SECURITY_ATTRIBUTES();
                sa.nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES));
                sa.lpSecurityDescriptor = IntPtr.Zero;
                sa.bInheritHandle = 1;

                IntPtr stdinWrite;
                if (!CreatePipe(out stdinRead, out stdinWrite, ref sa, 0))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "CreatePipe(stdin) failed");
                }
                result.StdinWrite = stdinWrite;
                if (!SetHandleInformation(stdinWrite, HANDLE_FLAG_INHERIT, 0))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "SetHandleInformation(stdin write) failed");
                }

                stdoutHandle = CreateFileW(stdoutPath, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, ref sa, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
                if (stdoutHandle == new IntPtr(-1))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateFileW(stdout) failed");
                }
                stderrHandle = CreateFileW(stderrPath, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, ref sa, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
                if (stderrHandle == new IntPtr(-1))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateFileW(stderr) failed");
                }

                StringBuilder commandLine = new StringBuilder();
                commandLine.Append(QuoteArgument(exePath));
                if (arguments != null)
                {
                    foreach (string argument in arguments)
                    {
                        commandLine.Append(' ');
                        commandLine.Append(QuoteArgument(argument));
                    }
                }

                STARTUPINFO si = new STARTUPINFO();
                si.cb = Marshal.SizeOf(typeof(STARTUPINFO));
                si.dwFlags = (int)STARTF_USESTDHANDLES;
                si.hStdInput = stdinRead;
                si.hStdOutput = stdoutHandle;
                si.hStdError = stderrHandle;

                if (!CreateProcessW(exePath, commandLine, IntPtr.Zero, IntPtr.Zero, true,
                        CREATE_SUSPENDED | CREATE_NO_WINDOW, IntPtr.Zero, workingDirectory, ref si, out pi))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateProcessW failed");
                }

                result.ProcessHandle = pi.hProcess;
                result.Pid = pi.dwProcessId;

                if (!AssignProcessToJobObject(job, pi.hProcess))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "AssignProcessToJobObject failed (nested jobs need Windows 8+)");
                }

                if (affinityMask != 0)
                {
                    if (!SetProcessAffinityMask(pi.hProcess, new UIntPtr(affinityMask)))
                    {
                        throw new Win32Exception(Marshal.GetLastWin32Error(), "SetProcessAffinityMask failed");
                    }
                }

                if (priorityClass != 0)
                {
                    if (!SetPriorityClass(pi.hProcess, priorityClass))
                    {
                        throw new Win32Exception(Marshal.GetLastWin32Error(), "SetPriorityClass failed");
                    }
                }

                if (ResumeThread(pi.hThread) == 0xFFFFFFFF)
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "ResumeThread failed");
                }
                CloseHandle(pi.hThread);
                pi.hThread = IntPtr.Zero;

                CloseHandle(stdinRead);
                stdinRead = IntPtr.Zero;
                CloseHandle(stdoutHandle);
                stdoutHandle = IntPtr.Zero;
                CloseHandle(stderrHandle);
                stderrHandle = IntPtr.Zero;

                return result;
            }
            catch
            {
                if (stdinRead != IntPtr.Zero) CloseHandle(stdinRead);
                if (stdoutHandle != IntPtr.Zero) CloseHandle(stdoutHandle);
                if (stderrHandle != IntPtr.Zero) CloseHandle(stderrHandle);
                if (pi.hThread != IntPtr.Zero) CloseHandle(pi.hThread);
                if (pi.hProcess != IntPtr.Zero) CloseHandle(pi.hProcess);
                result.Dispose();
                throw;
            }
        }

        public bool HasExited()
        {
            return WaitForSingleObject(ProcessHandle, 0) == 0;
        }

        public bool WaitForExit(int milliseconds)
        {
            return WaitForSingleObject(ProcessHandle, (uint)milliseconds) == 0;
        }

        public uint GetExitCode()
        {
            uint code;
            if (!GetExitCodeProcess(ProcessHandle, out code))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "GetExitCodeProcess failed");
            }
            return code;
        }

        public Snapshot Sample()
        {
            Snapshot snapshot = new Snapshot();
            snapshot.Alive = !HasExited();
            snapshot.ExitCode = snapshot.Alive ? 0 : GetExitCode();

            FILETIME creation;
            FILETIME exit;
            FILETIME kernel;
            FILETIME user;
            if (GetProcessTimes(ProcessHandle, out creation, out exit, out kernel, out user))
            {
                snapshot.TimesOk = true;
                snapshot.UserTime100ns = ((ulong)user.dwHighDateTime << 32) | user.dwLowDateTime;
                snapshot.KernelTime100ns = ((ulong)kernel.dwHighDateTime << 32) | kernel.dwLowDateTime;
            }

            PROCESS_MEMORY_COUNTERS_EX counters = new PROCESS_MEMORY_COUNTERS_EX();
            counters.cb = (uint)Marshal.SizeOf(typeof(PROCESS_MEMORY_COUNTERS_EX));
            if (GetProcessMemoryInfo(ProcessHandle, out counters, counters.cb))
            {
                snapshot.MemoryOk = true;
                snapshot.WorkingSetBytes = counters.WorkingSetSize.ToUInt64();
                snapshot.PeakWorkingSetBytes = counters.PeakWorkingSetSize.ToUInt64();
                snapshot.PrivateBytes = counters.PrivateUsage.ToUInt64();
                snapshot.PeakPrivateBytes = counters.PeakPagefileUsage.ToUInt64();
            }

            IO_COUNTERS io;
            if (GetProcessIoCounters(ProcessHandle, out io))
            {
                snapshot.IoOk = true;
                snapshot.IoReadOps = io.ReadOperationCount;
                snapshot.IoWriteOps = io.WriteOperationCount;
                snapshot.IoOtherOps = io.OtherOperationCount;
            }

            uint handles;
            if (GetProcessHandleCount(ProcessHandle, out handles))
            {
                snapshot.HandleCount = handles;
            }
            try
            {
                using (Process process = Process.GetProcessById((int)Pid))
                {
                    snapshot.ThreadCount = (uint)process.Threads.Count;
                    // Cross-check for GetProcessTimes: .NET reads the same
                    // counters through its own path. A nonzero value here
                    // with a zero GetProcessTimes delta would mean the raw
                    // P/Invoke is broken, not that the child is idle.
                    snapshot.CrossCpuMs = process.TotalProcessorTime.TotalMilliseconds;
                }
            }
            catch
            {
                snapshot.ThreadCount = 0;
            }
            return snapshot;
        }

        public ulong GetAffinityMask()
        {
            UIntPtr processMask;
            UIntPtr systemMask;
            if (!GetProcessAffinityMask(ProcessHandle, out processMask, out systemMask))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "GetProcessAffinityMask failed");
            }
            return processMask.ToUInt64();
        }

        public uint GetPriority()
        {
            return GetPriorityClass(ProcessHandle);
        }

        public JobStats QueryJob()
        {
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION info = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
            IntPtr infoPtr = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION)));
            try
            {
                Marshal.StructureToPtr(info, infoPtr, false);
                uint size = (uint)Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION));
                if (!QueryInformationJobObject(JobHandle, JobObjectExtendedLimitInformation, infoPtr, size, IntPtr.Zero))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "QueryInformationJobObject failed");
                }
                info = (JOBOBJECT_EXTENDED_LIMIT_INFORMATION)Marshal.PtrToStructure(infoPtr, typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION));
            }
            finally
            {
                Marshal.FreeHGlobal(infoPtr);
            }

            JobStats stats = new JobStats();
            stats.ProcessMemoryLimit = info.ProcessMemoryLimit.ToUInt64();
            stats.JobMemoryLimit = info.JobMemoryLimit.ToUInt64();
            stats.PeakProcessMemoryUsed = info.PeakProcessMemoryUsed.ToUInt64();
            stats.PeakJobMemoryUsed = info.PeakJobMemoryUsed.ToUInt64();
            stats.LimitFlags = info.BasicLimitInformation.LimitFlags;
            return stats;
        }

        public void WriteStdin(string text)
        {
            if (StdinWrite == IntPtr.Zero)
            {
                throw new InvalidOperationException("stdin is already closed");
            }
            byte[] bytes = Encoding.UTF8.GetBytes(text);
            uint written;
            if (!WriteFile(StdinWrite, bytes, (uint)bytes.Length, out written, IntPtr.Zero))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "WriteFile(stdin) failed");
            }
        }

        public void CloseStdin()
        {
            if (StdinWrite != IntPtr.Zero)
            {
                CloseHandle(StdinWrite);
                StdinWrite = IntPtr.Zero;
            }
        }

        public void KillJob()
        {
            TerminateJobObject(JobHandle, 137);
        }

        public void Dispose()
        {
            CloseStdin();
            if (ProcessHandle != IntPtr.Zero)
            {
                CloseHandle(ProcessHandle);
                ProcessHandle = IntPtr.Zero;
            }
            if (JobHandle != IntPtr.Zero)
            {
                // Closing the job with KILL_ON_JOB_CLOSE reaps any surviving child.
                CloseHandle(JobHandle);
                JobHandle = IntPtr.Zero;
            }
        }

        private static uint CountThreads(uint pid)
        {
            // Toolhelp32 scanning every system thread costs ~0.7 s on this
            // host, which both stretches the sample cadence and starves the
            // loop; Process.Threads asks the kernel per process instead.
            try
            {
                using (Process process = Process.GetProcessById((int)pid))
                {
                    return (uint)process.Threads.Count;
                }
            }
            catch
            {
                return 0;
            }
        }
    }

    /// <summary>
    /// Minimal OpenAI-compatible chat-completions server for the print/soak
    /// legs.  It accepts any POST, ignores the body, and answers with one
    /// non-streaming-looking SSE chunk set ending in [DONE].  No network
    /// access beyond 127.0.0.1; no credential is validated.
    /// </summary>
    public sealed class MockOpenAiServer : IDisposable
    {
        private readonly TcpListener listener;
        private readonly Thread thread;
        private volatile bool running = true;
        private int requests;

        public int Port { get; private set; }
        public int Requests { get { return requests; } }

        public MockOpenAiServer()
        {
            listener = new TcpListener(IPAddress.Loopback, 0);
            listener.Start();
            Port = ((IPEndPoint)listener.LocalEndpoint).Port;
            thread = new Thread(Loop);
            thread.IsBackground = true;
            thread.Start();
        }

        private void Loop()
        {
            while (running)
            {
                TcpClient client;
                try { client = listener.AcceptTcpClient(); }
                catch { break; }
                try { Handle(client); }
                catch { }
                finally { try { client.Close(); } catch { } }
            }
        }

        private static string ReadRequestBody(NetworkStream stream, out int contentLength)
        {
            MemoryStream header = new MemoryStream();
            contentLength = 0;
            int match = 0;
            while (true)
            {
                int b = stream.ReadByte();
                if (b < 0) break;
                header.WriteByte((byte)b);
                if ((match == 0 && b == '\r') || (match == 1 && b == '\n') ||
                    (match == 2 && b == '\r') || (match == 3 && b == '\n'))
                {
                    match++;
                    if (match == 4) break;
                }
                else match = (b == '\r') ? 1 : 0;
            }
            string text = Encoding.ASCII.GetString(header.ToArray());
            foreach (string line in text.Split(new string[] { "\r\n" }, StringSplitOptions.None))
            {
                if (line.StartsWith("Content-Length:", StringComparison.OrdinalIgnoreCase))
                {
                    contentLength = int.Parse(line.Substring(15).Trim());
                }
            }
            byte[] body = new byte[contentLength];
            int read = 0;
            while (read < contentLength)
            {
                int n = stream.Read(body, read, contentLength - read);
                if (n <= 0) break;
                read += n;
            }
            return Encoding.UTF8.GetString(body);
        }

        private void Handle(TcpClient client)
        {
            client.ReceiveTimeout = 10000;
            NetworkStream stream = client.GetStream();
            int contentLength;
            ReadRequestBody(stream, out contentLength);
            Interlocked.Increment(ref requests);

            string chunk1 = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"gate-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n";
            string chunk2 = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"gate-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n";
            string done = "data: [DONE]\n\n";
            byte[] payload = Encoding.UTF8.GetBytes(chunk1 + chunk2 + done);
            string head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nContent-Length: " + payload.Length + "\r\nConnection: close\r\n\r\n";
            byte[] headBytes = Encoding.ASCII.GetBytes(head);
            stream.Write(headBytes, 0, headBytes.Length);
            stream.Write(payload, 0, payload.Length);
            stream.Flush();
        }

        public void Dispose()
        {
            running = false;
            try { listener.Stop(); } catch { }
        }
    }
}
'@

Add-Type -TypeDefinition $nativeSource -Language CSharp -ErrorAction Stop

# Hermetic agent dir: the probe must not read (or depend on) the developer's
# real ~/.pi/agent.  A local mock provider is used for print/soak; idle/version
# never send a request, so their provider can stay unreachable.
$createdAgentDir = $false
if ($AgentDir -eq "") {
    $AgentDir = Join-Path $OutDir "agent"
    $createdAgentDir = $true
}
if (-not (Test-Path -LiteralPath $AgentDir)) {
    New-Item -ItemType Directory -Force -Path $AgentDir | Out-Null
}

$useMock = $MockProvider -or ($Workload -eq "print") -or ($Workload -eq "soak")
$mock = $null
$mockPort = 9
if ($useMock) {
    if (-not $createdAgentDir) {
        Write-Host "[constrained] NOTE: -AgentDir was supplied; print/soak will use its provider configuration (no mock server is started)."
        $useMock = $false
    } else {
        try {
            $mock = New-Object PirGate.MockOpenAiServer
            $mockPort = $mock.Port
            Write-Host "[constrained] mock OpenAI provider listening on 127.0.0.1:$mockPort"
        } catch {
            Write-Host "[constrained] ERROR: could not start mock provider: $($_.Exception.Message)"
            exit 2
        }
    }
}
if ($createdAgentDir) {
    $modelsPath = Join-Path $AgentDir "models.json"
    $models = @"
{
  "providers": {
    "gate": {
      "name": "Constrained Device Gate",
      "baseUrl": "http://127.0.0.1:$mockPort/v1",
      "api": "openai-completions",
      "apiKey": "gate-no-network",
      "models": [
        { "id": "gate-model", "name": "Gate Model", "contextWindow": 8192, "maxTokens": 1024 }
      ]
    }
  }
}
"@
    [System.IO.File]::WriteAllText($modelsPath, $models, (New-Object System.Text.UTF8Encoding($false)))
    $settingsPath = Join-Path $AgentDir "settings.json"
    $settings = '{ "defaultProvider": "gate", "defaultModel": "gate-model" }'
    [System.IO.File]::WriteAllText($settingsPath, $settings, (New-Object System.Text.UTF8Encoding($false)))
}

$priorityClasses = @{
    ""           = 0
    "Idle"       = 0x00000040
    "BelowNormal"= 0x00004000
    "Normal"     = 0x00000020
    "AboveNormal"= 0x00008000
    "High"       = 0x00000080
    "RealTime"   = 0x00000100
}
$priorityValue = [uint32]$priorityClasses[$Priority]

$logicalCpus = [Environment]::ProcessorCount
if ($Cores -gt $logicalCpus) {
    Write-Host "[constrained] ERROR: -Cores $Cores exceeds this host's $logicalCpus logical processors"
    exit 2
}
$affinityMask = [uint64](([uint64]1 -shl $Cores) - 1)
$memoryBytes = [uint64]$MemoryMiB * 1MB
$jobMemoryBytes = [uint64]$JobMemoryMiB * 1MB
$rssBudgetMiB = if ($MaxRssMiB -gt 0) { [double]$MaxRssMiB } else { [double]$MemoryMiB }

$failures = New-Object System.Collections.ArrayList
function Add-Failure([string]$message) {
    [void]$failures.Add($message)
    Write-Host "[constrained] FAIL: $message"
}

function Get-Median([double[]]$values) {
    if ($values.Count -eq 0) { return $null }
    $sorted = @($values | Sort-Object)
    $mid = [int][Math]::Floor($sorted.Count / 2)
    if (($sorted.Count % 2) -eq 0) {
        return ([double]$sorted[$mid - 1] + [double]$sorted[$mid]) / 2.0
    }
    return [double]$sorted[$mid]
}

# The child's stdout file is held open for writing by CreateFileW, so the
# default File.ReadAllText share mode (FileShare.Read) collides with it.
# Open with FileShare.ReadWrite|Delete -- the same rule the probes' log readers
# need while the process is still running.
function Read-SharedText([string]$path) {
    if (-not (Test-Path -LiteralPath $path)) { return "" }
    try {
        $stream = New-Object System.IO.FileStream(
            $path,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read,
            ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
        try {
            $reader = New-Object System.IO.StreamReader($stream, [System.Text.Encoding]::UTF8)
            return $reader.ReadToEnd()
        } finally {
            $stream.Dispose()
        }
    } catch {
        return ""
    }
}

function Get-RunStats($Start, $End, $Samples, [double]$Seconds) {
    $rss = @($Samples | ForEach-Object { [double]$_.WorkingSetBytes / 1MB })
    $commit = @($Samples | ForEach-Object { [double]$_.PrivateBytes / 1MB })
    $threads = @($Samples | ForEach-Object { [int]$_.ThreadCount })
    $handles = @($Samples | ForEach-Object { [int]$_.HandleCount })
    $rssPeak = if ($rss.Count -gt 0) { ($rss | Measure-Object -Maximum).Maximum } else { $null }
    $rssMedian = Get-Median $rss
    $commitPeak = if ($commit.Count -gt 0) { ($commit | Measure-Object -Maximum).Maximum } else { $null }
    $threadsMin = if ($threads.Count -gt 0) { ($threads | Measure-Object -Minimum).Minimum } else { $null }
    $threadsMax = if ($threads.Count -gt 0) { ($threads | Measure-Object -Maximum).Maximum } else { $null }
    $handlesMin = if ($handles.Count -gt 0) { ($handles | Measure-Object -Minimum).Minimum } else { $null }
    $handlesMax = if ($handles.Count -gt 0) { ($handles | Measure-Object -Maximum).Maximum } else { $null }
    $cpuUserMs = ([double]$End.UserTime100ns - [double]$Start.UserTime100ns) / 10000.0
    $cpuKernelMs = ([double]$End.KernelTime100ns - [double]$Start.KernelTime100ns) / 10000.0
    $cpuDeltaMs = $cpuUserMs + $cpuKernelMs
    $cpuPercent = 0.0
    if ($Seconds -gt 0) { $cpuPercent = $cpuDeltaMs / ($Seconds * 1000.0) * 100.0 }
    $ioRead = [double]$End.IoReadOps - [double]$Start.IoReadOps
    $ioWrite = [double]$End.IoWriteOps - [double]$Start.IoWriteOps
    $ioOther = [double]$End.IoOtherOps - [double]$Start.IoOtherOps
    $processPeakRss = if ($End.MemoryOk) { [double]$End.PeakWorkingSetBytes / 1MB } else { $null }
    $processPeakCommit = if ($End.MemoryOk) { [double]$End.PeakPrivateBytes / 1MB } else { $null }
    $peakRss = $rssPeak
    if ($null -ne $processPeakRss -and ($null -eq $peakRss -or $processPeakRss -gt $peakRss)) { $peakRss = $processPeakRss }
    $peakCommit = $commitPeak
    if ($null -ne $processPeakCommit -and ($null -eq $peakCommit -or $processPeakCommit -gt $peakCommit)) { $peakCommit = $processPeakCommit }
    return [ordered]@{
        seconds = [Math]::Round($Seconds, 2)
        samples = $Samples.Count
        peakRssMiB = $peakRss
        peakCommitMiB = $peakCommit
        rssPeakMiB = $rssPeak
        rssMedianMiB = $rssMedian
        rssFinalMiB = [double]$End.WorkingSetBytes / 1MB
        processPeakRssMiB = $processPeakRss
        commitPeakMiB = $commitPeak
        commitFinalMiB = [double]$End.PrivateBytes / 1MB
        processPeakCommitMiB = $processPeakCommit
        threadsMin = $threadsMin
        threadsMax = $threadsMax
        threadsFinal = $End.ThreadCount
        handlesMin = $handlesMin
        handlesMax = $handlesMax
        handlesFinal = $End.HandleCount
        cpuUserMs = [Math]::Round($cpuUserMs, 3)
        cpuKernelMs = [Math]::Round($cpuKernelMs, 3)
        cpuTotalMs = [Math]::Round($cpuDeltaMs, 3)
        cpuSinceStartMs = [Math]::Round(([double]$End.UserTime100ns + [double]$End.KernelTime100ns) / 10000.0, 3)
        cpuPercentOfOneCore = [Math]::Round($cpuPercent, 4)
        ioReadOps = $ioRead
        ioWriteOps = $ioWrite
        ioOtherOps = $ioOther
        wakeupProxy = "GetProcessTimes delta + GetProcessIoCounters delta over the window (no public per-process wakeup counter on Windows)"
    }
}

function Get-OomKind([uint32]$code, [string]$stderrText, [double]$peakCommitMiB, [int]$capMiB) {
    if ($code -eq 0) { return "" }
    # 0xC0000142 = STATUS_DLL_INIT_FAILED: the loader could not allocate while
    # mapping/initializing the image, the usual tiny-cap signature for a binary
    # that dies before Rust code (and therefore before any allocator message).
    if ($code -eq [uint32]3221225794) { return "oom" }
    if ($stderrText -match "memory allocation|VirtualProtect|failed to allocate|out of memory|Out of memory|paging file") {
        return "oom"
    }
    if ($capMiB -gt 0 -and $peakCommitMiB -ge ([double]$capMiB * 0.90)) {
        return "oom"
    }
    return "exit"
}

$env:PIR_AGENT_DIR = $AgentDir
$env:PI_CODING_AGENT_DIR = $AgentDir

Write-Host "[constrained] binary: $Pir ($($exeItem.Length) bytes, mtime $($exeItem.LastWriteTime.ToString('o')))"
Write-Host "[constrained] job: processMemoryMiB=$MemoryMiB jobMemoryMiB=$JobMemoryMiB cores=$Cores mask=0x$($affinityMask.ToString('X')) priority=$(if ($Priority -eq '') { 'unchanged' } else { $Priority })"
Write-Host "[constrained] workload: $Workload$(if ($Workload -eq 'soak') { " ($Turns turns)" } elseif ($Workload -eq 'print') { " (prompt: $Prompt)" } else { '' })"
Write-Host "[constrained] agent-dir: $AgentDir"

$arguments = @()
switch ($Workload) {
    "idle"    { $arguments = @("--mode", "rpc", "--no-session", "--no-context-files") }
    "soak"    { $arguments = @("--mode", "rpc", "--no-session", "--no-context-files") }
    "print"   { $arguments = @("print", "--no-session", "--no-context-files", "--provider", "gate", "--model", "gate-model", $Prompt) }
    "version" { $arguments = @("--version") }
}

$process = $null
$shutdownMode = "not-started"
$jobStats = $null
# Initialised up front so the summary can never hit Set-StrictMode on an
# unexpected mid-run Win32 failure.
$ready = $false
$helloMs = $null
$firstOutputMs = $null
$idleSecondsActual = 0.0
$startSnapshot = $null
$endSnapshot = $null
$samples = New-Object System.Collections.ArrayList
$series = New-Object System.Collections.ArrayList
$stats = $null
$exitCode = 0
$enforce = $null
$timedOut = $false
$turnsCompleted = 0
$turnTimes = New-Object System.Collections.ArrayList
$workloadInfo = $null

$launchWatch = [System.Diagnostics.Stopwatch]::StartNew()
try {
    $process = [PirGate.GateProcess]::Start(
        $Pir, $arguments, $repoRoot,
        $memoryBytes, $jobMemoryBytes, $affinityMask, $priorityValue,
        $stdoutPath, $stderrPath)
}
catch {
    Write-Host "[constrained] LAUNCH FAILED: $($_.Exception.Message)"
    if ($process -ne $null) { $process.Dispose() }
    if ($mock -ne $null) { $mock.Dispose() }
    exit 2
}

try {
    $jobStats = $process.QueryJob()
    if (($jobStats.LimitFlags -band 0x100) -eq 0) {
        Add-Failure "job LimitFlags does not contain JOB_OBJECT_LIMIT_PROCESS_MEMORY (0x100)"
    }
    if ($jobStats.ProcessMemoryLimit -ne $memoryBytes) {
        Add-Failure "job ProcessMemoryLimit=$($jobStats.ProcessMemoryLimit) != requested $memoryBytes"
    }
    if ($JobMemoryMiB -gt 0 -and $jobStats.JobMemoryLimit -ne $jobMemoryBytes) {
        Add-Failure "job JobMemoryLimit=$($jobStats.JobMemoryLimit) != requested $jobMemoryBytes"
    }
    $actualAffinity = $process.GetAffinityMask()
    if ($actualAffinity -ne $affinityMask) {
        Add-Failure "process affinity mask=0x$($actualAffinity.ToString('X')) != requested 0x$($affinityMask.ToString('X'))"
    }
    Write-Host ("[constrained] verified: job limitFlags=0x{0:X} processMemoryLimit={1} MiB peak-so-far={2:N2} MiB; affinity=0x{3:X}" -f $jobStats.LimitFlags, ($jobStats.ProcessMemoryLimit / 1MB), ($jobStats.PeakProcessMemoryUsed / 1MB), $actualAffinity)
    if ($Priority -ne "") {
        Write-Host ("[constrained] priority class=0x{0:X}" -f $process.GetPriority())
    }

    # ------------------------------------------------------------------
    # Workload execution
    # ------------------------------------------------------------------
    if ($Workload -eq "idle" -or $Workload -eq "soak") {
        # RPC readiness: one hello line, then wait for the protocol response.
        $startupWatch = [System.Diagnostics.Stopwatch]::StartNew()
        $process.WriteStdin("{`"type`":`"hello`",`"id`":`"startup`"}`n")
        $ready = $false
        $helloMs = $null
        while ($startupWatch.Elapsed.TotalSeconds -lt $StartupTimeoutSeconds) {
            if ($process.HasExited()) { break }
            $text = Read-SharedText $stdoutPath
            if ($text.Contains("pir-rpc-jsonl")) {
                $ready = $true
                $helloMs = [Math]::Round($startupWatch.Elapsed.TotalMilliseconds, 1)
                break
            }
            Start-Sleep -Milliseconds 50
        }
        $startupWatch.Stop()
        if (-not $ready) {
            $code = if ($process.HasExited()) { $process.GetExitCode() } else { $null }
            Add-Failure ("RPC host did not answer hello within {0}s (alive={1}, exit={2})" -f $StartupTimeoutSeconds, (-not $process.HasExited()), $(if ($code -eq $null) { "n/a" } else { "0x{0:X8}" -f $code }))
        } else {
            Write-Host "[constrained] RPC hello answered in $helloMs ms (pid $($process.Pid))"
            Start-Sleep -Milliseconds 300
        }
    } elseif ($Workload -eq "print" -or $Workload -eq "version") {
        # print/version do not read stdin; close it so the child never waits.
        $process.CloseStdin()
    }

    if ($Workload -eq "idle") {
        $samples = New-Object System.Collections.ArrayList
        $series = New-Object System.Collections.ArrayList
        $startSnapshot = $process.Sample()
        $idleWatch = [System.Diagnostics.Stopwatch]::StartNew()
        if ($ready) {
            [void]$samples.Add($startSnapshot)
            [void]$series.Add([ordered]@{
                t = 0.0
                rssMiB = [Math]::Round([double]$startSnapshot.WorkingSetBytes / 1MB, 2)
                commitMiB = [Math]::Round([double]$startSnapshot.PrivateBytes / 1MB, 2)
                threads = [int]$startSnapshot.ThreadCount
                handles = [int]$startSnapshot.HandleCount
            })
            while ($idleWatch.Elapsed.TotalSeconds -lt $IdleSeconds) {
                Start-Sleep -Milliseconds $SampleIntervalMs
                $snap = $process.Sample()
                if (-not $snap.Alive) {
                    Add-Failure ("child exited during idle window (exit=0x{0:X8}); the run is not a valid idle measurement" -f $snap.ExitCode)
                    break
                }
                [void]$samples.Add($snap)
                [void]$series.Add([ordered]@{
                    t = [Math]::Round($idleWatch.Elapsed.TotalSeconds, 2)
                    rssMiB = [Math]::Round([double]$snap.WorkingSetBytes / 1MB, 2)
                    commitMiB = [Math]::Round([double]$snap.PrivateBytes / 1MB, 2)
                    threads = [int]$snap.ThreadCount
                    handles = [int]$snap.HandleCount
                })
            }
        }
        $idleWatch.Stop()
        $endSnapshot = $process.Sample()
        $idleSecondsActual = [Math]::Round($idleWatch.Elapsed.TotalSeconds, 2)
        if ($process.HasExited()) { $exitCode = $process.GetExitCode() }
        $stats = Get-RunStats $startSnapshot $endSnapshot $samples $idleSecondsActual
        if ($ready) {
            Write-Host ("[constrained] idle {0}s: RSS peak={1:N2} MiB median={2:N2} final={3:N2}; commit peak={4:N2} final={5:N2} MiB" -f $idleSecondsActual, $stats.rssPeakMiB, $stats.rssMedianMiB, $stats.rssFinalMiB, $stats.commitPeakMiB, $stats.commitFinalMiB)
            Write-Host ("[constrained] threads min/max/final={0}/{1}/{2}; handles min/max/final={3}/{4}/{5}" -f $stats.threadsMin, $stats.threadsMax, $stats.threadsFinal, $stats.handlesMin, $stats.handlesMax, $stats.handlesFinal)
            Write-Host ("[constrained] cpu delta: user={0:N2} ms kernel={1:N2} ms total={2:N2} ms => {3:N3}% of one core; io ops read/write/other={4}/{5}/{6}" -f $stats.cpuUserMs, $stats.cpuKernelMs, $stats.cpuTotalMs, $stats.cpuPercentOfOneCore, $stats.ioReadOps, $stats.ioWriteOps, $stats.ioOtherOps)
            Write-Host "[constrained] note: Windows has no public per-process wakeup counter; CPU-time delta plus IO-operation delta are the proxies (a polling idle loop shows non-zero kernel time and/or IO ops)."
        }
    } elseif ($Workload -eq "version") {
        $samples = New-Object System.Collections.ArrayList
        $series = New-Object System.Collections.ArrayList
        $startSnapshot = $process.Sample()
        [void]$samples.Add($startSnapshot)
        $exited = $process.WaitForExit($RunTimeoutSeconds * 1000)
        $launchWatch.Stop()
        if (-not $exited) {
            $timedOut = $true
            $process.KillJob()
            [void]$process.WaitForExit(5000)
        }
        $endSnapshot = $process.Sample()
        $stats = Get-RunStats $startSnapshot $endSnapshot $samples $launchWatch.Elapsed.TotalSeconds
        $firstOutputMs = [Math]::Round($launchWatch.Elapsed.TotalMilliseconds, 1)
        $exitCode = $process.GetExitCode()
        Write-Host ("[constrained] version run: {0:N1} ms, exit=0x{1:X8}, peak commit={2:N2} MiB" -f $firstOutputMs, $exitCode, $stats.processPeakCommitMiB)
    } elseif ($Workload -eq "print") {
        $samples = New-Object System.Collections.ArrayList
        $series = New-Object System.Collections.ArrayList
        $startSnapshot = $process.Sample()
        [void]$samples.Add($startSnapshot)
        [void]$series.Add([ordered]@{ t = 0.0; rssMiB = [Math]::Round([double]$startSnapshot.WorkingSetBytes / 1MB, 2); commitMiB = [Math]::Round([double]$startSnapshot.PrivateBytes / 1MB, 2); threads = [int]$startSnapshot.ThreadCount; handles = [int]$startSnapshot.HandleCount })
        $runInterval = [Math]::Min($SampleIntervalMs, 20)
        while (-not $process.HasExited()) {
            if ($launchWatch.Elapsed.TotalSeconds -gt $RunTimeoutSeconds) {
                $timedOut = $true
                Add-Failure ("print run exceeded {0}s; job terminated" -f $RunTimeoutSeconds)
                $process.KillJob()
                [void]$process.WaitForExit(5000)
                break
            }
            Start-Sleep -Milliseconds $runInterval
            $snap = $process.Sample()
            [void]$samples.Add($snap)
            [void]$series.Add([ordered]@{ t = [Math]::Round($launchWatch.Elapsed.TotalSeconds, 2); rssMiB = [Math]::Round([double]$snap.WorkingSetBytes / 1MB, 2); commitMiB = [Math]::Round([double]$snap.PrivateBytes / 1MB, 2); threads = [int]$snap.ThreadCount; handles = [int]$snap.HandleCount })
            if ($null -eq $firstOutputMs) {
                $text = Read-SharedText $stdoutPath
                if ($text.Length -gt 0) {
                    $firstOutputMs = [Math]::Round($launchWatch.Elapsed.TotalMilliseconds, 1)
                }
            }
        }
        $launchWatch.Stop()
        $endSnapshot = $process.Sample()
        $stats = Get-RunStats $startSnapshot $endSnapshot $samples $launchWatch.Elapsed.TotalSeconds
        $exitCode = $process.GetExitCode()
        if ($null -eq $firstOutputMs) { $firstOutputMs = [Math]::Round($stats.seconds * 1000.0, 1) }
        Write-Host ("[constrained] print run: {0:N0} ms, first output {1}, exit=0x{2:X8}, peak commit={3:N2} MiB, RSS peak={4:N2} MiB" -f ($stats.seconds * 1000.0), $(if ($null -eq $firstOutputMs) { "n/a" } else { "$firstOutputMs ms" }), $exitCode, $stats.processPeakCommitMiB, $stats.rssPeakMiB)
    } elseif ($Workload -eq "soak") {
        $samples = New-Object System.Collections.ArrayList
        $series = New-Object System.Collections.ArrayList
        $startSnapshot = $process.Sample()
        $launchWatch.Restart()
        [void]$samples.Add($startSnapshot)
        if ($ready) {
            for ($turn = 1; $turn -le $Turns; $turn++) {
                if ($process.HasExited()) { break }
                $beforeText = Read-SharedText $stdoutPath
                $beforeCount = ([regex]::Matches($beforeText, '"type":"agent_settled"')).Count
                $promptLine = "{`"type`":`"prompt`",`"id`":`"turn$turn`",`"text`":`"$Prompt`"}`n"
                try {
                    $process.WriteStdin($promptLine)
                } catch {
                    Add-Failure "soak turn $turn could not be written: $($_.Exception.Message)"
                    break
                }
                $turnWatch = [System.Diagnostics.Stopwatch]::StartNew()
                $settled = $false
                while ($turnWatch.Elapsed.TotalSeconds -lt $TurnTimeoutSeconds) {
                    Start-Sleep -Milliseconds 50
                    $snap = $process.Sample()
                    [void]$samples.Add($snap)
                    [void]$series.Add([ordered]@{ t = [Math]::Round($launchWatch.Elapsed.TotalSeconds, 2); turn = $turn; rssMiB = [Math]::Round([double]$snap.WorkingSetBytes / 1MB, 2); commitMiB = [Math]::Round([double]$snap.PrivateBytes / 1MB, 2); threads = [int]$snap.ThreadCount; handles = [int]$snap.HandleCount })
                    if (-not $snap.Alive) { break }
                    $text = Read-SharedText $stdoutPath
                    $count = ([regex]::Matches($text, '"type":"agent_settled"')).Count
                    if ($count -gt $beforeCount) { $settled = $true; break }
                }
                $turnWatch.Stop()
                if ($settled) {
                    $turnsCompleted = $turn
                    [void]$turnTimes.Add([Math]::Round($turnWatch.Elapsed.TotalMilliseconds, 1))
                    # The next prompt may only be claimed after the settled
                    # edge has been published; a short pause avoids racing the
                    # prompt-claim release.
                    Start-Sleep -Milliseconds 100
                } else {
                    $code = if ($process.HasExited()) { $process.GetExitCode() } else { $null }
                    Add-Failure ("soak turn {0} did not settle within {1}s (alive={2}, exit={3})" -f $turn, $TurnTimeoutSeconds, (-not $process.HasExited()), $(if ($code -eq $null) { "n/a" } else { "0x{0:X8}" -f $code }))
                    if (-not $process.HasExited()) { $process.KillJob(); [void]$process.WaitForExit(5000) }
                    break
                }
            }
            if ($ready -and $turnsCompleted -lt $Turns) {
                Add-Failure ("soak completed {0}/{1} turns" -f $turnsCompleted, $Turns)
            }
        }
        $launchWatch.Stop()
        $endSnapshot = $process.Sample()
        $stats = Get-RunStats $startSnapshot $endSnapshot $samples $launchWatch.Elapsed.TotalSeconds
        $exitCode = $process.GetExitCode()
        Write-Host ("[constrained] soak run: {0:N1} s, turns {1}/{2}, peak commit={3:N2} MiB, RSS peak={4:N2} MiB" -f $stats.seconds, $turnsCompleted, $Turns, $stats.processPeakCommitMiB, $stats.rssPeakMiB)
    }

    # ------------------------------------------------------------------
    # Workload verdict + failure classification
    # ------------------------------------------------------------------
    $stderrText = Read-SharedText $stderrPath
    $stdoutText = Read-SharedText $stdoutPath
    $stderrTail = if ($stderrText.Length -gt 600) { $stderrText.Substring($stderrText.Length - 600) } else { $stderrText }
    $expectedFound = $false
    $failureKind = ""
    $childAliveAtVerdict = -not $process.HasExited()
    if ($null -ne $stats) {
        if (-not $childAliveAtVerdict) {
            $failureKind = Get-OomKind ([uint32]$exitCode) $stderrText ([double]$stats.processPeakCommitMiB) $MemoryMiB
        }
    }
    if ($timedOut) { $failureKind = "timeout" }

    if ($Workload -eq "idle") {
        if (-not $startSnapshot.TimesOk -or -not $endSnapshot.TimesOk) {
            Add-Failure "GetProcessTimes did not return CPU time for both ends of the idle window"
        }
        if ($null -ne $stats.peakRssMiB -and $stats.peakRssMiB -gt $rssBudgetMiB) {
            Add-Failure ("idle RSS peak {0:N2} MiB exceeds budget {1:N2} MiB" -f $stats.peakRssMiB, $rssBudgetMiB)
        }
        if ($null -ne $stats.peakCommitMiB -and $stats.peakCommitMiB -gt $MemoryMiB) {
            Add-Failure ("idle commit peak {0:N2} MiB exceeds Job process cap {1} MiB" -f $stats.commitPeakMiB, $MemoryMiB)
        }
        if ($stats.cpuPercentOfOneCore -gt $MaxIdleCpuPercent) {
            Add-Failure ("idle CPU {0:N3}% of one core exceeds budget {1:N3}%" -f $stats.cpuPercentOfOneCore, $MaxIdleCpuPercent)
        }
    } else {
        if ($null -ne $stats.peakRssMiB -and $stats.peakRssMiB -gt $rssBudgetMiB) {
            Add-Failure ("RSS peak {0:N2} MiB exceeds budget {1:N2} MiB" -f $stats.peakRssMiB, $rssBudgetMiB)
        }
        if ($null -ne $stats.peakCommitMiB -and $stats.peakCommitMiB -gt $MemoryMiB) {
            Add-Failure ("commit peak {0:N2} MiB exceeds Job process cap {1} MiB" -f $stats.commitPeakMiB, $MemoryMiB)
        }
        if ($timedOut) {
            # failure already recorded
        } elseif ($childAliveAtVerdict) {
            # The process is still running (soak/idle): exit status is checked
            # after the graceful-shutdown block below.
        } elseif ($exitCode -ne 0) {
            Add-Failure ("{0} exited 0x{1:X8} under a {2} MiB cap (failureKind={3}); stderr tail: {4}" -f $Workload, $exitCode, $MemoryMiB, $(if ($failureKind -eq "") { "exit" } else { $failureKind }), $(if ($stderrTail -eq "") { "<empty>" } else { $stderrTail.Trim() }))
            if ($failureKind -eq "") { $failureKind = "exit" }
        } else {
            if ($Workload -eq "print") {
                $expectedFound = $stdoutText.Contains($ExpectedText)
                if (-not $expectedFound) {
                    Add-Failure ("print stdout does not contain expected text '$ExpectedText' (stdout: {0})" -f ($stdoutText.Trim()))
                    $failureKind = "output"
                }
            } elseif ($Workload -eq "soak") {
                if ($turnsCompleted -lt $Turns) {
                    $failureKind = "turns"
                }
            } elseif ($Workload -eq "version") {
                $expectedFound = $stdoutText.Contains($ExpectedText)
                if (-not $expectedFound) {
                    Add-Failure ("version stdout does not contain expected text '$ExpectedText' (stdout: {0})" -f ($stdoutText.Trim()))
                    $failureKind = "output"
                }
            }
        }
    }

    $workloadInfo = [ordered]@{
        kind = $Workload
        prompt = if ($Workload -eq "print" -or $Workload -eq "soak") { $Prompt } else { $null }
        expectedText = $ExpectedText
        expectedTextFound = $expectedFound
        turnsRequested = if ($Workload -eq "soak") { $Turns } else { $null }
        turnsCompleted = if ($Workload -eq "soak") { $turnsCompleted } else { $null }
        turnTimesMs = @($turnTimes)
        mockRequests = if ($mock -ne $null) { $mock.Requests } else { $null }
        timedOut = $timedOut
        failureKind = $failureKind
        stderrTail = $stderrTail
        stdoutBytes = (Get-Item -LiteralPath $stdoutPath).Length
        series = @($series)
    }

    # Graceful shutdown: close stdin -> RPC loop sees EOF and exits.
    if (-not $process.HasExited()) {
        $process.CloseStdin()
        if ($process.WaitForExit($ShutdownTimeoutSeconds * 1000)) {
            $shutdownMode = "stdin-eof"
        } else {
            $shutdownMode = "forced"
            Add-Failure ("child did not exit within {0}s of stdin EOF; job terminated" -f $ShutdownTimeoutSeconds)
            $process.KillJob()
            [void]$process.WaitForExit(5000)
        }
    } else {
        $shutdownMode = "already-exited"
    }
    $exitCode = $process.GetExitCode()
    if ($shutdownMode -ne "already-exited" -and $exitCode -ne 0) {
        Add-Failure ("child exit code 0x{0:X8} ({1}) is not success" -f $exitCode, $exitCode)
        if ($failureKind -eq "") {
            $failureKind = Get-OomKind ([uint32]$exitCode) $stderrText ([double]$stats.processPeakCommitMiB) $MemoryMiB
            if ($failureKind -eq "") { $failureKind = "exit" }
        }
    }
    if ($null -ne $workloadInfo) { $workloadInfo.failureKind = $failureKind }

    $jobStats = $process.QueryJob()
    Write-Host ("[constrained] job accounting: peakProcessCommit={0:N2} MiB (limit {1} MiB), peakJobCommit={2:N2} MiB (limit {3} MiB), shutdown={4}, exit=0x{5:X8}" -f ($jobStats.PeakProcessMemoryUsed / 1MB), $MemoryMiB, ($jobStats.PeakJobMemoryUsed / 1MB), $JobMemoryMiB, $shutdownMode, $exitCode)
    if ($jobStats.PeakProcessMemoryUsed -gt $memoryBytes) {
        Add-Failure ("job recorded peak process commit {0} > limit {1}; the cap was not effective" -f $jobStats.PeakProcessMemoryUsed, $memoryBytes)
    }

    # ------------------------------------------------------------------
    # Enforcement probe: a tiny cap must produce a hard, non-zero exit.
    # ------------------------------------------------------------------
    $enforce = $null
    if ($EnforceProbeMiB -gt 0) {
        Write-Host "[constrained] enforcement probe: starting the same binary under a $EnforceProbeMiB MiB job cap (must die non-zero)"
        $probeOut = Join-Path $OutDir "$Label.enforce.stdout.txt"
        $probeErr = Join-Path $OutDir "$Label.enforce.stderr.txt"
        $probe = $null
        try {
            $probeArgs = @("--mode", "rpc", "--no-session", "--no-context-files")
            $probe = [PirGate.GateProcess]::Start(
                $Pir, $probeArgs, $repoRoot,
                ([uint64]$EnforceProbeMiB * 1MB), 0, $affinityMask, $priorityValue,
                $probeOut, $probeErr)
            $probeWatch = [System.Diagnostics.Stopwatch]::StartNew()
            # The tiny cap can kill the child before the pipe accepts a line;
            # that is the expected outcome, not a probe error.
            try {
                $probe.WriteStdin("{`"type`":`"hello`",`"id`":`"probe`"}`n")
            } catch {
            }
            $exited = $probe.WaitForExit(30000)
            $probeWatch.Stop()
            $probeExit = $probe.GetExitCode()
            $probeJob = $probe.QueryJob()
            $probeStderr = Read-SharedText $probeErr
            $probeKind = Get-OomKind ([uint32]$probeExit) $probeStderr ([double]$probeJob.PeakProcessMemoryUsed / 1MB) $EnforceProbeMiB
            $enforce = [ordered]@{
                capMiB = $EnforceProbeMiB
                exited = $exited
                exitCode = $probeExit
                exitKey = ("0x{0:X8}" -f $probeExit)
                waitedSeconds = [Math]::Round($probeWatch.Elapsed.TotalSeconds, 2)
                peakProcessCommitMiB = [Math]::Round($probeJob.PeakProcessMemoryUsed / 1MB, 2)
                failureKind = $probeKind
                stderrTail = if ($probeStderr.Length -gt 300) { $probeStderr.Substring($probeStderr.Length - 300) } else { $probeStderr }
            }
            if (-not $exited) {
                Add-Failure "enforcement probe survived 30s under a $EnforceProbeMiB MiB cap (limit not biting)"
            } elseif ($probeExit -eq 0) {
                Add-Failure "enforcement probe exited 0 under a $EnforceProbeMiB MiB cap (limit silently ignored)"
            } else {
                Write-Host ("[constrained] enforcement probe: died after {0}s with exit=0x{1:X8} ({2}), peak commit={3:N2} MiB -- cap is enforced" -f $enforce.waitedSeconds, $probeExit, $probeExit, $enforce.peakProcessCommitMiB)
            }
        } catch {
            Add-Failure "enforcement probe could not run: $($_.Exception.Message)"
        } finally {
            if ($probe -ne $null) { $probe.Dispose() }
        }
    }
}
catch {
    Add-Failure "unexpected probe failure: $($_.Exception.Message)"
}
finally {
    if ($process -ne $null) { $process.Dispose() }
    if ($mock -ne $null) { $mock.Dispose() }
}

$verdict = if ($failures.Count -eq 0) { "PASS" } else { "FAIL" }
$overallFailureKind = ""
if ($null -ne $workloadInfo -and $workloadInfo.failureKind -ne "") { $overallFailureKind = $workloadInfo.failureKind }
if ($overallFailureKind -eq "" -and $failures.Count -gt 0) { $overallFailureKind = "probe" }

$summary = [ordered]@{
    schema = 2
    verdict = $verdict
    failureKind = $overallFailureKind
    failures = @($failures)
    binary = $exeStamp
    config = [ordered]@{
        workload = $Workload
        memoryMiB = $MemoryMiB
        jobMemoryMiB = $JobMemoryMiB
        cores = $Cores
        affinityMask = ("0x{0:X}" -f $affinityMask)
        priority = $Priority
        idleSecondsRequested = $IdleSeconds
        sampleIntervalMs = $SampleIntervalMs
        maxRssMiB = $rssBudgetMiB
        maxIdleCpuPercent = $MaxIdleCpuPercent
        enforceProbeMiB = $EnforceProbeMiB
        turns = if ($Workload -eq "soak") { $Turns } else { $null }
        turnTimeoutSeconds = $TurnTimeoutSeconds
        runTimeoutSeconds = $RunTimeoutSeconds
        prompt = if ($Workload -eq "print" -or $Workload -eq "soak") { $Prompt } else { $null }
        expectedText = $ExpectedText
        mockProvider = ($mock -ne $null)
        agentDir = $AgentDir
    }
    job = if ($jobStats -ne $null) {
        [ordered]@{
            limitFlags = ("0x{0:X}" -f $jobStats.LimitFlags)
            processMemoryLimitBytes = $jobStats.ProcessMemoryLimit
            jobMemoryLimitBytes = $jobStats.JobMemoryLimit
            peakProcessCommitMiB = [Math]::Round($jobStats.PeakProcessMemoryUsed / 1MB, 2)
            peakJobCommitMiB = [Math]::Round($jobStats.PeakJobMemoryUsed / 1MB, 2)
        }
    } else { $null }
    startup = [ordered]@{
        rpcReady = $ready
        helloMs = $helloMs
        firstOutputMs = $firstOutputMs
        pid = $process.Pid
    }
    measurement = $stats
    workload = $workloadInfo
    idle = if ($Workload -eq "idle") { $stats } else { $null }
    shutdown = [ordered]@{
        mode = $shutdownMode
        exitCode = $exitCode
        exitKey = ("0x{0:X8}" -f $exitCode)
    }
    enforcement = $enforce
    outputs = [ordered]@{
        stdout = $stdoutPath
        stderr = $stderrPath
        json = $jsonPath
    }
}
$jsonText = $summary | ConvertTo-Json -Depth 10
[System.IO.File]::WriteAllText($jsonPath, $jsonText, (New-Object System.Text.UTF8Encoding($false)))

Write-Host "[constrained] summary: $jsonPath"
Write-Host "[constrained] VERDICT: $verdict"
if ($verdict -ne "PASS") {
    foreach ($failure in $failures) {
        Write-Host "[constrained]   - $failure"
    }
    exit 1
}
exit 0
