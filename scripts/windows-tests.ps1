<#
.SYNOPSIS
Runs the workspace tests on a Windows host (docs/design/daemon-lifecycle.md §7).

.DESCRIPTION
cargo puts the processes of `cargo test` in a kill-on-close job of its own that allows no
breakaway, so a `daemon start` under it is refused by design (§2.12 A-W1). The tests are
therefore split into two lanes, and both must pass:

  -Lane Cargo  every test except those that need a successful detached start, under cargo.
  -Lane Wmi    builds the test executables of those tests and runs them from a wrapper
               started through WMI `Win32_Process.Create` with `CREATE_BREAKAWAY_FROM_JOB`,
               outside any job. The wrapper first records its identity and job state; the lane
               fails if it is in a job, if a check of its profile or access fails, if a
               selected test did not run and pass by name, if an output file is missing, or on
               timeout.

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

function Invoke-CargoLane {
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

# The run's processes: the wrapper's live tree, and detached daemons whose command line names a
# state directory this run's tests created. Each is a (ProcessId, CreationDate) pair, so a
# reused id is never taken for one of them.
function Get-RunProcesses($Wrapper, [string[]] $Exes, [string[]] $StateDirs) {
    $all = Get-CimInstance Win32_Process -OperationTimeoutSec 30 |
        Where-Object { $_.CreationDate -ge $Wrapper.CreationDate }
    $tree = @{ [uint32]$Wrapper.ProcessId = $true }
    do {
        $grew = $false
        foreach ($p in $all) {
            if ($tree.ContainsKey([uint32]$p.ParentProcessId) -and -not $tree.ContainsKey([uint32]$p.ProcessId)) {
                $tree[[uint32]$p.ProcessId] = $true
                $grew = $true
            }
        }
    } while ($grew)
    $all | Where-Object {
        $line = [string]$_.CommandLine
        $tree.ContainsKey([uint32]$_.ProcessId) -or
            ($_.ExecutablePath -in $Exes -and $line.Contains('--detached-child') -and
                @($StateDirs | Where-Object { $line.Contains($_) }).Count -gt 0)
    }
}

# Stops what a wrapper that ran out of time left: `daemon stop` for the state directories the
# tests created, then the run's verified processes, the wrapper last.
function Stop-Run($Wrapper, [string] $Chaldea, [string[]] $Exes, [datetime] $Started) {
    $base = [Environment]::GetFolderPath('LocalApplicationData')
    $dirs = @(Get-ChildItem -LiteralPath $base -Directory -Force -Filter '.tmp*' -ErrorAction SilentlyContinue |
        Where-Object { $_.CreationTimeUtc -ge $Started -and -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) } |
        ForEach-Object { Join-Path $_.FullName 'state' } |
        Where-Object { Test-Path -LiteralPath $_ })
    foreach ($d in $dirs) {
        $psi = [Diagnostics.ProcessStartInfo]::new($Chaldea)
        foreach ($a in @('--state-dir', $d, 'daemon', 'stop')) { $psi.ArgumentList.Add($a) }
        $psi.UseShellExecute = $false
        $p = [Diagnostics.Process]::Start($psi)
        if (-not $p.WaitForExit(20000)) { Write-Host "daemon stop for $d did not finish within 20 s" }
        else { Write-Host "daemon stop for $d exited $($p.ExitCode)" }
    }
    $procs = @(Get-RunProcesses $Wrapper $Exes $dirs | Sort-Object { $_.ProcessId -eq $Wrapper.ProcessId })
    foreach ($p in $procs) {
        $now = Get-CimInstance Win32_Process -Filter "ProcessId = $($p.ProcessId)" -OperationTimeoutSec 30
        if ($now -and $now.CreationDate -eq $p.CreationDate) {
            Write-Host "terminating $($p.ProcessId) $($p.CommandLine)"
            Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
        }
    }
}

function Invoke-WmiLane {
    $pty = Get-Artifacts @('-p', 'chaldea-pty-runtime', '--lib')
    $cli = Get-Artifacts @('-p', 'chaldea-cli', '--test', 'daemon_start', '--test', 'daemon_windows')
    $ptyExe = Select-Executable $pty 'chaldea_pty_runtime' 'lib' $true
    $startExe = Select-Executable $cli 'daemon_start' 'test' $true
    $windowsExe = Select-Executable $cli 'daemon_windows' 'test' $true
    $chaldea = Select-Executable $cli 'chaldea' 'bin' $false

    # Every `daemon_start` case, by name (its harness exits 0 with zero selected).
    $startTests = @(& $startExe --list | ForEach-Object { if ($_ -match '^(\S+): test$') { $Matches[1] } })
    if ($LASTEXITCODE -ne 0 -or $startTests.Count -eq 0) { throw 'daemon_start listed no tests' }
    $steps = @(
        @{ name = 'a-w1'; exe = $ptyExe; args = @($AW1, '--exact', '--nocapture'); required = @($AW1) },
        @{ name = 'daemon_start'; exe = $startExe; args = @(); required = $startTests },
        @{ name = 'daemon_windows'; exe = $windowsExe; args = @('--exact') + $WmiWindowsTests; required = $WmiWindowsTests }
    )

    $base = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
    $run = Join-Path $base ('chaldea-wmi-' + [guid]::NewGuid().ToString('N'))
    $tmp = Join-Path $run 'tmp'
    New-Item -ItemType Directory -Path $tmp | Out-Null
    @{
        root = $Root
        local_app_data = [Environment]::GetFolderPath('LocalApplicationData')
        steps = $steps
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $run 'plan.json') -Encoding utf8

    $vars = foreach ($n in $PassedVariables) {
        $v = [Environment]::GetEnvironmentVariable($n)
        if ($v) { "$n=$v" }
    }
    $vars = @($vars) + @("TEMP=$tmp", "TMP=$tmp")
    $startup = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{
        # CREATE_BREAKAWAY_FROM_JOB | CREATE_UNICODE_ENVIRONMENT
        CreateFlags = [uint32](0x01000000 -bor 0x400)
        EnvironmentVariables = [string[]]$vars
        ShowWindow = [uint16]0
    }
    $pwsh = (Get-Process -Id $PID).Path
    $script = Join-Path $PSScriptRoot 'windows-tests.ps1'
    $command = "`"$pwsh`" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$script`" -Lane Wrapper -RunDir `"$run`""
    $started = [datetime]::UtcNow

    # Submission, bounded by the CIM operation timeout.
    $created = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -OperationTimeoutSec 60 -Arguments @{
        CommandLine = $command
        CurrentDirectory = $Root
        ProcessStartupInformation = $startup
    }
    if ($created.ReturnValue -ne 0) { throw "Win32_Process.Create returned $($created.ReturnValue)" }
    $wrapperId = [uint32]$created.ProcessId
    $wrapper = Get-CimInstance Win32_Process -Filter "ProcessId = $wrapperId" -OperationTimeoutSec 30
    Write-Host "wrapper: pid $wrapperId, run directory $run"

    $problems = @()
    $timedOut = $false
    try {
        # Execution, bounded by -TimeoutMinutes.
        $deadline = [datetime]::UtcNow.AddMinutes($TimeoutMinutes)
        $process = Get-Process -Id $wrapperId -ErrorAction SilentlyContinue
        # The two clocks differ in precision: the same process starts within a second of both.
        $same = $process -and $wrapper -and
            [Math]::Abs(($process.StartTime.ToUniversalTime() - $wrapper.CreationDate.ToUniversalTime()).TotalSeconds) -lt 1
        if ($same) {
            $left = [int][Math]::Max(0, ($deadline - [datetime]::UtcNow).TotalMilliseconds)
            $timedOut = -not $process.WaitForExit($left)
        }
        elseif ($process) {
            $problems += "process $wrapperId is not the wrapper"
        }
        if ($timedOut) { $problems += "the wrapper did not finish within $TimeoutMinutes min" }
    }
    finally {
        # Cleanup, only of what this run started.
        if ($timedOut -and $wrapper) {
            Stop-Run $wrapper $chaldea @($ptyExe, $startExe, $windowsExe, $chaldea) $started
        }
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
        $missing = @($files | Where-Object { -not (Test-Path -LiteralPath $_) })
        if ($missing.Count -gt 0) {
            $problems += "$($s.name): missing $($missing -join ', ')"
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
    $done = Join-Path $run 'done'
    if (Test-Path -LiteralPath $done) { Write-Host "== done: $(Get-Content -LiteralPath $done -Raw)" }
    else { $problems += 'the final status file is missing' }

    if ($problems.Count -gt 0) { throw "the WMI lane failed:`n$($problems -join "`n")" }
}

# The wrapper: records its identity and job state, then runs the plan's steps, each test
# executable's stdout, stderr, and exit status into files; `done` last.
function Invoke-Wrapper {
    $plan = Get-Content -LiteralPath (Join-Path $RunDir 'plan.json') -Raw | ConvertFrom-Json
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
        } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $RunDir 'identity.json') -Encoding utf8
        if ($inJob -or -not $profileOk -or -not $checkoutRead -or -not $tempWrite) {
            Set-Content -LiteralPath (Join-Path $RunDir 'done') -Value 'stopped: the identity checks failed'
            return
        }

        foreach ($s in $plan.steps) {
            $out = Join-Path $RunDir "$($s.name).out"
            $err = Join-Path $RunDir "$($s.name).err"
            # cmd opens the files and hands them to the test executable, so the wait ends at its
            # exit, not when every holder of a pipe has closed it.
            $psi = [Diagnostics.ProcessStartInfo]::new((Join-Path $env:SystemRoot 'System32\cmd.exe'))
            $psi.Arguments = "/d /s /c `"`"$($s.exe)`" $(@($s.args) -join ' ') > `"$out`" 2> `"$err`"`""
            $psi.UseShellExecute = $false
            $psi.WorkingDirectory = $plan.root
            $p = [Diagnostics.Process]::Start($psi)
            $p.WaitForExit()
            Set-Content -LiteralPath (Join-Path $RunDir "$($s.name).exit") -Value $p.ExitCode
            if ($s.name -eq 'a-w1' -and $p.ExitCode -ne 0) {
                Set-Content -LiteralPath (Join-Path $RunDir 'done') -Value 'stopped: A-W1 failed'
                return
            }
        }
        Set-Content -LiteralPath (Join-Path $RunDir 'done') -Value 'finished'
    }
    catch {
        Set-Content -LiteralPath (Join-Path $RunDir 'error.txt') -Value ($_ | Out-String)
        Set-Content -LiteralPath (Join-Path $RunDir 'done') -Value 'stopped: error'
        exit 1
    }
}

Set-Location -LiteralPath $Root
switch ($Lane) {
    'Cargo' { Invoke-CargoLane }
    'Wmi' { Invoke-WmiLane }
    'Wrapper' { Invoke-Wrapper }
}
