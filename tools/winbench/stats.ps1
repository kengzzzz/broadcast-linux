# CPU (% of one core), private memory and GPU engine time per process over a window,
# plus nvidia-smi every 250 ms. Writes results\NAME-procs.csv, NAME-gpu.csv and NAME.txt.
# Usage: powershell -ExecutionPolicy Bypass -File stats.ps1 -Name NAME [-Seconds 30] [-Delay 0]
param(
    [Parameter(Mandatory)][string]$Name,
    [int]$Seconds = 30,
    [int]$Delay = 0
)
$ErrorActionPreference = 'Stop'
$dir = Join-Path $PSScriptRoot 'results'
New-Item -ItemType Directory -Force $dir | Out-Null

function Get-Procs {
    $map = @{}
    foreach ($p in Get-CimInstance Win32_PerfRawData_PerfProc_Process) {
        if ($p.Name -in '_Total', 'Idle') { continue }
        $map[[string]$p.IDProcess] = $p
    }
    $map
}

function Get-GpuTime {
    $map = @{}
    try {
        foreach ($e in Get-CimInstance Win32_PerfRawData_GPUPerformanceCounters_GPUEngine) {
            if ($e.Name -match '^pid_(\d+)_') {
                $map[$Matches[1]] = [double]$map[$Matches[1]] + [double]$e.RunningTime
            }
        }
    } catch {}
    $map
}

Start-Sleep -Seconds $Delay
$gpuCsv = Join-Path $dir "$Name-gpu.csv"
$smi = Start-Process -FilePath nvidia-smi.exe -NoNewWindow -PassThru -RedirectStandardOutput $gpuCsv `
    -ArgumentList '--query-gpu=timestamp,name,driver_version,pstate,utilization.gpu,memory.used,power.draw,clocks.gr,clocks.mem', '--format=csv', '-lms', '250'

$a = Get-Procs
$ga = Get-GpuTime
$clock = [Diagnostics.Stopwatch]::StartNew()
Start-Sleep -Seconds $Seconds
$b = Get-Procs
$gb = Get-GpuTime
$ticks = [double]$clock.Elapsed.Ticks
Stop-Process -Id $smi.Id -ErrorAction SilentlyContinue

$rows = foreach ($id in $b.Keys) {
    $old = $a[$id]
    $new = $b[$id]
    if (-not $old -or $old.Name -ne $new.Name) { continue }
    $gpu = 0
    if ($gb.ContainsKey($id) -and $ga.ContainsKey($id)) { $gpu = ($gb[$id] - $ga[$id]) / $ticks * 100 }
    [pscustomobject]@{
        Process             = $new.Name
        PID                 = [int]$id
        CpuPctOfCore        = [math]::Round(([double]$new.PercentProcessorTime - [double]$old.PercentProcessorTime) / $ticks * 100, 2)
        PrivateMB           = [math]::Round($new.PrivateBytes / 1MB)
        WorkingSetPrivateMB = [math]::Round($new.WorkingSetPrivate / 1MB)
        GpuEnginePct        = [math]::Round($gpu, 2)
    }
}
$rows = $rows | Sort-Object CpuPctOfCore -Descending
$rows | Export-Csv -NoTypeInformation (Join-Path $dir "$Name-procs.csv")

$interesting = $rows | Where-Object {
    $_.CpuPctOfCore -ge 0.5 -or $_.GpuEnginePct -ge 0.5 -or $_.Process -match 'broadcast|nvidia|^nv|audiodg|winbench'
}
$report = @(
    "$Name over $Seconds s ($([Environment]::ProcessorCount) logical CPUs)"
    $interesting | Select-Object -First 30 | Format-Table -AutoSize | Out-String -Width 200
    "all processes: {0:N1}% of a core" -f ($rows | Measure-Object CpuPctOfCore -Sum).Sum
) -join "`r`n"
$report | Out-File -Encoding utf8 (Join-Path $dir "$Name.txt")
$report
