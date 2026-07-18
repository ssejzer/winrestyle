<#
.SYNOPSIS
  WinRestyle automated VM test harness: pull, build, unit-test, then run every
  process-level safety test that doesn't need a real logon.

.DESCRIPTION
  Automates docs/TESTING.md: T0 (registry round-trip), T1/T2 (shell crash /
  crash-loop), T5/T6/T7 (watchdog kill / convergence / runaway cap),
  T8/T9 (hung shell / hung watchdog via the ADR 0003 heartbeat), and
  T10 (config load + hot reload over IPC).

  NOT covered — still manual, once per release: T3 (real swap + logon + blank
  desktop + Win+Ctrl+F1) and the logged-in halves of T4.

  Run inside the disposable Windows 11 VM, from anywhere:

    powershell -ExecutionPolicy Bypass -File scripts\vm-test.ps1

  Fully hands-off: no logon, no hotkey, no interaction. Per-test logs land in
  target\vm-test-logs\ for debugging failures.

.PARAMETER SkipPull
  Don't run `git pull` first (e.g. testing local uncommitted work).
.PARAMETER SkipBuild
  Don't rebuild (reuse the existing release binaries).
.PARAMETER SkipUnit
  Don't run `cargo test`.
#>
[CmdletBinding()]
param(
    [switch]$SkipPull,
    [switch]$SkipBuild,
    [switch]$SkipUnit
)

$ErrorActionPreference = 'Stop'

$RepoRoot    = Split-Path -Parent $PSScriptRoot
$Bin         = Join-Path $RepoRoot 'target\release'
$Watchdog    = Join-Path $Bin 'wr-watchdog.exe'
$Installer   = Join-Path $Bin 'wr-installer.exe'
$LogDir      = Join-Path $RepoRoot 'target\vm-test-logs'
$WinlogonKey = 'HKCU:\Software\Microsoft\Windows NT\CurrentVersion\Winlogon'
$BackupKey   = 'HKCU:\Software\WinRestyle'
$ConfigFile  = Join-Path $env:APPDATA 'WinRestyle\config.toml'
$ConfigBak   = Join-Path $LogDir 'config.toml.bak'

# T10 overwrites the user's real config.toml; these drive the byte-identical
# restore in the finally block even if the test dies halfway.
$script:ConfigTouched = $false
$script:HadConfig     = $false

$script:Results = @()

function Write-Section([string]$Msg) { Write-Host "`n== $Msg ==" -ForegroundColor Cyan }

function Record([string]$Name, [bool]$Pass, [string]$Detail = '', [string]$LogFile = '') {
    $script:Results += [pscustomobject]@{ Test = $Name; Pass = $Pass; Detail = $Detail }
    if ($Pass) { Write-Host "  PASS  $Name" -ForegroundColor Green }
    else {
        Write-Host "  FAIL  $Name  $Detail" -ForegroundColor Red
        if ($LogFile -and (Test-Path $LogFile)) {
            Write-Host "  ----- tail of $(Split-Path -Leaf $LogFile) -----" -ForegroundColor DarkGray
            Get-Content $LogFile -Tail 40 -ErrorAction SilentlyContinue |
                ForEach-Object { Write-Host "  | $_" -ForegroundColor DarkGray }
        }
    }
}

function Get-Pids([string]$Name) {
    @(Get-Process -Name $Name -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id)
}

# Killing one of the pair makes the other resurrect it (that's the feature),
# so sweep both repeatedly until neither remains.
function Stop-WrProcesses {
    foreach ($attempt in 1..8) {
        $procs = Get-Process -Name 'wr-shell', 'wr-watchdog' -ErrorAction SilentlyContinue
        if (-not $procs) { return }
        $procs | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 400
    }
    Write-Warning 'wr processes still alive after cleanup sweeps'
}

function Reset-TestEnv {
    Stop-WrProcesses
    Remove-Item Env:\WR_SHELL_TEST_ARGS -ErrorAction SilentlyContinue
}

function Wait-Until([scriptblock]$Condition, [int]$TimeoutSec = 15) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        try { if (& $Condition) { return $true } } catch { }
        Start-Sleep -Milliseconds 300
    }
    return $false
}

# env_logger writes to stderr; children (wr-shell, and any watchdog *it*
# relaunches) inherit the handle, so one file collects the whole family's logs.
function Start-Watchdog([string[]]$Arguments = @(), [string]$LogName = 'watchdog') {
    $log = Join-Path $LogDir "$LogName.stderr.log"
    $out = Join-Path $LogDir "$LogName.stdout.log"
    Remove-Item $log, $out -ErrorAction SilentlyContinue
    $params = @{
        FilePath               = $Watchdog
        RedirectStandardError  = $log
        RedirectStandardOutput = $out
        WindowStyle            = 'Hidden'
        PassThru               = $true
    }
    if ($Arguments.Count -gt 0) { $params.ArgumentList = $Arguments }
    $proc = Start-Process @params
    [pscustomobject]@{ Proc = $proc; Log = $log }
}

function Get-Log([string]$Path) {
    if (Test-Path $Path) { Get-Content $Path -Raw -ErrorAction SilentlyContinue } else { '' }
}

Push-Location $RepoRoot
try {
    New-Item -ItemType Directory -Force -Path $LogDir | Out-Null

    if (-not $SkipPull) {
        Write-Section 'git pull'
        # PowerShell parses the whole script before running it, so a pull that
        # updates this file does not affect the *current* execution - assertions
        # would lag one commit behind the code. Detect that and re-exec.
        $selfBefore = (Get-FileHash $PSCommandPath).Hash
        git pull --ff-only
        if ($LASTEXITCODE -ne 0) { throw 'git pull failed' }
        if ((Get-FileHash $PSCommandPath).Hash -ne $selfBefore) {
            Write-Host 'Test script was updated by the pull; re-running the new version...' -ForegroundColor Yellow
            $forward = @('-SkipPull')
            if ($SkipBuild) { $forward += '-SkipBuild' }
            if ($SkipUnit)  { $forward += '-SkipUnit' }
            & powershell -ExecutionPolicy Bypass -File $PSCommandPath @forward
            exit $LASTEXITCODE
        }
    }
    if (-not $SkipBuild) {
        Write-Section 'cargo build --release'
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw 'build failed' }
    }
    if (-not (Test-Path $Watchdog)) { throw "missing $Watchdog - build first" }

    if (-not $SkipUnit) {
        Write-Section 'cargo test --workspace'
        cargo test --workspace
        Record 'unit tests' ($LASTEXITCODE -eq 0)
    }

    Reset-TestEnv

    # ---- T0: registry backup/restore round-trip --------------------------
    Write-Section 'T0: registry backup/restore round-trip'
    if (Test-Path $BackupKey) {
        Write-Warning 'stale WinRestyle backup found - restoring first'
        & $Installer restore | Out-Null
    }
    $before = (Get-ItemProperty $WinlogonKey -ErrorAction SilentlyContinue).Shell
    & $Installer apply | Out-Null
    $applied = (Get-ItemProperty $WinlogonKey -ErrorAction SilentlyContinue).Shell
    & $Installer restore | Out-Null
    $after = (Get-ItemProperty $WinlogonKey -ErrorAction SilentlyContinue).Shell
    $backupGone = -not (Test-Path $BackupKey)
    Record 'T0 apply points Shell at the watchdog' ($applied -like '*wr-watchdog*') "Shell=$applied"
    Record 'T0 restore returns the original value' (($after -eq $before) -and $backupGone) `
        "before=[$before] after=[$after] backupGone=$backupGone"

    # ---- T1: crashed shell is relaunched ----------------------------------
    Write-Section 'T1: watchdog relaunches a crashed shell'
    $env:WR_SHELL_TEST_ARGS = '--crash-after=2'
    $wd = Start-Watchdog -LogName 't1'
    $ok = Wait-Until { (Get-Log $wd.Log) -match 'relaunching shell' } 25
    Record 'T1 crashed shell is relaunched' $ok -LogFile $wd.Log
    # T7/T9 assert on wr-shell (grandchild) log lines; prove they reach the
    # capture file at all, so a plumbing failure there isn't misread as a bug.
    $shellVisible = (Get-Log $wd.Log) -match 'wr-shell \(Phase 0 dummy\) starting'
    Record 'T1b shell (grandchild) logs reach the capture file' $shellVisible -LogFile $wd.Log
    Reset-TestEnv

    # ---- T2: crash-loop falls back to explorer ----------------------------
    Write-Section 'T2: shell crash-loop falls back to explorer'
    $env:WR_SHELL_TEST_ARGS = '--crash-after=1'
    $wd = Start-Watchdog -LogName 't2'
    $looped = Wait-Until { (Get-Log $wd.Log) -match 'shell crash-loop' } 45
    $exited = Wait-Until { $wd.Proc.HasExited } 10
    Record 'T2 crash-loop detected and watchdog exits' ($looped -and $exited) -LogFile $wd.Log
    Reset-TestEnv

    # ---- T5/T6: killed watchdog is relaunched; pair converges -------------
    Write-Section 'T5/T6: killed watchdog is relaunched by the shell'
    $wd = Start-Watchdog -LogName 't5'
    $spawned = Wait-Until { (Get-Pids 'wr-shell').Count -eq 1 } 10
    $w1 = $wd.Proc.Id
    $s1 = @(Get-Pids 'wr-shell')[0]
    Stop-Process -Id $w1 -Force -ErrorAction SilentlyContinue
    $relaunched = Wait-Until {
        $pids = Get-Pids 'wr-watchdog'
        ($pids.Count -eq 1) -and ($pids[0] -ne $w1)
    } 15
    Start-Sleep -Seconds 2   # let the stray sweep + fresh spawn settle
    $wPids = Get-Pids 'wr-watchdog'; $sPids = Get-Pids 'wr-shell'
    $converged = ($wPids.Count -eq 1) -and ($sPids.Count -eq 1) -and ($sPids[0] -ne $s1)
    Record 'T5 shell relaunches a killed watchdog' ($spawned -and $relaunched) -LogFile $wd.Log
    Record 'T6 pair converges to one of each (fresh shell)' $converged `
        "watchdogs=[$($wPids -join ',')] shells=[$($sPids -join ',')]" -LogFile $wd.Log
    Reset-TestEnv

    # ---- T7: watchdog runaway cap ------------------------------------------
    Write-Section 'T7: repeated watchdog kills trip the runaway cap'
    $wd = Start-Watchdog -LogName 't7'
    Wait-Until { (Get-Pids 'wr-shell').Count -eq 1 } 10 | Out-Null
    $kills = 0
    foreach ($i in 1..4) {
        $pids = Get-Pids 'wr-watchdog'
        if ($pids.Count -eq 0) { break }
        $old = $pids[0]
        Stop-Process -Id $old -Force -ErrorAction SilentlyContinue
        $kills++
        if ($i -lt 4) {
            # Momentarily zero watchdogs is normal (old dead, replacement not
            # spawned yet) - wait for the actual replacement pid.
            $resurrected = Wait-Until {
                $now = Get-Pids 'wr-watchdog'
                ($now.Count -ge 1) -and ($now[0] -ne $old)
            } 10
            if (-not $resurrected) { break }
        }
    }
    # After the 4th kill the runaway cap must stop the cycle: no relaunch, and
    # the shell restores + exits. Process state is authoritative; the log line
    # (from the shell) is diagnostic.
    $allGone = Wait-Until {
        ((Get-Pids 'wr-watchdog').Count -eq 0) -and ((Get-Pids 'wr-shell').Count -eq 0)
    } 20
    $capLogged = (Get-Log $wd.Log) -match 'watchdog crash-loop'
    Record 'T7 runaway cap stops the relaunch cycle' ($kills -eq 4 -and $allGone) `
        "kills=$kills allGone=$allGone capLogged=$capLogged" -LogFile $wd.Log
    Reset-TestEnv

    # ---- T8: hung shell (heartbeat) ----------------------------------------
    Write-Section 'T8: hung shell is killed and relaunched (ADR 0003)'
    $env:WR_SHELL_TEST_ARGS = '--hang-heartbeat-after=3'
    $wd = Start-Watchdog -LogName 't8'
    $killed = Wait-Until { (Get-Log $wd.Log) -match 'killing hung shell' } 30
    $relaunched = Wait-Until { (Get-Log $wd.Log) -match 'relaunching shell' } 10
    Record 'T8 hung shell is killed and relaunched' ($killed -and $relaunched) -LogFile $wd.Log
    Reset-TestEnv

    # ---- T9: hung watchdog (heartbeat) --------------------------------------
    Write-Section 'T9: hung watchdog is killed and relaunched (ADR 0003)'
    $wd = Start-Watchdog -Arguments @('--ack-hang-after=6') -LogName 't9'
    $w1 = $wd.Proc.Id
    # Freeze at 6s + 5s heartbeat timeout => the shell should kill pid $w1 and
    # the monitor should relaunch a fresh watchdog. Process state (a new
    # watchdog pid) is authoritative; the log lines are diagnostic.
    $relaunched = Wait-Until {
        $pids = Get-Pids 'wr-watchdog'
        ($pids.Count -eq 1) -and ($pids[0] -ne $w1)
    } 40
    Start-Sleep -Seconds 2
    $converged = ((Get-Pids 'wr-watchdog').Count -eq 1) -and ((Get-Pids 'wr-shell').Count -eq 1)
    $log = Get-Log $wd.Log
    $froze  = $log -match 'SIMULATING WATCHDOG HANG'
    $killed = $log -match 'killing hung watchdog'
    Record 'T9 hung watchdog is killed and relaunched' ($relaunched -and $converged) `
        "relaunched=$relaunched converged=$converged froze=$froze killedLogged=$killed" -LogFile $wd.Log
    Reset-TestEnv

    # ---- T10: config load + hot reload over IPC ----------------------------
    Write-Section 'T10: config loads at startup and hot-reloads over IPC'
    $script:HadConfig = Test-Path $ConfigFile
    if ($script:HadConfig) { Copy-Item $ConfigFile $ConfigBak -Force }
    $script:ConfigTouched = $true
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $ConfigFile) | Out-Null
    Set-Content $ConfigFile "[wallpaper]`ncolor = `"#112233`""
    # The flag makes the watchdog send ReloadConfig every 3s (nothing sends it
    # for real until the Phase 3 installer), so the rewrite below is picked up
    # no matter when it lands relative to the message.
    $wd = Start-Watchdog -Arguments @('--send-reload-every=3') -LogName 't10'
    $loaded = Wait-Until { (Get-Log $wd.Log) -match 'config: wallpaper color #112233' } 25
    Set-Content $ConfigFile "[wallpaper]`ncolor = `"#445566`""
    $reloaded = Wait-Until { (Get-Log $wd.Log) -match 'config now: wallpaper color #445566' } 20
    Record 'T10 config.toml loaded at shell startup' $loaded -LogFile $wd.Log
    Record 'T10 ReloadConfig hot-swaps the config over the pipe' $reloaded -LogFile $wd.Log
    Reset-TestEnv
}
finally {
    Reset-TestEnv
    if ($script:ConfigTouched) {
        if ($script:HadConfig) { Copy-Item $ConfigBak $ConfigFile -Force }
        else {
            Remove-Item $ConfigFile -ErrorAction SilentlyContinue
            # Drop the WinRestyle dir too if T10 created it and nothing else
            # lives there.
            $cfgDir = Split-Path -Parent $ConfigFile
            if ((Test-Path $cfgDir) -and -not (Get-ChildItem $cfgDir -Force)) {
                Remove-Item $cfgDir -ErrorAction SilentlyContinue
            }
        }
    }
    if (Test-Path $BackupKey) {
        Write-Warning 'WinRestyle registry backup still present - restoring'
        & $Installer restore
    }
    Pop-Location
}

# ---- Summary ---------------------------------------------------------------
$passed = @($script:Results | Where-Object Pass).Count
$total  = $script:Results.Count
Write-Host ''
Write-Host "== Summary: $passed/$total passed ==" -ForegroundColor Cyan
$failed = @($script:Results | Where-Object { -not $_.Pass })
if ($failed) {
    $failed | ForEach-Object { Write-Host "  FAIL  $($_.Test)  $($_.Detail)" -ForegroundColor Red }
    Write-Host "Logs: $LogDir" -ForegroundColor Yellow
    exit 1
}
Write-Host 'All automated tests passed.' -ForegroundColor Green
Write-Host 'Still manual (once per release): T3 - real swap + logon + Win+Ctrl+F1.'
exit 0
