<#
.SYNOPSIS
  WinRestyle automated VM test harness: pull, build, unit-test, then run every
  process-level safety test that doesn't need a real logon.

.DESCRIPTION
  Automates docs/TESTING.md: T0 (registry round-trip), T1/T2 (shell crash /
  crash-loop), T5/T6/T7 (watchdog kill / convergence / runaway cap),
  T8/T9 (hung shell / hung watchdog via the ADR 0003 heartbeat),
  T10/T11 (config load + hot reload over IPC; wallpaper paint + repaint),
  T12 (logon autostart + config opt-out, ADR 0004), T13 (taskbar surface
  supervision: spawn/paint, relaunch, crash-loop give-up, config opt-out,
  ADR 0005), T14 (window buttons track opened/closed windows), T15
  (taskbar extras: pinned apps incl. a real click-to-launch, backdrop +
  date config, single-bar startup, tray host gated off while unswapped),
  T16 (Phase 3 installer trial-run primitive: wr-shell --selftest),
  T17 (Phase 4 start menu: Start-chip click opens it, Esc closes it), and
  T18 (live activate/deactivate, ADR 0008 - swaps THIS session's desktop
  and puts it back, no logon).

  NOT covered — still manual, once per release: T3 (real swap + logon + blank
  desktop + Win+Ctrl+F1), the logged-in halves of T4, the manager *window*
  itself (T16 visual: checklist, startup list, Restyle Now / Undo), and the
  start menu's look + keyboard filter + launching (T17 visual).

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
.PARAMETER Tests
  Run only the named tests, comma/space separated, wildcards allowed:
  -Tests 'T17'   -Tests 'T13,T17'   -Tests 'T1?'. Default: everything.
  Combined sections match either id (T5/T6 runs for 'T5' or 'T6'). For a
  quick iteration loop: -SkipPull -SkipBuild -SkipUnit -Tests T17.
#>
[CmdletBinding()]
param(
    [switch]$SkipPull,
    [switch]$SkipBuild,
    [switch]$SkipUnit,
    [string]$Tests = ''
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
$RunKey      = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$RunOnceKey  = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\RunOnce'

# T10 overwrites the user's real config.toml; these drive the byte-identical
# restore in the finally block even if the test dies halfway.
$script:ConfigTouched = $false
$script:HadConfig     = $false
# A fresh Win11 image may not have the HKCU RunOnce key at all (Windows
# creates it on demand); if T12 creates it, cleanup removes it again.
$script:CreatedRunOnceKey = $false

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

# Killing one of the family makes a survivor resurrect it (that's the
# feature), so sweep all of them repeatedly until none remain.
function Stop-WrProcesses {
    foreach ($attempt in 1..8) {
        $procs = Get-Process -Name 'wr-shell', 'wr-watchdog', 'wr-taskbar' -ErrorAction SilentlyContinue
        if (-not $procs) { return }
        $procs | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 400
    }
    Write-Warning 'wr processes still alive after cleanup sweeps'
}

function Reset-TestEnv {
    Stop-WrProcesses
    Remove-Item Env:\WR_SHELL_TEST_ARGS -ErrorAction SilentlyContinue
    Remove-Item Env:\WR_TASKBAR_TEST_ARGS -ErrorAction SilentlyContinue
}

function Wait-Until([scriptblock]$Condition, [int]$TimeoutSec = 15) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        try { if (& $Condition) { return $true } } catch { }
        Start-Sleep -Milliseconds 300
    }
    return $false
}

# -Tests filter: empty runs everything; sections pass every id they cover.
$TestFilter = @($Tests -split '[,\s]+' | Where-Object { $_ })
function Test-Wanted([string[]]$Ids) {
    if ($TestFilter.Count -eq 0) { return $true }
    foreach ($id in $Ids) {
        foreach ($want in $TestFilter) { if ($id -like $want) { return $true } }
    }
    return $false
}

# Win32 helpers for tests that post real messages at our windows (T15, T17).
# Defined unconditionally so any -Tests subset can use them.
Add-Type -Namespace WRTest -Name U32 -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode)]
public static extern IntPtr FindWindowW(string lpClassName, string lpWindowName);
[DllImport("user32.dll")]
public static extern bool PostMessageW(IntPtr hWnd, uint msg, IntPtr wParam, IntPtr lParam);
'@

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
            if ($Tests)     { $forward += @('-Tests', $Tests) }
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

    # Any section may rewrite config.toml; snapshot it once, unconditionally,
    # so the finally block restores it byte-for-byte no matter which -Tests
    # subset ran (T10 used to own this, but T10 can be filtered out now).
    $script:HadConfig = Test-Path $ConfigFile
    if ($script:HadConfig) { Copy-Item $ConfigFile $ConfigBak -Force }
    $script:ConfigTouched = $true

    # ---- T0: registry backup/restore round-trip --------------------------
    if (Test-Wanted 'T0') {
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
    }

    # ---- T1: crashed shell is relaunched ----------------------------------
    if (Test-Wanted 'T1') {
    Write-Section 'T1: watchdog relaunches a crashed shell'
    $env:WR_SHELL_TEST_ARGS = '--crash-after=2'
    $wd = Start-Watchdog -LogName 't1'
    $ok = Wait-Until { (Get-Log $wd.Log) -match 'relaunching shell' } 25
    Record 'T1 crashed shell is relaunched' $ok -LogFile $wd.Log
    # T7/T9 assert on wr-shell (grandchild) log lines; prove they reach the
    # capture file at all, so a plumbing failure there isn't misread as a bug.
    $shellVisible = (Get-Log $wd.Log) -match 'wr-shell \(Phase 1 minimal\) starting'
    Record 'T1b shell (grandchild) logs reach the capture file' $shellVisible -LogFile $wd.Log
    Reset-TestEnv
    }

    # ---- T2: crash-loop falls back to explorer ----------------------------
    if (Test-Wanted 'T2') {
    Write-Section 'T2: shell crash-loop falls back to explorer'
    $env:WR_SHELL_TEST_ARGS = '--crash-after=1'
    $wd = Start-Watchdog -LogName 't2'
    $looped = Wait-Until { (Get-Log $wd.Log) -match 'shell crash-loop' } 45
    $exited = Wait-Until { $wd.Proc.HasExited } 10
    Record 'T2 crash-loop detected and watchdog exits' ($looped -and $exited) -LogFile $wd.Log
    Reset-TestEnv
    }

    # ---- T5/T6: killed watchdog is relaunched; pair converges -------------
    if (Test-Wanted 'T5', 'T6') {
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
    }

    # ---- T7: watchdog runaway cap ------------------------------------------
    if (Test-Wanted 'T7') {
    Write-Section 'T7: repeated watchdog kills trip the runaway cap'
    $wd = Start-Watchdog -LogName 't7'
    Wait-Until { (Get-Pids 'wr-shell').Count -eq 1 } 10 | Out-Null
    $kills = 0
    foreach ($i in 1..4) {
        $pids = Get-Pids 'wr-watchdog'
        if ($pids.Count -eq 0) { break }
        $old = $pids[0]
        $oldShell = @(Get-Pids 'wr-shell')[0]
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
            # Also wait for the pair to converge (stray shell swept, FRESH
            # shell spawned) before the next kill. A kill landing between
            # sweep and spawn hits the single-process window (ADR 0002
            # amendment) - a different failure mode than the runaway cap this
            # test validates, and one no human can reproduce.
            $converged = Wait-Until {
                $s = Get-Pids 'wr-shell'
                ($s.Count -eq 1) -and ($s[0] -ne $oldShell)
            } 10
            if (-not $converged) { break }
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
    }

    # ---- T8: hung shell (heartbeat) ----------------------------------------
    if (Test-Wanted 'T8') {
    Write-Section 'T8: hung shell is killed and relaunched (ADR 0003)'
    $env:WR_SHELL_TEST_ARGS = '--hang-heartbeat-after=3'
    $wd = Start-Watchdog -LogName 't8'
    $killed = Wait-Until { (Get-Log $wd.Log) -match 'killing hung shell' } 30
    $relaunched = Wait-Until { (Get-Log $wd.Log) -match 'relaunching shell' } 10
    Record 'T8 hung shell is killed and relaunched' ($killed -and $relaunched) -LogFile $wd.Log
    Reset-TestEnv
    }

    # ---- T9: hung watchdog (heartbeat) --------------------------------------
    if (Test-Wanted 'T9') {
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
    }

    # ---- T10/T11: config load + hot reload + wallpaper ---------------------
    if (Test-Wanted 'T10', 'T11') {
    Write-Section 'T10/T11: config load + hot reload; wallpaper paints and repaints'
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $ConfigFile) | Out-Null
    Set-Content $ConfigFile "[wallpaper]`ncolor = `"#112233`""
    # The flag makes the watchdog send ReloadConfig every 3s (nothing sends it
    # for real until the Phase 3 installer), so the rewrite below is picked up
    # no matter when it lands relative to the message.
    $wd = Start-Watchdog -Arguments @('--send-reload-every=3') -LogName 't10'
    $loaded = Wait-Until { (Get-Log $wd.Log) -match 'config: wallpaper color #112233' } 25
    $painted = Wait-Until { (Get-Log $wd.Log) -match 'wallpaper painted: color #112233' } 15
    Set-Content $ConfigFile "[wallpaper]`ncolor = `"#445566`""
    $reloaded = Wait-Until { (Get-Log $wd.Log) -match 'config now: wallpaper color #445566' } 20
    $repainted = Wait-Until { (Get-Log $wd.Log) -match 'wallpaper painted: color #445566' } 15
    Record 'T10 config.toml loaded at shell startup' $loaded -LogFile $wd.Log
    Record 'T10 ReloadConfig hot-swaps the config over the pipe' $reloaded -LogFile $wd.Log
    Record 'T11 wallpaper paints the configured color at startup' $painted -LogFile $wd.Log
    Record 'T11 wallpaper repaints after a hot reload' $repainted -LogFile $wd.Log
    Reset-TestEnv
    }

    # ---- T12: logon autostart (ADR 0004) -----------------------------------
    if (Test-Wanted 'T12') {
    # The shell only sees test entries: --autostart-test-filter bypasses the
    # "another desktop shell is on screen" guard but restricts launching to
    # ids containing the marker string, so the VM session's real startup apps
    # are never touched.
    Write-Section 'T12: autostart runs Run/RunOnce entries; config opt-out works'
    $runMarker  = Join-Path $LogDir 't12-run-ran.txt'
    $onceMarker = Join-Path $LogDir 't12-runonce-ran.txt'
    Remove-Item $runMarker, $onceMarker -ErrorAction SilentlyContinue
    New-ItemProperty -Path $RunKey -Name 'WinRestyleT12' `
        -Value "cmd /c echo ran> `"$runMarker`"" -Force | Out-Null
    if (-not (Test-Path $RunOnceKey)) {
        New-Item -Path $RunOnceKey -Force | Out-Null
        $script:CreatedRunOnceKey = $true
    }
    New-ItemProperty -Path $RunOnceKey -Name 'WinRestyleT12Once' `
        -Value "cmd /c echo ran> `"$onceMarker`"" -Force | Out-Null
    $env:WR_SHELL_TEST_ARGS = '--autostart-test-filter=WinRestyleT12'
    $wd = Start-Watchdog -LogName 't12'
    $ranRun  = Wait-Until { Test-Path $runMarker } 25
    $ranOnce = Wait-Until { Test-Path $onceMarker } 10
    $onceDeleted = Wait-Until {
        -not (Get-ItemProperty $RunOnceKey -Name 'WinRestyleT12Once' -ErrorAction SilentlyContinue)
    } 10
    Record 'T12 Run entry launched at shell start' $ranRun -LogFile $wd.Log
    Record 'T12 RunOnce entry launched and its value deleted' ($ranOnce -and $onceDeleted) `
        "ran=$ranOnce deleted=$onceDeleted" -LogFile $wd.Log
    Stop-WrProcesses

    # Opt-out: same entry, disabled via config, fresh shell.
    Remove-Item $runMarker -ErrorAction SilentlyContinue
    Set-Content $ConfigFile "[autostart]`ndisabled = [`"hkcu-run:WinRestyleT12`"]"
    $wd = Start-Watchdog -LogName 't12b'
    $skipLogged = Wait-Until {
        (Get-Log $wd.Log) -match 'autostart: skipped hkcu-run:WinRestyleT12 \(disabled in config\)'
    } 25
    Start-Sleep -Seconds 2   # would-be launch window
    $notRun = -not (Test-Path $runMarker)
    Record 'T12 config opt-out skips the entry' ($skipLogged -and $notRun) `
        "skipLogged=$skipLogged notRun=$notRun" -LogFile $wd.Log
    Remove-ItemProperty -Path $RunKey -Name 'WinRestyleT12' -ErrorAction SilentlyContinue
    Reset-TestEnv
    }

    # ---- T13: taskbar surface supervision (ADR 0005) -----------------------
    if (Test-Wanted 'T13') {
    # Unswapped, the taskbar detects explorer's live desktop and stays
    # non-topmost, so this never covers the VM's real taskbar. Rendering is
    # asserted via logs (like T11); visuals are eyeballed at the manual T3.
    Write-Section 'T13: taskbar spawns/paints, relaunches, crash-loop gives up'
    Remove-Item $ConfigFile -ErrorAction SilentlyContinue   # defaults: taskbar enabled
    $wd = Start-Watchdog -LogName 't13'
    $up = Wait-Until {
        ((Get-Pids 'wr-taskbar').Count -eq 1) -and ((Get-Log $wd.Log) -match 'taskbar window up')
    } 25
    $painted = Wait-Until { (Get-Log $wd.Log) -match 'taskbar painted: color ' } 15
    Record 'T13 taskbar spawns and paints' ($up -and $painted) "up=$up painted=$painted" -LogFile $wd.Log

    $t1 = @(Get-Pids 'wr-taskbar')[0]
    if ($null -ne $t1) { Stop-Process -Id $t1 -Force -ErrorAction SilentlyContinue }
    $relaunched = Wait-Until {
        $pids = Get-Pids 'wr-taskbar'
        ($pids.Count -eq 1) -and ($pids[0] -ne $t1)
    } 15
    $loggedRelaunch = (Get-Log $wd.Log) -match 'relaunching taskbar'
    Record 'T13 killed taskbar is relaunched by the shell' ($relaunched -and $loggedRelaunch) `
        "relaunched=$relaunched logged=$loggedRelaunch" -LogFile $wd.Log
    Reset-TestEnv

    # Crash-loop: the shell must give up on the taskbar and itself stay alive
    # (the taskbar is cosmetic - its failure never escalates to recovery).
    $env:WR_TASKBAR_TEST_ARGS = '--crash-after=1'
    $wd = Start-Watchdog -LogName 't13b'
    $gaveUp = Wait-Until { (Get-Log $wd.Log) -match 'taskbar crash-loop' } 45
    Start-Sleep -Seconds 2
    $pairAlive = ((Get-Pids 'wr-shell').Count -eq 1) -and ((Get-Pids 'wr-watchdog').Count -eq 1)
    $taskbarGone = (Get-Pids 'wr-taskbar').Count -eq 0
    Record 'T13 taskbar crash-loop gives up; shell+watchdog unaffected' `
        ($gaveUp -and $pairAlive -and $taskbarGone) `
        "gaveUp=$gaveUp pairAlive=$pairAlive taskbarGone=$taskbarGone" -LogFile $wd.Log
    Reset-TestEnv

    # Config opt-out: [taskbar] enabled = false means it is never spawned.
    $script:ConfigTouched = $true
    Set-Content $ConfigFile "[taskbar]`nenabled = false"
    $wd = Start-Watchdog -LogName 't13c'
    $skipped = Wait-Until { (Get-Log $wd.Log) -match 'taskbar disabled in config; not spawning it' } 25
    Start-Sleep -Seconds 2
    $noTaskbar = (Get-Pids 'wr-taskbar').Count -eq 0
    Record 'T13 config opt-out never spawns the taskbar' ($skipped -and $noTaskbar) `
        "skipped=$skipped noTaskbar=$noTaskbar" -LogFile $wd.Log
    Reset-TestEnv
    }

    # ---- T14: window buttons track open windows -----------------------------
    if (Test-Wanted 'T14') {
    # A WScript.Shell popup gives a top-level, unowned, titled dialog with a
    # title we control - deterministic and locale-independent (notepad's
    # title is localized; consoles may open in Windows Terminal).
    Write-Section 'T14: taskbar window buttons track opened/closed windows'
    Remove-Item $ConfigFile -ErrorAction SilentlyContinue   # defaults: taskbar enabled
    $wd = Start-Watchdog -LogName 't14'
    Wait-Until { (Get-Log $wd.Log) -match 'taskbar window up' } 25 | Out-Null
    $popup = Start-Process powershell -WindowStyle Hidden -PassThru -ArgumentList `
        '-NoProfile', '-Command',
        "(New-Object -ComObject WScript.Shell).Popup('WinRestyle T14', 90, 'WinRestyleT14') | Out-Null"
    $added = Wait-Until { (Get-Log $wd.Log) -match 'window added: .*WinRestyleT14' } 25
    Stop-Process -Id $popup.Id -Force -ErrorAction SilentlyContinue   # dialog dies with its process
    $removed = Wait-Until { (Get-Log $wd.Log) -match 'window removed: .*WinRestyleT14' } 15
    Record 'T14 new window becomes a taskbar button' $added -LogFile $wd.Log
    Record 'T14 closed window drops its button' $removed -LogFile $wd.Log
    Reset-TestEnv
    }

    # ---- T15: taskbar extras (pinned, backdrop, date, bars, tray gating) ----
    if (Test-Wanted 'T15') {
    # Unswapped smoke of the Phase 2 completion slices: the extras config
    # must start cleanly, pin an app (and launch it on a real click, posted
    # as WM_LBUTTONDOWN at the pinned square), apply/report the backdrop,
    # create exactly one bar on the single-monitor VM, and - the safety
    # assertion - NOT host a Shell_TrayWnd while explorer's desktop is live.
    Write-Section 'T15: taskbar extras (pinned apps, backdrop, tray gating)'
    $pinnedExe = @("$env:WINDIR\System32\notepad.exe", "$env:WINDIR\notepad.exe") |
        Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $pinnedExe) { $pinnedExe = "$env:WINDIR\System32\cmd.exe" }
    $pinnedName = [IO.Path]::GetFileNameWithoutExtension($pinnedExe)
    $script:ConfigTouched = $true
    # TOML single-quoted strings are literal (no escape processing): write
    # the path exactly as a user would, single backslashes.
    Set-Content $ConfigFile ("[taskbar]`nbackdrop = `"acrylic`"`nshow_date = true`n" +
        "pinned = ['" + $pinnedExe + "']")
    $preLaunch = @(Get-Process $pinnedName -ErrorAction SilentlyContinue).Id
    $wd = Start-Watchdog -LogName 't15'
    $up = Wait-Until { (Get-Log $wd.Log) -match 'taskbar painted: color ' } 25
    $oneBar   = (Get-Log $wd.Log) -match 'taskbar up: 1 bar\(s\)'
    $trayOff  = (Get-Log $wd.Log) -match 'tray host off'
    $pinnedOk = (Get-Log $wd.Log) -match 'pinned apps: 1'
    # The backdrop path must run and settle either way (applied, or cleanly
    # unavailable on this build) - never crash the bar.
    $backdrop = (Get-Log $wd.Log) -match 'backdrop applied: Acrylic|backdrop: system backdrop unavailable'
    Record 'T15 extras config paints with one bar' ($up -and $oneBar) `
        "painted=$up oneBar=$oneBar" -LogFile $wd.Log
    Record 'T15 pinned app loaded' $pinnedOk -LogFile $wd.Log
    Record 'T15 backdrop applied or cleanly unavailable' $backdrop -LogFile $wd.Log
    Record 'T15 tray host stays off while unswapped' $trayOff -LogFile $wd.Log

    # Click the pinned square. The bar logs its geometry at startup
    # ('pinned[0] chip at x,y WxH (bar-local)') precisely so this test never
    # re-derives layout constants or DPI math.
    # PowerShell marshals $null as String.Empty for string parameters, which
    # would make FindWindowW match on an empty *title* too; [NullString]
    # passes a real NULL so only the class is matched.
    $barWnd = [WRTest.U32]::FindWindowW('WinRestyleTaskbar', [NullString]::Value)
    $clicked = $false
    $geom = [regex]::Match((Get-Log $wd.Log), 'pinned\[0\] chip at (\d+),(\d+) (\d+)x(\d+)')
    if ($barWnd -ne [IntPtr]::Zero -and $geom.Success) {
        $cx = [int]$geom.Groups[1].Value + [int]([int]$geom.Groups[3].Value / 2)
        $cy = [int]$geom.Groups[2].Value + [int]([int]$geom.Groups[4].Value / 2)
        $lparam = [IntPtr](($cy -shl 16) -bor $cx)
        [WRTest.U32]::PostMessageW($barWnd, 0x0201, [IntPtr]::Zero, $lparam) | Out-Null
        $clicked = Wait-Until { (Get-Log $wd.Log) -match 'pinned launch: ' } 15
    }
    Record 'T15 clicking the pinned square launches the app' $clicked `
        "barWnd=$barWnd geom=$($geom.Success)" -LogFile $wd.Log
    # Reap whatever the click started so it never outlives the suite.
    Get-Process $pinnedName -ErrorAction SilentlyContinue |
        Where-Object { $preLaunch -notcontains $_.Id } |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Reset-TestEnv
    }

    # ---- T16: installer manager trial-run primitive (Phase 3) ---------------
    if (Test-Wanted 'T16') {
    # The manager *window* is manual (T3/T16 visual). Its safety-critical
    # pre-swap primitive is automatable: the `wr-shell --selftest` trial run the
    # installer performs before it ever touches the registry must load+validate
    # the config, log 'selftest ok', and exit 0 - without spawning any surface
    # or the safety harness. A non-zero exit is the installer's signal to abort
    # the swap.
    Write-Section 'T16: installer trial-run primitive (wr-shell --selftest)'
    $Shell = Join-Path $Bin 'wr-shell.exe'
    $selftestLog = Join-Path $LogDir 't16-selftest.log'
    $proc = Start-Process -FilePath $Shell -ArgumentList '--selftest' -NoNewWindow `
        -PassThru -Wait -RedirectStandardError $selftestLog
    $selftestOk = ($proc.ExitCode -eq 0) -and ((Get-Log $selftestLog) -match 'selftest ok')
    Record 'T16 shell --selftest validates config and exits 0' $selftestOk `
        "exit=$($proc.ExitCode)" -LogFile $selftestLog
    Reset-TestEnv
    }

    # ---- T17: start menu opens and closes (Phase 4, ADR 0007) ---------------
    if (Test-Wanted 'T17') {
    # The menu is a window inside wr-taskbar; clicking the Start chip opens it
    # in ALL sessions, so the unswapped suite can drive it. The bar logs the
    # Start chip's geometry at startup ('start chip at x,y WxH (bar-local)');
    # the menu logs its app count on open and 'start menu closed' on dismissal.
    # Esc is posted straight to the menu window, so the test doesn't depend on
    # the menu winning the foreground from a posted (not real) click.
    Write-Section 'T17: start menu (Start chip click opens, Esc closes)'
    Remove-Item $ConfigFile -ErrorAction SilentlyContinue   # defaults: taskbar enabled
    $wd = Start-Watchdog -LogName 't17'
    $up = Wait-Until { (Get-Log $wd.Log) -match 'taskbar painted: color ' } 25
    $barWnd = [WRTest.U32]::FindWindowW('WinRestyleTaskbar', [NullString]::Value)
    $opened = $false
    $geom = [regex]::Match((Get-Log $wd.Log), 'start chip at (\d+),(\d+) (\d+)x(\d+)')
    if ($barWnd -ne [IntPtr]::Zero -and $geom.Success) {
        $cx = [int]$geom.Groups[1].Value + [int]([int]$geom.Groups[3].Value / 2)
        $cy = [int]$geom.Groups[2].Value + [int]([int]$geom.Groups[4].Value / 2)
        $lparam = [IntPtr](($cy -shl 16) -bor $cx)
        [WRTest.U32]::PostMessageW($barWnd, 0x0201, [IntPtr]::Zero, $lparam) | Out-Null
        $opened = Wait-Until { (Get-Log $wd.Log) -match 'start menu opened: \d+ apps' } 15
    }
    Record 'T17 clicking the Start chip opens the menu' ($up -and $opened) `
        "painted=$up barWnd=$barWnd geom=$($geom.Success)" -LogFile $wd.Log
    $menuWnd = [WRTest.U32]::FindWindowW('WinRestyleStartMenu', [NullString]::Value)
    $closed = $false
    if ($menuWnd -ne [IntPtr]::Zero) {
        # WM_KEYDOWN, VK_ESCAPE.
        [WRTest.U32]::PostMessageW($menuWnd, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero) | Out-Null
        $closed = Wait-Until { (Get-Log $wd.Log) -match 'start menu closed' } 15
    }
    Record 'T17 Esc closes the menu' $closed "menuWnd=$menuWnd" -LogFile $wd.Log
    Reset-TestEnv
    }

    # ---- T18: live activate / deactivate (no logon; ADR 0008) ---------------
    # The one automated test that swaps THIS session's desktop for real:
    # `apply` + `activate` stop explorer and bring the wr family up swapped
    # (topmost, tray host on); `deactivate` restores the registry, sweeps the
    # family, and brings explorer back. Runs last so a failure can't disturb
    # other tests; the finally block relaunches explorer if we die midway.
    if (Test-Wanted 'T18') {
    Write-Section 'T18: live activate/deactivate (full session swap, no logon)'
    Remove-Item $ConfigFile -ErrorAction SilentlyContinue   # defaults
    $t18log = Join-Path $LogDir 't18-activate.log'
    & $Installer apply | Out-Null
    # The spawned watchdog (and its children) inherit stderr, so the whole
    # family's logs land in the activate log file.
    $proc = Start-Process -FilePath $Installer -ArgumentList 'activate' -NoNewWindow `
        -PassThru -Wait -RedirectStandardError $t18log
    $familyUp = Wait-Until {
        ((Get-Pids 'wr-watchdog').Count -eq 1) -and
        ((Get-Pids 'wr-shell').Count -eq 1) -and
        ((Get-Pids 'wr-taskbar').Count -eq 1)
    } 20
    # The swapped-mode signature: topmost bar + tray host on (never true in
    # the unswapped runs above).
    $swapped = Wait-Until { (Get-Log $t18log) -match 'taskbar up: .*topmost, tray host active' } 20
    $explorerGone = $null -eq (Get-Process explorer -ErrorAction SilentlyContinue)
    Record 'T18 activate swaps the live session (explorer out, wr desktop up)' `
        (($proc.ExitCode -eq 0) -and $familyUp -and $swapped -and $explorerGone) `
        "exit=$($proc.ExitCode) family=$familyUp swapped=$swapped explorerGone=$explorerGone" `
        -LogFile $t18log

    $t18dlog = Join-Path $LogDir 't18-deactivate.log'
    $proc = Start-Process -FilePath $Installer -ArgumentList 'deactivate' -NoNewWindow `
        -PassThru -Wait -RedirectStandardError $t18dlog
    $explorerBack = Wait-Until { $null -ne (Get-Process explorer -ErrorAction SilentlyContinue) } 25
    $wrGone = Wait-Until {
        ((Get-Pids 'wr-watchdog').Count -eq 0) -and
        ((Get-Pids 'wr-shell').Count -eq 0) -and
        ((Get-Pids 'wr-taskbar').Count -eq 0)
    } 15
    $backupGone = -not (Test-Path $BackupKey)
    Record 'T18 deactivate restores explorer + registry, sweeps the family' `
        (($proc.ExitCode -eq 0) -and $explorerBack -and $wrGone -and $backupGone) `
        "exit=$($proc.ExitCode) explorerBack=$explorerBack wrGone=$wrGone backupGone=$backupGone" `
        -LogFile $t18dlog
    Reset-TestEnv
    }
}
finally {
    Reset-TestEnv
    # T12 leftovers must never survive into the user's real logon.
    Remove-ItemProperty -Path $RunKey -Name 'WinRestyleT12' -ErrorAction SilentlyContinue
    Remove-ItemProperty -Path $RunOnceKey -Name 'WinRestyleT12Once' -ErrorAction SilentlyContinue
    if ($script:CreatedRunOnceKey -and (Test-Path $RunOnceKey)) {
        $key = Get-Item $RunOnceKey -ErrorAction SilentlyContinue
        if ($key -and $key.ValueCount -eq 0 -and $key.SubKeyCount -eq 0) {
            Remove-Item $RunOnceKey -ErrorAction SilentlyContinue
        }
    }
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
    # T18 stops explorer; if the run died between activate and deactivate,
    # put the desktop back (the wr processes were already swept above).
    if (-not (Get-Process explorer -ErrorAction SilentlyContinue)) {
        Write-Warning 'explorer not running - relaunching it'
        Start-Process explorer.exe
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
