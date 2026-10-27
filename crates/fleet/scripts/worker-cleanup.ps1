# Disk cleanup on a Windows worker. The controller sets RF_MODE (survey or
# clean) and RF_DIAG_DAYS, then calls Invoke-RunnerTarget once per target.
# Every path is derived here from a fixed suffix of the exact runner directory
# and must not pass through a reparse point (symlink or junction). Child
# directories are removed with rmdir /s, which removes junctions without
# following them. .runner, .credentials*, config files, bin and externals are
# never targets. Rows: RFC (survey), RFF (survey: a log file), RFX (removed),
# RFE (refused or failed).
$ErrorActionPreference = 'Stop'
if ($env:RF_MODE -notin @('survey', 'clean')) { throw 'Invalid RF_MODE' }
$survey = $env:RF_MODE -eq 'survey'
$days = [int]$env:RF_DIAG_DAYS

function Write-Row([string[]]$Fields) { Write-Output ($Fields -join "`t") }
function Test-Link([string]$Path) {
  $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
  return [bool]($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint))
}
function Get-KiB([string]$Path) {
  $sum = (Get-ChildItem -LiteralPath $Path -Recurse -Force -File -ErrorAction SilentlyContinue | Measure-Object -Property Length -Sum).Sum
  if ($null -eq $sum) { return '0' }
  return [string][int64][math]::Floor($sum / 1024)
}
function Test-Job([string]$Root) {
  $bin = (Join-Path $Root 'bin') + [IO.Path]::DirectorySeparatorChar
  return [bool](Get-Process -Name Runner.Worker -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($bin, [StringComparison]::OrdinalIgnoreCase) })
}
function Remove-Tree([string]$Path) {
  if (Test-Link $Path) { [IO.Directory]::Delete($Path); return }
  & cmd.exe /d /c rmdir /s /q $Path
  if (Test-Path -LiteralPath $Path) { throw "couldn't remove $Path" }
}
function Clear-Directory([string]$Path) {
  foreach ($child in Get-ChildItem -LiteralPath $Path -Force) {
    if ($child.PSIsContainer) { Remove-Tree $child.FullName }
    elseif ($child.Attributes -band [IO.FileAttributes]::ReparsePoint) { [IO.File]::Delete($child.FullName) }
    else { Remove-Item -LiteralPath $child.FullName -Force }
  }
}
function Get-OldLogs([string]$Path) {
  $cutoff = (Get-Date).AddDays(-$days)
  Get-ChildItem -LiteralPath $Path -File -Force |
    Where-Object { ($_.Name -like 'Runner_*.log' -or $_.Name -like 'Worker_*.log') -and $_.LastWriteTime -lt $cutoff -and -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) } |
    Sort-Object Name
}

function Invoke-RunnerTarget([string]$Kind, [string]$Runner, [string]$Root, [string]$Expected, [string[]]$Files) {
  $suffix = switch ($Kind) {
    'work-temp' { '_work\_temp' }
    'work-actions' { '_work\_actions' }
    'work' { '_work' }
    'diag' { '_diag' }
    'previous-bin' { 'bin.fleet-previous' }
    'previous-externals' { 'externals.fleet-previous' }
    default { $null }
  }
  if (-not $suffix) { Write-Row @('RFE', $Kind, $Runner, "unknown cleanup target $Kind"); return }
  $Root = [IO.Path]::GetFullPath($Root)
  $runnerFile = Join-Path $Root '.runner'
  if ((Test-Link $Root) -or -not (Test-Path -LiteralPath $runnerFile)) {
    if (-not $survey) { Write-Row @('RFE', $Kind, $Runner, "no configured runner at $Root") }
    return
  }
  if ((Get-Content -LiteralPath $runnerFile -Raw | ConvertFrom-Json).agentName -ne $Runner) {
    Write-Row @('RFE', $Kind, $Runner, "the runner at $Root is not $Runner"); return
  }
  $path = $Root
  foreach ($part in $suffix.Split('\')) {
    $path = Join-Path $path $part
    if (Test-Link $path) { Write-Row @('RFE', $Kind, $Runner, "$path is a symlink or junction; left it alone"); return }
  }
  if (-not (Test-Path -LiteralPath $path -PathType Container)) {
    if (-not $survey) { Write-Row @('RFE', $Kind, $Runner, "$path no longer exists") }
    return
  }
  $real = [IO.Path]::GetFullPath($path)
  if ($survey) {
    if ($Kind -eq 'diag') {
      $old = @(Get-OldLogs $real)
      if ($old.Count -eq 0) { return }
      foreach ($file in $old) { Write-Row @('RFF', $Runner, $file.Name) }
      $sum = ($old | Measure-Object -Property Length -Sum).Sum
      Write-Row @('RFC', $Kind, $Runner, $real, [string][int64][math]::Floor($sum / 1024), [string]$old.Count, '')
      return
    }
    if ($Kind -notlike 'previous-*' -and -not (Get-ChildItem -LiteralPath $real -Force | Select-Object -First 1)) { return }
    Write-Row @('RFC', $Kind, $Runner, $real, (Get-KiB $real), '', '')
    return
  }
  if ($real -ne $Expected) { Write-Row @('RFE', $Kind, $Runner, "$path resolves to $real, not the reviewed $Expected; left it alone"); return }
  if (Test-Job $Root) { Write-Row @('RFE', $Kind, $Runner, "a job is running in $Root; left it alone"); return }
  try {
    if ($Kind -eq 'diag') {
      $cutoff = (Get-Date).AddDays(-$days)
      $removed = 0
      $freed = 0
      foreach ($name in $Files) {
        if ($name -notmatch '^(Runner|Worker)_[^\\/]*\.log$') { Write-Row @('RFE', $Kind, $Runner, "unexpected log name $name"); continue }
        $file = Get-Item -LiteralPath (Join-Path $real $name) -Force -ErrorAction SilentlyContinue
        if ($file -and -not $file.PSIsContainer -and -not ($file.Attributes -band [IO.FileAttributes]::ReparsePoint) -and $file.LastWriteTime -lt $cutoff) {
          $freed += [int64][math]::Floor($file.Length / 1024)
          Remove-Item -LiteralPath $file.FullName -Force
          $removed += 1
        }
      }
      Write-Row @('RFX', $Kind, $Runner, $real, [string]$freed, [string]$removed, '')
    } elseif ($Kind -like 'previous-*') {
      $size = Get-KiB $real
      Remove-Tree $real
      Write-Row @('RFX', $Kind, $Runner, $real, $size, '', '')
    } else {
      $size = Get-KiB $real
      Clear-Directory $real
      Write-Row @('RFX', $Kind, $Runner, $real, $size, '', '')
    }
  } catch {
    Write-Row @('RFE', $Kind, $Runner, "couldn't clean $real`: $($_.Exception.Message)")
  }
}
