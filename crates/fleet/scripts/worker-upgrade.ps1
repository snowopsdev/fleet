# Upgrades one stopped runner in place on a Windows worker: downloads the
# release, verifies the SHA-256 digest GitHub publishes, and replaces bin and
# externals inside the exact runner directory. The directory is never moved,
# and .runner, .credentials* and the configuration are never touched. The
# replaced folders are kept as bin.fleet-previous and externals.fleet-previous;
# a failure while swapping puts them back. RF_* values are process-only.
$ErrorActionPreference = 'Stop'
foreach ($required in @('RF_ROOT', 'RF_NAME', 'RF_ASSET_URL', 'RF_ASSET_NAME', 'RF_CHECKSUM', 'RF_VERSION')) {
  if (-not [Environment]::GetEnvironmentVariable($required)) { throw "Missing $required" }
}
$assetUri = [Uri]$env:RF_ASSET_URL
if ($assetUri.Scheme -ne 'https' -or $assetUri.Host -ne 'github.com' -or -not $assetUri.AbsolutePath.StartsWith('/actions/runner/releases/download/')) {
  throw "Refusing unexpected runner asset URL: $env:RF_ASSET_URL"
}
$allowedRoot = [IO.Path]::GetFullPath('C:\gh-runners')
$root = [IO.Path]::GetFullPath($env:RF_ROOT)
if (-not $root.StartsWith($allowedRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
  throw "Refusing root outside C:\gh-runners: $root"
}
function Test-Link([string]$Path) {
  $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
  return [bool]($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint))
}
function Remove-Tree([string]$Path) {
  if (Test-Link $Path) { throw "refusing to remove $Path`: it is a symlink or junction" }
  & cmd.exe /d /c rmdir /s /q $Path
  if (Test-Path -LiteralPath $Path) { throw "couldn't remove $Path" }
}
$runnerFile = Join-Path $root '.runner'
if ((Test-Link $root) -or -not (Test-Path -LiteralPath $runnerFile)) { throw "No configured runner at exact path $root" }
if ((Get-Content -LiteralPath $runnerFile -Raw | ConvertFrom-Json).agentName -ne $env:RF_NAME) {
  throw "The runner at exact path $root is not $env:RF_NAME"
}
$bin = (Join-Path $root 'bin') + [IO.Path]::DirectorySeparatorChar
$running = Get-Process -Name Runner.Worker, Runner.Listener -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($bin, [StringComparison]::OrdinalIgnoreCase) }
if ($running | Where-Object { $_.ProcessName -eq 'Runner.Worker' }) { throw "a job is running in $root; refusing to upgrade $env:RF_NAME" }
if ($running) { throw "$env:RF_NAME is still running; its service must be stopped before upgrading" }

$current = (& (Join-Path $root 'bin\Runner.Listener.exe') --version 2>$null | Select-Object -Last 1)
if ($current -eq $env:RF_VERSION) { Write-Output "$env:RF_NAME already runs $env:RF_VERSION"; exit 0 }

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
  $version = (& $listener --version 2>$null | Select-Object -Last 1)
  if ($version -ne $env:RF_VERSION) { throw "The downloaded runner reports version '$version', expected $env:RF_VERSION" }
  foreach ($name in @('bin', 'externals')) {
    $previous = Join-Path $root "$name.fleet-previous"
    if (Test-Path -LiteralPath $previous) { Remove-Tree $previous }
  }
  $swapping = $true
  foreach ($name in @('bin', 'externals')) {
    Rename-Item -LiteralPath (Join-Path $root $name) -NewName "$name.fleet-previous"
    Move-Item -LiteralPath (Join-Path $extracted $name) -Destination (Join-Path $root $name)
  }
  $swapping = $false
} catch {
  if ($swapping) {
    foreach ($name in @('bin', 'externals')) {
      $previous = Join-Path $root "$name.fleet-previous"
      if (Test-Path -LiteralPath $previous) {
        $current = Join-Path $root $name
        if (Test-Path -LiteralPath $current) { Move-Item -LiteralPath $current -Destination (Join-Path $stage "failed-$name") }
        Rename-Item -LiteralPath $previous -NewName $name
      }
    }
    Write-Warning "Restored the previous bin and externals of $env:RF_NAME"
  }
  throw
} finally {
  if (Test-Path -LiteralPath $stage) { & cmd.exe /d /c rmdir /s /q $stage }
}
Write-Output "Replaced bin and externals of $env:RF_NAME with $env:RF_VERSION; the previous files are in bin.fleet-previous and externals.fleet-previous"
