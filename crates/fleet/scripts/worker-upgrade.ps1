# Upgrades one stopped runner in place on a Windows worker, using the runner's
# own self-update layout: the release goes to bin.VERSION and
# externals.VERSION inside the exact runner directory, and bin and externals
# become junctions to them. A runner still on plain bin and externals folders
# is converted the way its self-update does. The old versioned folders stay as
# the rollback copy until the runner's next self-update prunes them. The
# directory is never moved, and .runner, .credentials* and the configuration
# are never touched. A swap marker written before the first change and
# removed after the last lets the next run roll back an interrupted swap.
# RF_* values are process-only.
$ErrorActionPreference = 'Stop'
foreach ($required in @('RF_ROOT', 'RF_NAME', 'RF_ASSET_URL', 'RF_ASSET_NAME', 'RF_CHECKSUM', 'RF_VERSION')) {
  if (-not [Environment]::GetEnvironmentVariable($required)) { throw "Missing $required" }
}
$assetUri = [Uri]$env:RF_ASSET_URL
if ($assetUri.Scheme -ne 'https' -or $assetUri.Host -ne 'github.com' -or -not $assetUri.AbsolutePath.StartsWith('/actions/runner/releases/download/')) {
  throw "Refusing unexpected runner asset URL: $env:RF_ASSET_URL"
}
if ($env:RF_VERSION -notmatch '^\d+\.\d+\.\d+$') { throw "Refusing unexpected runner version: $env:RF_VERSION" }
$version = $env:RF_VERSION
$allowedRoot = [IO.Path]::GetFullPath('C:\gh-runners')
$root = [IO.Path]::GetFullPath($env:RF_ROOT)
if (-not $root.StartsWith($allowedRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
  throw "Refusing root outside C:\gh-runners: $root"
}
function Test-Link([string]$Path) {
  $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
  return [bool]($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint))
}
# Removes one entry without following links or junctions, and without cmd.exe.
function Remove-Entry([string]$Path) {
  $item = Get-Item -LiteralPath $Path -Force
  if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
    if ($item.PSIsContainer) { [IO.Directory]::Delete($item.FullName, $false) } else { [IO.File]::Delete($item.FullName) }
    return
  }
  if ($item.PSIsContainer) {
    foreach ($child in @(Get-ChildItem -LiteralPath $item.FullName -Force)) { Remove-Entry $child.FullName }
    $item.Attributes = [IO.FileAttributes]::Directory
    [IO.Directory]::Delete($item.FullName, $false)
  } else {
    $item.Attributes = [IO.FileAttributes]::Normal
    [IO.File]::Delete($item.FullName)
  }
}
$runnerFile = Join-Path $root '.runner'
if ((Test-Link $root) -or -not (Test-Path -LiteralPath $runnerFile)) { throw "No configured runner at exact path $root" }
if ((Get-Content -LiteralPath $runnerFile -Raw | ConvertFrom-Json).agentName -ne $env:RF_NAME) {
  throw "The runner at exact path $root is not $env:RF_NAME"
}

# Anything started from the runner directory (its listener, a job, its
# self-update script) means it isn't stopped. Processes are attributed by
# their executable path or command line from CIM; a runner process neither
# can be read for (another account, without elevation) counts as this
# runner's, so the check fails closed. An elevated session attributes
# precisely.
# Compared with a trailing separator, so runner-1 doesn't match runner-10.
# Checked again right before the swap.
function Assert-Stopped {
  $prefix = $root.TrimEnd('\') + '\'
  $fromRoot = @(Get-CimInstance Win32_Process | Where-Object {
    $where = if ($_.ExecutablePath) { $_.ExecutablePath } else { $_.CommandLine }
    ($where -and $where.IndexOf($prefix, [StringComparison]::OrdinalIgnoreCase) -ge 0) -or
      (-not $where -and $_.Name -in @('Runner.Listener.exe', 'Runner.Worker.exe'))
  })
  if ($fromRoot | Where-Object { $_.Name -eq 'Runner.Worker.exe' }) { throw "a job is running in $root; refusing to upgrade $env:RF_NAME" }
  if ($fromRoot) { throw "$env:RF_NAME is still running (a process from $root, such as its service or a self-update); it must be stopped before upgrading" }
}
Assert-Stopped
# A self-update writes _work\_update.cmd, then update.finished when it's done.
$updateScript = Join-Path $root '_work\_update.cmd'
$finished = Join-Path $root 'update.finished'
if ((Test-Path -LiteralPath $updateScript) -and (Get-Item -LiteralPath $updateScript).LastWriteTime -gt (Get-Date).AddMinutes(-30) -and
    -not ((Test-Path -LiteralPath $finished) -and (Get-Item -LiteralPath $finished).LastWriteTime -ge (Get-Item -LiteralPath $updateScript).LastWriteTime)) {
  throw "a runner self-update of $env:RF_NAME started in the last 30 minutes and hasn't finished; try again later"
}
# A cleanup holds the maintenance lock while it works; the upgrade takes it
# too, until it ends. A recycled PID keeps a lock live: that fails closed.
$lock = Join-Path $root '.fleet-maintenance'
if (Test-Path -LiteralPath $lock) {
  $live = @((Get-Content -LiteralPath $lock -Raw) -split '\s+' | Where-Object { $_ -match '^\d+$' } | Where-Object { Get-Process -Id ([int]$_) -ErrorAction SilentlyContinue })
  if ($live.Count -gt 0) { throw "a cleanup or another upgrade is still working in $root (its processes are recorded in $lock); try again when it has finished. If those processes are unrelated, remove that file by hand" }
  Remove-Item -LiteralPath $lock -Force
}
Set-Content -LiteralPath $lock -Value $PID -NoNewline
try {

# Where bin or externals points, as a folder name in the root: bin.2.329.0, or
# bin itself for a plain folder.
function Get-Target([string]$Name) {
  $path = Join-Path $root $Name
  $item = Get-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
  if (-not $item) { throw "$path is missing; repair it by hand" }
  if (-not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { return $Name }
  $target = [IO.Path]::GetFullPath((@($item.Target)[0]))
  $leaf = Split-Path -Leaf $target
  if ((Split-Path -Parent $target) -ne $root -or -not $leaf.StartsWith("$Name.", [StringComparison]::OrdinalIgnoreCase) -or -not (Test-Path -LiteralPath $target -PathType Container)) {
    throw "$path points outside the runner directory ($target); repair it by hand"
  }
  return $leaf
}
function Set-Link([string]$Name, [string]$Folder) {
  $path = Join-Path $root $Name
  if (Test-Link $path) { [IO.Directory]::Delete($path, $false) }
  New-Item -ItemType Junction -Path $path -Target (Join-Path $root $Folder) | Out-Null
}
$marker = Join-Path $root '.fleet-upgrade-swap'
# The marker names the folder that held the old version for bin and externals;
# rolling back points each link at it again.
# Returns $true only when every link points back; the marker stays otherwise,
# so the next run tries again.
function Restore-Marker {
  $failed = $false
  foreach ($line in @(Get-Content -LiteralPath $marker)) {
    $name, $previous = $line -split '=', 2
    if ($name -notin @('bin', 'externals') -or -not $previous.StartsWith("$name.")) { continue }
    $folder = Join-Path $root $previous
    $path = Join-Path $root $name
    # Still the plain folder it was: this one was never moved.
    if ((Test-Path -LiteralPath $path -PathType Container) -and -not (Test-Link $path)) { continue }
    if (-not (Test-Path -LiteralPath $folder -PathType Container) -or (Test-Link $folder)) { $failed = $true; continue }
    try {
      if ((Test-Link $path) -or -not (Test-Path -LiteralPath $path)) { Set-Link $name $previous }
      if ((Get-Target $name) -ne $previous) { $failed = $true }
    } catch { $failed = $true }
  }
  if ($failed) { return $false }
  Remove-Item -LiteralPath $marker -Force
  return $true
}
if (Test-Path -LiteralPath $marker) {
  if (-not (Restore-Marker)) {
    throw "An earlier upgrade of $env:RF_NAME was interrupted and couldn't be rolled back: with its service stopped, point bin and externals at the folders named in $marker, remove the marker, then upgrade again"
  }
  Write-Output "Rolled back an interrupted upgrade of $env:RF_NAME"
}

$oldBin = Get-Target 'bin'
$oldExternals = Get-Target 'externals'
$current = (& (Join-Path $root 'bin\Runner.Listener.exe') --version 2>$null | Select-Object -Last 1)
if ($current -eq $version) { Write-Output "$env:RF_NAME already runs $version"; return }
if (($oldBin -eq 'bin' -or $oldExternals -eq 'externals') -and $current -notmatch '^\d+\.\d+\.\d+$') {
  throw "Can't tell which version $env:RF_NAME runs ('$current'), so its folders can't be kept as a rollback copy; refusing"
}
$keep = @{}
foreach ($pair in @(@('bin', $oldBin), @('externals', $oldExternals))) {
  $name, $old = $pair
  $kept = if ($old -eq $name) { "$name.$current" } else { $old }
  if ($old -eq $name -and (Test-Path -LiteralPath (Join-Path $root $kept))) { throw "$root\$name is a plain folder but $root\$kept already exists; inspect both before upgrading" }
  if ($kept -eq "$name.$version") { throw "$env:RF_NAME's $name already points at $name.$version; nothing to swap" }
  if (Test-Link (Join-Path $root "$name.$version")) { throw "$root\$name.$version is a symlink or junction; refusing" }
  $keep[$name] = $kept
}

$stage = Join-Path $root ('.fleet-upgrade-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $stage | Out-Null
$swapping = $false
try {
  $archive = Join-Path $stage 'runner.zip'
  Write-Output "Downloading $env:RF_ASSET_NAME"
  Invoke-WebRequest -Uri $env:RF_ASSET_URL -OutFile $archive -UseBasicParsing
  $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
  if ($actual -ne $env:RF_CHECKSUM.ToLowerInvariant()) { throw "SHA-256 mismatch for $env:RF_ASSET_NAME" }
  Write-Output 'Verified GitHub SHA-256 digest'
  $extracted = Join-Path $stage 'new'
  Expand-Archive -LiteralPath $archive -DestinationPath $extracted
  $listener = Join-Path $extracted 'bin\Runner.Listener.exe'
  if (-not (Test-Path -LiteralPath $listener) -or -not (Test-Path -LiteralPath (Join-Path $extracted 'externals'))) {
    throw "$env:RF_ASSET_NAME does not contain bin\Runner.Listener.exe and externals"
  }
  $downloaded = (& $listener --version 2>$null | Select-Object -Last 1)
  if ($downloaded -ne $version) { throw "The downloaded runner reports version '$downloaded', expected $version" }
  # New folders beside the current ones; nothing points at them yet.
  foreach ($name in @('bin', 'externals')) {
    $folder = Join-Path $root "$name.$version"
    if (Test-Path -LiteralPath $folder) { Remove-Entry $folder }
    Move-Item -LiteralPath (Join-Path $extracted $name) -Destination $folder
  }
  Assert-Stopped
  Set-Content -LiteralPath $marker -Value "bin=$($keep['bin'])`nexternals=$($keep['externals'])"
  $swapping = $true
  foreach ($name in @('bin', 'externals')) {
    if ((Get-Target $name) -eq $name) { Rename-Item -LiteralPath (Join-Path $root $name) -NewName $keep[$name] }
    Set-Link $name "$name.$version"
  }
  # Complete: removing the marker is the commit point.
  Remove-Item -LiteralPath $marker -Force
  $swapping = $false
  # Refresh the root files a self-update copies (the package's top-level
  # scripts and templates), each through a temporary file and a rename.
  $stale = @()
  foreach ($file in @(Get-ChildItem -LiteralPath $extracted -File -Force)) {
    if ($file.Name.StartsWith('.') -or $file.Extension -notin @('.cmd', '.sh', '.template')) { continue }
    $destination = Join-Path $root $file.Name
    if (Test-Path -LiteralPath $destination -PathType Container) { $stale += $file.Name; continue }
    try {
      $temporary = Join-Path $root ".$($file.Name).fleet-new"
      Copy-Item -LiteralPath $file.FullName -Destination $temporary -Force
      Move-Item -LiteralPath $temporary -Destination $destination -Force
    } catch { $stale += $file.Name }
  }
  if ($stale.Count -gt 0) { Write-Warning "Switched $env:RF_NAME to $version, but couldn't refresh: $($stale -join ', ')" }
} catch {
  if ($swapping -and (Test-Path -LiteralPath $marker)) {
    if (Restore-Marker) { Write-Warning "Restored the previous bin and externals of $env:RF_NAME" }
    else { Write-Warning "Couldn't restore the previous bin and externals of $env:RF_NAME; they're named in $marker" }
  }
  throw
} finally {
  if (Test-Path -LiteralPath $stage) { Remove-Entry $stage }
}
Write-Output "Switched $env:RF_NAME to $version (bin -> bin.$version); the previous version stays beside it as the rollback copy"
} finally {
  Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue
}
