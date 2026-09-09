# safe-build.ps1 - 8GB memory machine build watchdog.
# Runs a build command at below-normal priority and kills the whole process
# tree if commit charge or free memory approaches the danger zone.
# Rationale: 2026-09-04 22:37 hard freeze (Kernel-Power 41) during an
# incremental release build - commit thrash left the system unresponsive.
# Better to abort the build and retry than to freeze the machine.

param(
    [Parameter(Mandatory = $true)][string]$Command,
    [string]$WorkDir = (Get-Location).Path,
    # Abort the build when this share of the commit limit is in use.
    [int]$MaxCommitPercent = 88,
    # Abort when physical free memory drops below this (MB).
    [int]$MinFreeMB = 500,
    [int]$PollSeconds = 12
)

$ErrorActionPreference = "Continue"

function Get-MemState {
    $os = Get-CimInstance Win32_OperatingSystem
    $perf = Get-CimInstance Win32_PerfFormattedData_PerfOS_Memory
    [pscustomobject]@{
        FreeMB        = [math]::Round($os.FreePhysicalMemory / 1KB)
        CommitPercent = [math]::Round($perf.PercentCommittedBytesInUse)
    }
}

# ---- preflight ----
$mem = Get-MemState
Write-Output ("[preflight] free={0}MB commit={1}%" -f $mem.FreeMB, $mem.CommitPercent)
if ($mem.FreeMB -lt ($MinFreeMB * 2) -or $mem.CommitPercent -gt ($MaxCommitPercent - 8)) {
    Write-Output ("ABORT-PREFLIGHT: memory baseline too high (free={0}MB commit={1}%). Close Chrome tabs / other apps and retry." -f $mem.FreeMB, $mem.CommitPercent)
    exit 2
}

# ---- launch build below normal priority ----
$logOut = Join-Path $env:TEMP "safe-build-out.log"
$logErr = Join-Path $env:TEMP "safe-build-err.log"
Remove-Item $logOut, $logErr -ErrorAction SilentlyContinue
$proc = Start-Process cmd.exe -ArgumentList "/c", $Command `
    -WorkingDirectory $WorkDir -WindowStyle Hidden -PassThru `
    -RedirectStandardOutput $logOut -RedirectStandardError $logErr
try { $proc.PriorityClass = "BelowNormal" } catch {}

Write-Output ("[build] started pid={0} cmd={1}" -f $proc.Id, $Command)

# ---- watchdog ----
while (-not $proc.HasExited) {
    Start-Sleep -Seconds $PollSeconds
    if ($proc.HasExited) { break }
    $mem = Get-MemState
    $bad = ($mem.FreeMB -lt $MinFreeMB) -or ($mem.CommitPercent -gt $MaxCommitPercent)
    if ($bad) {
        Write-Output ("WATCHDOG-TRIP: free={0}MB commit={1}% -> killing build tree pid={2}" -f $mem.FreeMB, $mem.CommitPercent, $proc.Id)
        taskkill /T /F /PID $proc.Id 2>$null | Out-Null
        # sweep strays that escaped the tree
        foreach ($n in @("cargo", "rustc", "link")) {
            Get-Process -Name $n -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
        }
        Write-Output "ABORTED-WATCHDOG: build killed before system freeze. Reduce memory usage and retry."
        exit 3
    }
}

$code = $proc.ExitCode
Write-Output "[build] exited with code $code"
if (Test-Path $logOut) { Get-Content $logOut -Tail 25 | ForEach-Object { "[out] $_" } }
if ((Test-Path $logErr) -and ((Get-Item $logErr).Length -gt 0)) { Get-Content $logErr -Tail 10 | ForEach-Object { "[err] $_" } }
exit $code
