# Installs nyq on Windows from the latest GitHub release.
#
#   irm https://nyq.zalthar.com/install.ps1 | iex
#
# Set $env:NYQ_VERSION (for example "0.0.1") to pin a version, and
# $env:NYQ_INSTALL_DIR to install somewhere other than %LOCALAPPDATA%\nyq\bin.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'Znskr/nyq'
$target = 'x86_64-pc-windows-gnu'
$archive = "nyq-$target.zip"

if (-not [Environment]::Is64BitOperatingSystem) {
  throw 'nyq needs 64-bit Windows.'
}

if ($env:NYQ_VERSION) {
  $base = "https://github.com/$repo/releases/download/v$($env:NYQ_VERSION.TrimStart('v'))"
  $label = "v$($env:NYQ_VERSION.TrimStart('v'))"
} else {
  $base = "https://github.com/$repo/releases/latest/download"
  $label = 'the latest release'
}
$dir = if ($env:NYQ_INSTALL_DIR) { $env:NYQ_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'nyq\bin' }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("nyq-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  Write-Host "Downloading nyq ($label) for $target..."
  $zip = Join-Path $tmp $archive
  try {
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$archive" -OutFile $zip
    Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')
  } catch {
    throw "failed to download $base/$archive ($($_.Exception.Message))"
  }

  $line = Get-Content (Join-Path $tmp 'SHA256SUMS') | Where-Object { $_ -match "\s\*?$([regex]::Escape($archive))$" } | Select-Object -First 1
  if (-not $line) { throw "SHA256SUMS has no entry for $archive" }
  $want = ($line -split '\s+')[0].ToLower()
  $got = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
  if ($want -ne $got) { throw "checksum mismatch for $archive (expected $want, got $got)" }

  Expand-Archive -Path $zip -DestinationPath (Join-Path $tmp 'out') -Force
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  Copy-Item -Force (Join-Path $tmp 'out\nyq.exe') (Join-Path $dir 'nyq.exe')
} finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

$version = & (Join-Path $dir 'nyq.exe') --version
Write-Host "Installed $version to $dir"

# Add the install directory to the user's PATH once.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$parts = @($userPath -split ';' | Where-Object { $_ })
if ($parts -notcontains $dir) {
  [Environment]::SetEnvironmentVariable('Path', (($parts + $dir) -join ';'), 'User')
  $env:Path = "$env:Path;$dir"
  Write-Host "Added $dir to your PATH. Open a new terminal to pick it up."
}
Write-Host 'Run nyq to get started. Docs: https://nyq.zalthar.com/docs'
