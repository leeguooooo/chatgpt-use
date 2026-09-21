# PowerShell installer for Windows. Run from PowerShell:
#   irm https://raw.githubusercontent.com/leeguooooo/chatgpt-use/main/install.ps1 | iex
$ErrorActionPreference = 'Stop'
$repo = 'leeguooooo/chatgpt-use'
$release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
$asset = $release.assets | Where-Object { $_.name -match 'x86_64-pc-windows-msvc' -and $_.name -match '\.zip$' } | Select-Object -First 1
if (-not $asset) { throw 'No Windows release asset found (expected x86_64-pc-windows-msvc.zip).' }
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("chatgpt-use-" + [guid]::NewGuid())
$null = New-Item -ItemType Directory -Path $tmp
try {
  $zip = Join-Path $tmp $asset.name
  Invoke-WebRequest $asset.browser_download_url -OutFile $zip
  Expand-Archive $zip -DestinationPath $tmp -Force
  # Keep one stable custom-bin directory instead of adding one PATH entry per
  # tool. Override with CGU_BIN_DIR when desired.
  $binDir = if ($env:CGU_BIN_DIR) { $env:CGU_BIN_DIR } else { Join-Path $env:USERPROFILE 'chrome-tools' }
  New-Item -ItemType Directory -Path $binDir -Force | Out-Null
  Copy-Item (Join-Path $tmp 'chatgpt-use.exe') (Join-Path $binDir 'chatgpt-use.exe') -Force
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (($userPath -split ';') -notcontains $binDir -and (($env:Path -split ';') -notcontains $binDir)) {
    $currentUserPath = if ($userPath) { $userPath.TrimEnd(';') } else { '' }
    $newPath = (($currentUserPath + ';' + $binDir).Trim(';'))
    if ($newPath.Length -le 2047) { [Environment]::SetEnvironmentVariable('Path', $newPath, 'User') }
    else { Write-Warning "PATH is too long; add $binDir manually or use a PowerShell profile alias." }
  }
  Write-Host "Installed chatgpt-use to $binDir. Open a new terminal to refresh PATH."
} finally { Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue }
