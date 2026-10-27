# Disk cleanup on a Windows worker. The controller sets RF_MODE (survey or
# clean), RF_DIAG_DAYS and RF_SECONDS (this script's own deadline), then calls
# Invoke-RunnerTarget once per target. The runner's service must already be
# stopped. Every path is derived here from a fixed suffix of the exact runner
# directory and must not pass through a reparse point (symlink or junction).
# Removal is plain PowerShell and .NET: links and junctions are removed
# without being followed, and no name ever passes through cmd.exe. .runner,
# .credentials*, config files and the bin and externals the runner uses are
# never targets; only previous versions of them (bin.VERSION and
# externals.VERSION that bin and externals don't point at) are, by exact
# reviewed name. While removing anything in a runner directory it holds
# RUNNER\.fleet-maintenance (its PID), which start, restart and upgrade refuse
# while that process is alive. Rows: RFC (survey), RFF (survey: a named entry
# of a target), RFX (removed), RFE (refused or failed).
$ErrorActionPreference = 'Stop'
if ($env:RF_MODE -notin @('survey', 'clean')) { throw 'Invalid RF_MODE' }
$survey = $env:RF_MODE -eq 'survey'
$days = [int]$env:RF_DIAG_DAYS
$seconds = if ($env:RF_SECONDS) { [int]$env:RF_SECONDS } else { 840 }
$deadline = (Get-Date).AddSeconds($seconds)
$locks = @()

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
# Processes are attributed to the runner by their executable path or command
# line from CIM. A runner process for which neither can be read (another
# account, without elevation) counts as this runner's: the check fails closed.
# An elevated SSH session attributes precisely.
function Get-RootProcesses([string]$Root) {
  @(Get-CimInstance Win32_Process | Where-Object {
    $where = if ($_.ExecutablePath) { $_.ExecutablePath } else { $_.CommandLine }
    ($where -and $where.IndexOf($Root, [StringComparison]::OrdinalIgnoreCase) -ge 0) -or
      (-not $where -and $_.Name -in @('Runner.Listener.exe', 'Runner.Worker.exe'))
  })
}
function Test-RunnerProcess([string]$Name, [string]$Root) {
  return [bool](Get-RootProcesses $Root | Where-Object { $_.Name -eq "$Name.exe" })
}
function Test-Job([string]$Root) { return Test-RunnerProcess 'Runner.Worker' $Root }
# A self-update writes _work\_update.cmd, then update.finished when it's done.
function Test-Updating([string]$Root) {
  $script = Join-Path $Root '_work\_update.cmd'
  $finished = Join-Path $Root 'update.finished'
  if (-not (Test-Path -LiteralPath $script)) { return $false }
  $written = (Get-Item -LiteralPath $script).LastWriteTime
  if ($written -lt (Get-Date).AddMinutes(-30)) { return $false }
  return -not ((Test-Path -LiteralPath $finished) -and (Get-Item -LiteralPath $finished).LastWriteTime -ge $written)
}
# Takes RUNNER\.fleet-maintenance for this run, unless a live process holds it.
function Lock-Root([string]$Root) {
  $lock = Join-Path $Root '.fleet-maintenance'
  if ($script:locks -contains $lock) { return $true }
  if (Test-Path -LiteralPath $lock) {
    $live = @((Get-Content -LiteralPath $lock -Raw) -split '\s+' | Where-Object { $_ -match '^\d+$' } | Where-Object { Get-Process -Id ([int]$_) -ErrorAction SilentlyContinue })
    if ($live.Count -gt 0) { return $false }
    Remove-Item -LiteralPath $lock -Force
  }
  Set-Content -LiteralPath $lock -Value $PID -NoNewline
  $script:locks += $lock
  return $true
}
# Releases every lock this run took; the controller calls it in a finally block.
function Unlock-All { foreach ($lock in $script:locks) { Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue } }
function Test-Deadline { if ((Get-Date) -ge $deadline) { throw 'the cleanup ran out of time; nothing more was removed' } }
# Removes one entry. A symlink or junction is removed itself and never
# followed; a directory's contents go first. Names are only ever passed as
# -LiteralPath or .NET paths, never through a shell.
function Remove-Tree([string]$Path) {
  Test-Deadline
  $item = Get-Item -LiteralPath $Path -Force
  if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
    if ($item.PSIsContainer) { [IO.Directory]::Delete($item.FullName, $false) } else { [IO.File]::Delete($item.FullName) }
    return
  }
  if ($item.PSIsContainer) {
    foreach ($child in @(Get-ChildItem -LiteralPath $item.FullName -Force)) { Remove-Tree $child.FullName }
    $item.Attributes = [IO.FileAttributes]::Directory
    [IO.Directory]::Delete($item.FullName, $false)
  } else {
    $item.Attributes = [IO.FileAttributes]::Normal
    [IO.File]::Delete($item.FullName)
  }
}
function Clear-Directory([string]$Path) {
  foreach ($child in @(Get-ChildItem -LiteralPath $Path -Force)) { Remove-Tree $child.FullName }
}
function Get-OldLogs([string]$Path) {
  $cutoff = (Get-Date).AddDays(-$days)
  Get-ChildItem -LiteralPath $Path -File -Force |
    Where-Object { ($_.Name -like 'Runner_*.log' -or $_.Name -like 'Worker_*.log') -and $_.LastWriteTime -lt $cutoff -and -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) } |
    Sort-Object Name
}
# The folder bin (or externals) uses: its junction's target name, or itself.
function Get-InUse([string]$Root, [string]$Name) {
  $item = Get-Item -LiteralPath (Join-Path $Root $Name) -Force -ErrorAction SilentlyContinue
  if ($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { return Split-Path -Leaf (@($item.Target)[0]) }
  return $Name
}
function Test-PreviousName([string]$Name, [string]$Entry) {
  return ($Entry -eq "$Name.fleet-previous") -or ($Entry -match ('^' + [regex]::Escape($Name) + '\.\d+\.\d+\.\d+$'))
}
# Previous versions: real folders named NAME.VERSION that NAME doesn't point at.
function Get-PreviousVersions([string]$Root, [string]$Name) {
  if (Test-Path -LiteralPath (Join-Path $Root '.fleet-upgrade-swap')) { return @() }
  $current = Get-InUse $Root $Name
  @(Get-ChildItem -LiteralPath $Root -Directory -Force | Where-Object {
    -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -and (Test-PreviousName $Name $_.Name) -and $_.Name -ne $current
  } | Sort-Object Name)
}

function Invoke-RunnerTarget([string]$Kind, [string]$Runner, [string]$Root, [string]$Expected, [string[]]$Files) {
  $suffix = switch ($Kind) {
    'work-temp' { '_work\_temp' }
    'work-actions' { '_work\_actions' }
    'work' { '_work' }
    'diag' { '_diag' }
    'previous-bin' { '' }
    'previous-externals' { '' }
    default { $null }
  }
  if ($null -eq $suffix) { Write-Row @('RFE', $Kind, $Runner, "unknown cleanup target $Kind"); return }
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
  if ($suffix) {
    foreach ($part in $suffix.Split('\')) {
      $path = Join-Path $path $part
      if (Test-Link $path) { Write-Row @('RFE', $Kind, $Runner, "$path is a symlink or junction; left it alone"); return }
    }
  }
  if (-not (Test-Path -LiteralPath $path -PathType Container)) {
    if (-not $survey) { Write-Row @('RFE', $Kind, $Runner, "$path no longer exists") }
    return
  }
  $real = [IO.Path]::GetFullPath($path)
  $name = if ($Kind -eq 'previous-bin') { 'bin' } elseif ($Kind -eq 'previous-externals') { 'externals' } else { $null }
  if ($survey) {
    $entries = if ($Kind -eq 'diag') { @(Get-OldLogs $real) } elseif ($name) { @(Get-PreviousVersions $real $name) } else { $null }
    if ($null -eq $entries) {
      if (-not (Get-ChildItem -LiteralPath $real -Force | Select-Object -First 1)) { return }
      Write-Row @('RFC', $Kind, $Runner, $real, (Get-KiB $real), '', '')
      return
    }
    if ($entries.Count -eq 0) { return }
    $total = 0
    foreach ($entry in $entries) {
      Write-Row @('RFF', $Kind, $Runner, $entry.Name)
      $total += [int64](Get-KiB $entry.FullName)
    }
    Write-Row @('RFC', $Kind, $Runner, $real, [string]$total, [string]$entries.Count, '')
    return
  }
  if ($real -ne $Expected) { Write-Row @('RFE', $Kind, $Runner, "$path resolves to $real, not the reviewed $Expected; left it alone"); return }
  $processes = Get-RootProcesses $Root
  if ($processes | Where-Object { $_.Name -eq 'Runner.Worker.exe' }) { Write-Row @('RFE', $Kind, $Runner, "a job is running in $Root (or a runner job whose path can't be read); left it alone"); return }
  if ($processes | Where-Object { $_.Name -eq 'Runner.Listener.exe' }) { Write-Row @('RFE', $Kind, $Runner, "$Runner's service is still running; it must be stopped before cleaning"); return }
  if ($processes -or (Test-Updating $Root)) { Write-Row @('RFE', $Kind, $Runner, "something is still running from $Root, or a runner self-update is in progress; left it alone"); return }
  if ((Get-Date) -ge $deadline) { Write-Row @('RFE', $Kind, $Runner, "the cleanup ran out of time before $real; nothing more was removed"); return }
  if (-not (Lock-Root $Root)) { Write-Row @('RFE', $Kind, $Runner, "another cleanup is still working in $Root; left it alone"); return }
  try {
    if ($Kind -eq 'diag') {
      $cutoff = (Get-Date).AddDays(-$days)
      $removed = 0
      $freed = 0
      foreach ($entry in $Files) {
        if ($entry -notmatch '^(Runner|Worker)_[^\\/]*\.log$') { Write-Row @('RFE', $Kind, $Runner, "unexpected log name $entry"); continue }
        $file = Get-Item -LiteralPath (Join-Path $real $entry) -Force -ErrorAction SilentlyContinue
        if ($file -and -not $file.PSIsContainer -and -not ($file.Attributes -band [IO.FileAttributes]::ReparsePoint) -and $file.LastWriteTime -lt $cutoff) {
          $freed += [int64][math]::Floor($file.Length / 1024)
          Remove-Item -LiteralPath $file.FullName -Force
          $removed += 1
        }
      }
      Write-Row @('RFX', $Kind, $Runner, $real, [string]$freed, [string]$removed, '')
    } elseif ($name) {
      if (Test-Path -LiteralPath (Join-Path $real '.fleet-upgrade-swap')) { Write-Row @('RFE', $Kind, $Runner, "an upgrade of $Runner was interrupted; its previous versions are kept"); return }
      $current = Get-InUse $real $name
      $removed = 0
      $freed = 0
      foreach ($entry in $Files) {
        if (-not (Test-PreviousName $name $entry) -or $entry -eq $current) { Write-Row @('RFE', $Kind, $Runner, "$entry isn't a previous version of $name here; left it alone"); continue }
        $folder = Join-Path $real $entry
        if (-not (Test-Path -LiteralPath $folder -PathType Container) -or (Test-Link $folder)) { continue }
        $size = [int64](Get-KiB $folder)
        Remove-Tree $folder
        $removed += 1
        $freed += $size
      }
      Write-Row @('RFX', $Kind, $Runner, $real, [string]$freed, [string]$removed, '')
    } else {
      $size = Get-KiB $real
      Clear-Directory $real
      Write-Row @('RFX', $Kind, $Runner, $real, $size, '', '')
    }
  } catch {
    Write-Row @('RFE', $Kind, $Runner, "couldn't clean $real`: $($_.Exception.Message)")
  }
}
