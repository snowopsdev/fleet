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
  # With a trailing separator, so runner-1 doesn't match runner-10.
  $prefix = $Root.TrimEnd('\') + '\'
  @(Get-CimInstance Win32_Process | Where-Object {
    $where = if ($_.ExecutablePath) { $_.ExecutablePath } else { $_.CommandLine }
    ($where -and $where.IndexOf($prefix, [StringComparison]::OrdinalIgnoreCase) -ge 0) -or
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
    # A recycled PID keeps the lock live: that fails closed.
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
$timeoutMessage = 'the cleanup ran out of time and stopped part-way'
function Test-Deadline { if ((Get-Date) -ge $deadline) { throw $timeoutMessage } }
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
# The folder bin (or externals) uses: a plain folder (itself), or a junction or
# link that resolves to an existing NAME.VERSION folder directly in the root.
# $null when it's missing, dangling or points anywhere else: then nothing counts
# as a previous version, since the version in use can't be told apart.
function Get-InUse([string]$Root, [string]$Name) {
  $item = Get-Item -LiteralPath (Join-Path $Root $Name) -Force -ErrorAction SilentlyContinue
  if (-not $item) { return $null }
  if (-not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { return $(if ($item.PSIsContainer) { $Name } else { $null }) }
  $target = @($item.Target)[0]
  if (-not $target) { return $null }
  $target = [IO.Path]::GetFullPath($target)
  $leaf = Split-Path -Leaf $target
  if ((Split-Path -Parent $target) -ne $Root -or -not $leaf.StartsWith("$Name.", [StringComparison]::OrdinalIgnoreCase) -or -not (Test-Path -LiteralPath $target -PathType Container)) { return $null }
  return $leaf
}
function Test-PreviousName([string]$Name, [string]$Entry) {
  return ($Entry -eq "$Name.fleet-previous") -or ($Entry -match ('^' + [regex]::Escape($Name) + '\.\d+\.\d+\.\d+$'))
}
# Previous versions: real folders named NAME.VERSION that NAME doesn't point at.
# $null when NAME can't be resolved; nothing while an upgrade is mid-swap.
function Get-PreviousVersions([string]$Root, [string]$Name) {
  if (Test-Path -LiteralPath (Join-Path $Root '.fleet-upgrade-swap')) { return @() }
  $current = Get-InUse $Root $Name
  if (-not $current) { return $null }
  @(Get-ChildItem -LiteralPath $Root -Directory -Force | Where-Object {
    -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -and (Test-PreviousName $Name $_.Name) -and $_.Name -ne $current
  } | Sort-Object Name)
}
# Staging folders an interrupted upgrade left behind (.fleet-upgrade-GUID).
function Test-LeftoverName([string]$Entry) { return $Entry -match '^\.fleet-upgrade-[0-9a-fA-F-]+$' }
function Get-UpgradeLeftovers([string]$Root) {
  if (Test-Path -LiteralPath (Join-Path $Root '.fleet-upgrade-swap')) { return @() }
  $lock = Join-Path $Root '.fleet-maintenance'
  if ((Test-Path -LiteralPath $lock) -and @((Get-Content -LiteralPath $lock -Raw) -split '\s+' | Where-Object { $_ -match '^\d+$' } | Where-Object { Get-Process -Id ([int]$_) -ErrorAction SilentlyContinue }).Count -gt 0) { return @() }
  @(Get-ChildItem -LiteralPath $Root -Directory -Force | Where-Object {
    -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -and (Test-LeftoverName $_.Name)
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
    'upgrade-leftovers' { '' }
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
    if ($name -and $null -eq (Get-InUse $real $name)) {
      Write-Row @('RFE', $Kind, $Runner, "$real\$name is missing or doesn't point at a $name.VERSION folder here; previous versions are kept"); return
    }
    $entries = if ($Kind -eq 'diag') { @(Get-OldLogs $real) } elseif ($name) { @(Get-PreviousVersions $real $name) } elseif ($Kind -eq 'upgrade-leftovers') { @(Get-UpgradeLeftovers $real) } else { $null }
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
  if (-not (Lock-Root $Root)) { Write-Row @('RFE', $Kind, $Runner, "another cleanup or an upgrade is still working in $Root (its processes are recorded in $Root\.fleet-maintenance); left it alone. If those processes are unrelated, remove that file by hand"); return }
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
      if (-not $current) { Write-Row @('RFE', $Kind, $Runner, "$real\$name is missing or doesn't point at a $name.VERSION folder here; previous versions are kept"); return }
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
    } elseif ($Kind -eq 'upgrade-leftovers') {
      if (Test-Path -LiteralPath (Join-Path $real '.fleet-upgrade-swap')) { Write-Row @('RFE', $Kind, $Runner, "an upgrade of $Runner was interrupted; its folders are kept"); return }
      $removed = 0
      $freed = 0
      foreach ($entry in $Files) {
        if (-not (Test-LeftoverName $entry)) { Write-Row @('RFE', $Kind, $Runner, "$entry isn't an upgrade's staging folder; left it alone"); continue }
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
    if ($_.Exception.Message -eq $timeoutMessage) {
      # RFT: stopped part-way at the deadline; the controller holds the runner.
      Write-Row @('RFT', $Kind, $Runner, "ran out of time cleaning $real; stopped part-way, so the runner stays stopped")
    } else {
      Write-Row @('RFE', $Kind, $Runner, "couldn't clean $real`: $($_.Exception.Message)")
    }
  }
}
