<#
.SYNOPSIS
Runs the workspace tests on a Windows host (docs/design/daemon-lifecycle.md §7). Run it with
PowerShell 7 (`pwsh`).

.DESCRIPTION
cargo puts itself and the processes of `cargo test` in a kill-on-close job of its own that
allows no breakaway when it can set one up (as on the GitHub-hosted runner), so a
`daemon start` under it is refused by design (§2.12 A-W1). The tests are therefore split into
two lanes, and both must pass:

  -Lane Cargo  every test except those that need a successful detached start, under cargo.
  -Lane Wmi    builds the test executables of those tests and runs them from a wrapper
               started through WMI `Win32_Process.Create` with `CREATE_BREAKAWAY_FROM_JOB`,
               outside any job. The wrapper first records its identity and job state; the lane
               fails if it is in a job, if a check of its profile or access fails, if a
               required test did not run and pass by name, if an output file is missing, if
               the final status is not `finished`, or on timeout. Its cleanup runs on every
               ending and is checked against a daemon the wrapper leaves running on purpose.

-Lane Wrapper is the wrapper itself; only the Wmi lane starts it.
#>
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Cargo', 'Wmi', 'Wrapper')]
    [string] $Lane,
    # Wrapper: the run directory the Wmi lane created.
    [string] $RunDir,
    # Wmi: how long the wrapper may run.
    [int] $TimeoutMinutes = 20
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot

# The A-W1 smoke test: it runs first in the Wmi lane and decides it.
$AW1 = 'host::windows::tests::runner_can_break_away'
# Every `daemon_start` case, by name: its harness exits 0 with none selected, so a removed or
# renamed case must fail the lane instead of dropping out of it.
$StartTests = @(
    'reports_ready_url',
    'fail_arrives_after_the_lock_decision',
    'stopped_before_ready',
    'concurrent_starts',
    'concurrent_first_start_at_marker',
    'loser_sees_winner_exit',
    'explicit_port_conflict',
    'eof_follow_up_within_deadline',
    'timeout_after_15_s_leaves_child',
    'late_child_serves',
    'stopped_after_ready_is_not_reported_started',
    'stopping_winner_is_not_reported_running',
    'stopping_after_the_answer_is_not_reported_started',
    'detached_log_is_redacted',
    'startup_failure_output_is_redacted',
    'child_receives_the_resolved_dir'
)
# `daemon_windows` tests that need a successful detached start. The nested-job case is here too:
# under cargo's own job a refusal would come from that job, not from the jobs the test builds.
$WmiWindowsTests = @(
    'start_breaks_away_from_a_kill_on_close_job_with_breakaway',
    'start_breaks_away_from_a_kill_on_close_job_with_silent_breakaway',
    'daemon_holds_no_caller_handle',
    'detached_child_has_no_console',
    'nested_outer_job_refuses'
)
# The environment the wrapper gets: WMI otherwise gives it the provider's, not this step's.
$PassedVariables = @(
    'SystemRoot', 'windir', 'SystemDrive', 'ComSpec', 'PATHEXT', 'PATH', 'OS',
    'USERPROFILE', 'LOCALAPPDATA', 'APPDATA', 'USERNAME', 'USERDOMAIN', 'HOMEDRIVE', 'HOMEPATH',
    'ProgramData', 'ProgramFiles', 'ProgramFiles(x86)', 'CommonProgramFiles',
    'NUMBER_OF_PROCESSORS', 'PROCESSOR_ARCHITECTURE', 'RUST_BACKTRACE', 'RUNNER_TRACKING_ID'
)
# The cli test fixture creates every test directory under this run-owned root when it is set.
$TestRootVariable = 'CHALDEA_TEST_ROOT'
# How long the whole cleanup may take.
$CleanupSeconds = 120

function Invoke-CargoLane {
    # The owner the token gives new objects (an elevated administrator's: Administrators).
    Write-Host "token default owner: $([Security.Principal.WindowsIdentity]::GetCurrent().Owner.Value)"
    $failed = @()
    # `'--'` is quoted so that PowerShell passes it to cargo.
    cargo test --workspace --exclude chaldea-cli --locked --no-fail-fast '--' --exact --skip $AW1
    if ($LASTEXITCODE -ne 0) { $failed += 'the workspace without chaldea-cli' }

    $meta = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw "cargo metadata exited $LASTEXITCODE" }
    $cli = $meta.packages | Where-Object { $_.name -eq 'chaldea-cli' }
    $targets = $cli.targets | Where-Object { $_.kind -contains 'test' } | ForEach-Object { $_.name }
    $cargoArgs = @('test', '-p', 'chaldea-cli', '--locked', '--no-fail-fast', '--lib', '--bins')
    foreach ($t in $targets) {
        if ($t -notin @('daemon_start', 'daemon_windows')) { $cargoArgs += @('--test', $t) }
    }
    cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { $failed += 'chaldea-cli' }
    cargo test -p chaldea-cli --locked --doc
    if ($LASTEXITCODE -ne 0) { $failed += 'chaldea-cli doc tests' }

    $skip = foreach ($t in $WmiWindowsTests) { '--skip'; $t }
    cargo test -p chaldea-cli --locked --test daemon_windows '--' --exact @skip
    if ($LASTEXITCODE -ne 0) { $failed += 'daemon_windows' }

    if ($failed.Count -gt 0) { throw "failed: $($failed -join ', ')" }
}

# The executables of a `cargo test --no-run` build, from its JSON messages.
function Get-Artifacts([string[]] $CargoArgs) {
    $lines = cargo test @CargoArgs --locked --no-run --message-format=json
    if ($LASTEXITCODE -ne 0) { throw "cargo test $($CargoArgs -join ' ') --no-run exited $LASTEXITCODE" }
    $lines | ForEach-Object { $_ | ConvertFrom-Json } |
        Where-Object { $_.reason -eq 'compiler-artifact' -and $_.executable }
}

function Select-Executable($Artifacts, [string] $Name, [string] $Kind, [bool] $Test) {
    $exe = $Artifacts |
        Where-Object { $_.target.name -eq $Name -and $_.target.kind -contains $Kind -and $_.profile.test -eq $Test } |
        ForEach-Object { $_.executable } | Select-Object -First 1
    if (-not $exe) { throw "no executable for $Kind $Name (test: $Test)" }
    $exe
}

# Whether two clock readings of one process's start agree: CIM and .NET differ in precision.
function Test-SameStart([datetime] $A, [datetime] $B) {
    [Math]::Abs(($A.ToUniversalTime() - $B.ToUniversalTime()).TotalSeconds) -lt 1
}

# The run's processes in a CIM snapshot, by identity recorded independently of live ancestry:
# a process the wrapper recorded (id and start), one of the run's executables whose command line
# names the run root (a detached daemon of a state directory under it, whatever became of its
# starter), and a run executable whose recorded parent is one of these and started after it.
function Find-RunProcesses($Snapshot, $Recorded, [string[]] $Exes, [string] $RunRoot) {
    $ours = @{}
    do {
        $grew = $false
        foreach ($p in $Snapshot) {
            $id = [uint32]$p.ProcessId
            if ($ours.ContainsKey($id)) { continue }
            $line = [string]$p.CommandLine
            $isExe = $p.ExecutablePath -and ($p.ExecutablePath -in $Exes)
            $known = $Recorded | Where-Object { $_.Id -eq $id -and (Test-SameStart $_.Start $p.CreationDate) }
            $parent = $ours[[uint32]$p.ParentProcessId]
            if ($known -or
                ($isExe -and $line.Contains($RunRoot)) -or
                ($isExe -and $parent -and $p.CreationDate -ge $parent.CreationDate)) {
                $ours[$id] = $p
                $grew = $true
            }
        }
    } while ($grew)
    @($ours.Values)
}

# The cleanup, on every ending of the lane, within one deadline: identifies the run's
# processes and opens each (keeping the handle, so a reused id is never the one terminated),
# runs `chaldea daemon stop` for every state directory under the run root, terminates the
# identified processes still alive, and removes the run root. Returns what it found and did.
function Invoke-Cleanup($Wrapper, [string] $RunDir, [string] $RunRoot, [string] $Chaldea, [string[]] $Exes) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $left = { [int][Math]::Max(0, $CleanupSeconds * 1000 - $clock.ElapsedMilliseconds) }
    $report = [ordered]@{ found = @(); stopped_dirs = @(); terminated = @(); alive_after = @(); errors = @() }

    $recorded = @()
    if ($Wrapper) { $recorded += [pscustomobject]@{ Id = [uint32]$Wrapper.Id; Start = $Wrapper.StartTime } }
    $procsFile = Join-Path $RunDir 'procs.txt'
    if (Test-Path -LiteralPath $procsFile) {
        foreach ($l in Get-Content -LiteralPath $procsFile) {
            $f = $l -split ' '
            if ($f.Count -eq 2) {
                $recorded += [pscustomobject]@{ Id = [uint32]$f[0]; Start = [datetime]::new([long]$f[1], 'Utc') }
            }
        }
    }
    $held = @()
    try {
        $snapshot = Get-CimInstance Win32_Process -OperationTimeoutSec ([Math]::Max(1, [int]((& $left) / 1000)))
        foreach ($p in Find-RunProcesses $snapshot $recorded $Exes $RunRoot) {
            try {
                $proc = [Diagnostics.Process]::GetProcessById([int]$p.ProcessId)
                $null = $proc.Handle
                if (Test-SameStart $proc.StartTime $p.CreationDate) {
                    $held += $proc
                    $report.found += [ordered]@{ pid = $p.ProcessId; path = $p.ExecutablePath; command = $p.CommandLine }
                }
                else { $proc.Dispose() }
            }
            catch { }
        }
    }
    catch { $report.errors += "identification: $_" }

    if (Test-Path -LiteralPath $RunRoot) {
        $dirs = @(Get-ChildItem -LiteralPath $RunRoot -Recurse -Directory -Force -ErrorAction SilentlyContinue |
            Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'daemon.lock') } |
            ForEach-Object { $_.FullName })
        foreach ($d in $dirs) {
            if ((& $left) -eq 0) { $report.errors += "the cleanup deadline passed before $d"; break }
            try {
                $psi = [Diagnostics.ProcessStartInfo]::new($Chaldea)
                foreach ($a in @('--state-dir', $d, 'daemon', 'stop')) { $psi.ArgumentList.Add($a) }
                $psi.UseShellExecute = $false
                $psi.RedirectStandardOutput = $true
                $psi.RedirectStandardError = $true
                $stop = [Diagnostics.Process]::Start($psi)
                $out = $stop.StandardOutput.ReadToEndAsync()
                $err = $stop.StandardError.ReadToEndAsync()
                if ($stop.WaitForExit((& $left))) {
                    $null = $out.Wait(5000); $null = $err.Wait(5000)
                    $report.stopped_dirs += "$d -> exit $($stop.ExitCode): $(($out.Result + $err.Result).Trim())"
                }
                else {
                    $stop.Kill()
                    $report.errors += "daemon stop for $d did not finish before the cleanup deadline"
                }
            }
            catch { $report.errors += "daemon stop for ${d}: $_" }
        }
    }

    # The wrapper last, so it cannot start anything after its children were stopped.
    foreach ($p in @($held | Sort-Object { $Wrapper -and $_.Id -eq $Wrapper.Id })) {
        try {
            if (-not $p.HasExited) {
                $p.Kill()
                $report.terminated += "$($p.Id)"
            }
        }
        catch { $report.errors += "terminate $($p.Id): $_" }
    }
    foreach ($p in $held) {
        try {
            if (-not $p.WaitForExit((& $left))) { $report.alive_after += "$($p.Id)" }
        }
        catch { $report.errors += "wait $($p.Id): $_" }
        $p.Dispose()
    }
    try { Remove-Item -LiteralPath $RunRoot -Recurse -Force -ErrorAction Stop }
    catch { $report.errors += "the run root was not removed: $_" }
    $report
}

function Invoke-WmiLane {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $cutoff = [TimeSpan]::FromMinutes($TimeoutMinutes)
    $pty = Get-Artifacts @('-p', 'chaldea-pty-runtime', '--lib')
    $cli = Get-Artifacts @('-p', 'chaldea-cli', '--test', 'daemon_start', '--test', 'daemon_windows')
    $ptyExe = Select-Executable $pty 'chaldea_pty_runtime' 'lib' $true
    $startExe = Select-Executable $cli 'daemon_start' 'test' $true
    $windowsExe = Select-Executable $cli 'daemon_windows' 'test' $true
    $chaldea = Select-Executable $cli 'chaldea' 'bin' $false
    $exes = @($ptyExe, $startExe, $windowsExe, $chaldea)

    $problems = @()
    # The harness's own list must match the required names both ways.
    $listed = @(& $startExe --list | ForEach-Object { if ($_ -match '^(\S+): test$') { $Matches[1] } })
    $missing = @($StartTests | Where-Object { $_ -notin $listed })
    $unlisted = @($listed | Where-Object { $_ -notin $StartTests })
    if ($missing.Count -gt 0) { $problems += "daemon_start does not list: $($missing -join ', ')" }
    if ($unlisted.Count -gt 0) { $problems += "daemon_start lists cases this script does not require: $($unlisted -join ', ')" }

    $runId = [guid]::NewGuid().ToString('N')
    $base = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
    $run = Join-Path $base "chaldea-wmi-$runId"
    $tmp = Join-Path $run 'tmp'
    $runRoot = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) "chaldea-wmi-$runId"
    New-Item -ItemType Directory -Path $tmp | Out-Null
    New-Item -ItemType Directory -Path $runRoot | Out-Null
    $steps = @(
        @{ name = 'a-w1'; exe = $ptyExe; args = @($AW1, '--exact', '--nocapture'); required = @($AW1) },
        @{ name = 'daemon_start'; exe = $startExe; args = @(); required = $StartTests },
        @{ name = 'daemon_windows'; exe = $windowsExe; args = @('--exact') + $WmiWindowsTests; required = $WmiWindowsTests },
        # A daemon left running on purpose, for the cleanup to find after its starter exited.
        @{ name = 'cleanup-probe'; exe = $chaldea; args = @('--state-dir', (Join-Path $runRoot 'cleanup-probe\state'), 'daemon', 'start'); required = @() }
    )
    @{
        root = $Root
        run_root = $runRoot
        local_app_data = [Environment]::GetFolderPath('LocalApplicationData')
        steps = $steps
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $run 'plan.json') -Encoding utf8

    $vars = foreach ($n in $PassedVariables) {
        $v = [Environment]::GetEnvironmentVariable($n)
        if ($v) { "$n=$v" }
    }
    $vars = @($vars) + @("TEMP=$tmp", "TMP=$tmp", "$TestRootVariable=$runRoot")
    $startup = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{
        # CREATE_BREAKAWAY_FROM_JOB | CREATE_UNICODE_ENVIRONMENT
        CreateFlags = [uint32](0x01000000 -bor 0x400)
        EnvironmentVariables = [string[]]$vars
        ShowWindow = [uint16]0
    }
    $pwsh = (Get-Process -Id $PID).Path
    $script = Join-Path $PSScriptRoot 'windows-tests.ps1'
    $command = "`"$pwsh`" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$script`" -Lane Wrapper -RunDir `"$run`""

    $wrapper = $null
    $timedOut = $false
    $cleanup = $null
    try {
        # Submission, bounded by the CIM operation timeout.
        $created = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -OperationTimeoutSec 60 -Arguments @{
            CommandLine = $command
            CurrentDirectory = $Root
            ProcessStartupInformation = $startup
        }
        if ($created.ReturnValue -ne 0) { throw "Win32_Process.Create returned $($created.ReturnValue)" }
        $wrapperId = [uint32]$created.ProcessId
        $cim = Get-CimInstance Win32_Process -Filter "ProcessId = $wrapperId" -OperationTimeoutSec 30
        $candidate = Get-Process -Id $wrapperId -ErrorAction SilentlyContinue
        if ($candidate -and $cim -and (Test-SameStart $candidate.StartTime $cim.CreationDate)) {
            $null = $candidate.Handle
            $wrapper = $candidate
        }
        Write-Host "wrapper: pid $wrapperId, run directory $run, run root $runRoot"

        # Execution, classified against one monotonic cutoff: a wrapper that finishes at or
        # after it is a timeout.
        if ($wrapper) {
            $left = $cutoff - $clock.Elapsed
            $exited = $left -gt [TimeSpan]::Zero -and $wrapper.WaitForExit([int][Math]::Min([int]::MaxValue, $left.TotalMilliseconds))
            $timedOut = -not $exited -or $clock.Elapsed -ge $cutoff
        }
        elseif (-not (Test-Path -LiteralPath (Join-Path $run 'done'))) {
            $problems += "the wrapper (pid $wrapperId) could not be opened and left no final status"
        }
        if ($timedOut) { $problems += "the wrapper did not finish within $TimeoutMinutes min" }
    }
    catch { $problems += "the lane failed: $_" }
    finally {
        $cleanup = Invoke-Cleanup $wrapper $run $runRoot $chaldea $exes
        Write-Host "== cleanup ==`n$($cleanup | ConvertTo-Json -Depth 4)"
    }

    $identityFile = Join-Path $run 'identity.json'
    if (Test-Path -LiteralPath $identityFile) {
        $identityText = Get-Content -LiteralPath $identityFile -Raw
        Write-Host "== identity and job state ==`n$identityText"
        $identity = $identityText | ConvertFrom-Json
        if ($identity.in_job -ne $false) { $problems += "the wrapper reports in_job: $($identity.in_job)" }
        if (-not $identity.profile_ok) { $problems += "FOLDERID_LocalAppData is $($identity.local_app_data), not $($identity.expected_local_app_data)" }
        if (-not $identity.checkout_read) { $problems += 'the wrapper could not read the checkout' }
        if (-not $identity.temp_write) { $problems += 'the wrapper could not write its temp directory' }
    }
    else { $problems += 'identity.json is missing' }
    $errorFile = Join-Path $run 'error.txt'
    if (Test-Path -LiteralPath $errorFile) { $problems += "the wrapper failed: $(Get-Content -LiteralPath $errorFile -Raw)" }

    foreach ($s in $steps) {
        $files = @('out', 'err', 'exit') | ForEach-Object { Join-Path $run "$($s.name).$_" }
        $missingFiles = @($files | Where-Object { -not (Test-Path -LiteralPath $_) })
        if ($missingFiles.Count -gt 0) {
            $problems += "$($s.name): missing $($missingFiles -join ', ')"
            continue
        }
        $out = Get-Content -LiteralPath $files[0] -Raw
        $err = Get-Content -LiteralPath $files[1] -Raw
        $exit = (Get-Content -LiteralPath $files[2] -Raw).Trim()
        Write-Host "== $($s.name): exit $exit ==`n-- stdout --`n$out`n-- stderr --`n$err"
        if ($exit -ne '0') { $problems += "$($s.name) exited $exit" }
        foreach ($t in $s.required) {
            if ($out -notmatch ('(?m)^test ' + [regex]::Escape($t) + ' \.\.\. ok\b')) {
                $problems += "$($s.name): $t did not run and pass"
            }
        }
    }

    # The cleanup check: the probe daemon, whose starter exited, was found and is gone.
    $probeFile = Join-Path $run 'cleanup-probe.pid'
    if (Test-Path -LiteralPath $probeFile) {
        $probePid = (Get-Content -LiteralPath $probeFile -Raw).Trim()
        $found = @($cleanup.found | Where-Object { "$($_.pid)" -eq $probePid })
        Write-Host "== cleanup check: probe daemon pid $probePid, found: $($found.Count -gt 0) =="
        if ($found.Count -eq 0) { $problems += "the cleanup did not find the probe daemon (pid $probePid)" }
        if (Get-Process -Id ([int]$probePid) -ErrorAction SilentlyContinue | Where-Object { $_.Path -in $exes }) {
            $problems += "the probe daemon (pid $probePid) is still running after the cleanup"
        }
    }
    else { $problems += 'cleanup-probe.pid is missing' }
    if ($cleanup.alive_after.Count -gt 0) { $problems += "still running after the cleanup: $($cleanup.alive_after -join ', ')" }
    if ($cleanup.errors.Count -gt 0) { $problems += "cleanup errors: $($cleanup.errors -join '; ')" }

    $done = Join-Path $run 'done'
    if (Test-Path -LiteralPath $done) {
        $status = Get-Content -LiteralPath $done -Raw
        Write-Host "== done: $status"
        if ($status -cne 'finished') { $problems += "the final status is '$status', not 'finished'" }
    }
    else { $problems += 'the final status file is missing' }

    if ($problems.Count -gt 0) { throw "the WMI lane failed:`n$($problems -join "`n")" }
}

# Runs one step's executable with its arguments (no shell in between, so paths stay literal),
# its stdout, stderr, and exit status into files; records its id and start for the cleanup.
function Invoke-Step($Step, [string] $Cwd) {
    $psi = [Diagnostics.ProcessStartInfo]::new($Step.exe)
    foreach ($a in @($Step.args)) { $psi.ArgumentList.Add($a) }
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.WorkingDirectory = $Cwd
    $p = [Diagnostics.Process]::Start($psi)
    Add-Content -LiteralPath (Join-Path $RunDir 'procs.txt') -Value "$($p.Id) $($p.StartTime.ToUniversalTime().Ticks)"
    $out = $p.StandardOutput.ReadToEndAsync()
    $err = $p.StandardError.ReadToEndAsync()
    $p.WaitForExit()
    $code = $p.ExitCode
    # The output ends once every holder of the pipes closed them: a minute after the exit at most.
    if (-not $out.Wait(60000) -or -not $err.Wait(60000)) {
        $code = "$code (its output was still open 60 s after it exited)"
    }
    $o = if ($out.IsCompleted) { $out.Result } else { '' }
    $e = if ($err.IsCompleted) { $err.Result } else { '' }
    [IO.File]::WriteAllText((Join-Path $RunDir "$($Step.name).out"), $o)
    [IO.File]::WriteAllText((Join-Path $RunDir "$($Step.name).err"), $e)
    [IO.File]::WriteAllText((Join-Path $RunDir "$($Step.name).exit"), "$code")
    $code
}

# The wrapper: records its identity and job state, then runs the plan's steps; `done` last.
function Invoke-Wrapper {
    $plan = Get-Content -LiteralPath (Join-Path $RunDir 'plan.json') -Raw | ConvertFrom-Json
    $finish = { param($s) [IO.File]::WriteAllText((Join-Path $RunDir 'done'), $s) }
    try {
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public static class ChaldeaJob {
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool IsProcessInJob(IntPtr process, IntPtr job, out bool result);
    [DllImport("kernel32.dll")]
    static extern IntPtr GetCurrentProcess();
    public static bool InAnyJob() {
        bool result;
        if (!IsProcessInJob(GetCurrentProcess(), IntPtr.Zero, out result)) throw new Win32Exception();
        return result;
    }
}
'@
        $me = [Security.Principal.WindowsIdentity]::GetCurrent()
        $label = whoami /groups /fo csv | ConvertFrom-Csv | Where-Object { $_.SID -like 'S-1-16-*' } | Select-Object -First 1
        $localAppData = [Environment]::GetFolderPath('LocalApplicationData')
        $checkoutRead = $false
        try { $checkoutRead = (Get-Content -LiteralPath (Join-Path $plan.root 'Cargo.toml') -TotalCount 1).Length -gt 0 } catch {}
        $tempWrite = $false
        try {
            $probe = Join-Path $env:TEMP ('probe-' + [guid]::NewGuid().ToString('N'))
            Set-Content -LiteralPath $probe -Value 'probe'
            $tempWrite = (Get-Content -LiteralPath $probe -Raw).Trim() -eq 'probe'
            Remove-Item -LiteralPath $probe
        } catch {}
        $inJob = [ChaldeaJob]::InAnyJob()
        $profileOk = ($localAppData -eq $plan.local_app_data) -and ($localAppData -notmatch 'systemprofile')
        [ordered]@{
            user = $me.Name
            sid = $me.User.Value
            default_owner = $me.Owner.Value
            integrity = $label.'Group Name'
            integrity_sid = $label.SID
            session = (Get-Process -Id $PID).SessionId
            pid = $PID
            in_job = $inJob
            local_app_data = $localAppData
            expected_local_app_data = $plan.local_app_data
            profile_ok = $profileOk
            checkout = $plan.root
            checkout_read = $checkoutRead
            temp = $env:TEMP
            temp_write = $tempWrite
            test_root = [Environment]::GetEnvironmentVariable($TestRootVariable)
        } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $RunDir 'identity.json') -Encoding utf8
        if ($inJob -or -not $profileOk -or -not $checkoutRead -or -not $tempWrite) {
            & $finish 'stopped: the identity checks failed'
            return
        }

        foreach ($s in $plan.steps) {
            if ($s.name -eq 'cleanup-probe') {
                New-Item -ItemType Directory -Path (Join-Path $plan.run_root 'cleanup-probe') | Out-Null
            }
            $code = Invoke-Step $s $plan.root
            if ($s.name -eq 'a-w1' -and "$code" -ne '0') {
                & $finish 'stopped: A-W1 failed'
                return
            }
            if ($s.name -eq 'cleanup-probe') {
                $record = Join-Path $plan.run_root 'cleanup-probe\state\daemon.json'
                if (Test-Path -LiteralPath $record) {
                    $probePid = (Get-Content -LiteralPath $record -Raw | ConvertFrom-Json).pid
                    [IO.File]::WriteAllText((Join-Path $RunDir 'cleanup-probe.pid'), "$probePid")
                }
            }
        }
        & $finish 'finished'
    }
    catch {
        Set-Content -LiteralPath (Join-Path $RunDir 'error.txt') -Value ($_ | Out-String)
        & $finish 'stopped: error'
        exit 1
    }
}

Set-Location -LiteralPath $Root
switch ($Lane) {
    'Cargo' { Invoke-CargoLane }
    'Wmi' { Invoke-WmiLane }
    'Wrapper' { Invoke-Wrapper }
}
