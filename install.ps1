<#
.SYNOPSIS
  Helix installer for Windows.

.DESCRIPTION
  irm https://raw.githubusercontent.com/areeb-h/helix/main/install.ps1 | iex

  Downloads the prebuilt helix.exe for your architecture, VERIFIES its SHA-256,
  installs it to %LOCALAPPDATA%\Helix\bin, and puts that directory on your user
  PATH. No admin rights, no runtime, nothing else to install.

  Env knobs (set before running):
    $env:HELIX_INSTALL_DIR   where to put helix.exe
    $env:HELIX_VERSION       release tag (default: latest)
    $env:HELIX_NO_VERIFY     "1" to skip checksum verification (NOT recommended)
    $env:HELIX_REPO          owner/name on GitHub
#>

$ErrorActionPreference = 'Stop'

$Repo       = if ($env:HELIX_REPO) { $env:HELIX_REPO } else { 'areeb-h/helix' }
$InstallDir = if ($env:HELIX_INSTALL_DIR) { $env:HELIX_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Helix\bin' }
$Version    = if ($env:HELIX_VERSION) { $env:HELIX_VERSION } else { 'latest' }

function Say([string]$m) { Write-Host "helix-install: $m" -ForegroundColor Cyan }
function Die([string]$m) { Write-Host "helix-install: error: $m" -ForegroundColor Red; exit 1 }

# --- target -----------------------------------------------------------------
$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
switch ($arch) {
  'X64'   { $target = 'x86_64-pc-windows-msvc' }
  'Arm64' {
    # No aarch64-windows artifact is published yet; x64 runs under emulation on
    # Windows 11 ARM. Say which one you are getting rather than silently
    # installing a binary the user did not ask for.
    Say 'no native ARM64 build yet — installing the x64 build (runs under emulation).'
    $target = 'x86_64-pc-windows-msvc'
  }
  default { Die "unsupported architecture '$arch'." }
}

$base = if ($Version -eq 'latest') {
  "https://github.com/$Repo/releases/latest/download"
} else {
  "https://github.com/$Repo/releases/download/$Version"
}
$asset = "helix-$target.zip"

# --- download ---------------------------------------------------------------
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("helix-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  $zip = Join-Path $tmp $asset
  Say "downloading $base/$asset"
  try {
    Invoke-WebRequest -Uri "$base/$asset" -OutFile $zip -UseBasicParsing
  } catch {
    Die @"
could not download a prebuilt binary ($($_.Exception.Message)).

If no release is published yet, build from a checkout instead:
    cargo install --path .
(needs a Rust toolchain from https://rustup.rs)
"@
  }

  # --- verify ---------------------------------------------------------------
  if ($env:HELIX_NO_VERIFY -ne '1') {
    $sums = Join-Path $tmp 'SHA256SUMS'
    $haveSums = $true
    try {
      Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sums -UseBasicParsing
    } catch { $haveSums = $false }

    if ($haveSums) {
      $line = Select-String -Path $sums -Pattern ([regex]::Escape($asset)) | Select-Object -First 1
      if (-not $line) { Die "no checksum for $asset in SHA256SUMS." }
      $want = ($line.Line -split '\s+')[0].ToLower()
      $got  = (Get-FileHash -Path $zip -Algorithm SHA256).Hash.ToLower()
      if ($want -ne $got) {
        Die "CHECKSUM MISMATCH for $asset`n  expected $want`n  actual   $got"
      }
      Say "checksum ok ($asset)"
    } else {
      Say 'warning: no SHA256SUMS published for this release — INSTALLING UNVERIFIED'
    }
  }

  # --- install --------------------------------------------------------------
  Expand-Archive -Path $zip -DestinationPath $tmp -Force
  $exe = Get-ChildItem -Path $tmp -Filter 'helix.exe' -Recurse | Select-Object -First 1
  if (-not $exe) { Die 'archive did not contain helix.exe.' }

  New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
  Copy-Item -Path $exe.FullName -Destination (Join-Path $InstallDir 'helix.exe') -Force
  Say "installed helix.exe -> $InstallDir"

  # --- PATH (user scope; no admin needed) -----------------------------------
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if ($userPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable('Path', "$userPath;$InstallDir", 'User')
    Say "added $InstallDir to your user PATH"
    Say 'open a NEW terminal for that to take effect.'
  }
  # Make it usable in THIS session too, so the next line actually runs.
  $env:Path = "$env:Path;$InstallDir"

  & (Join-Path $InstallDir 'helix.exe') version
  Say 'done. try:  helix eval "print(1 + 2)"   or   helix repl'
}
finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
