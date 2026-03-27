# nemesis8 installer for Windows
# Usage: powershell -c "irm https://nemesis8.nuts.services/install.ps1 | iex"

$ErrorActionPreference = "Stop"

Write-Host ""
Write-Host "  nemesis8 installer" -ForegroundColor Cyan
Write-Host ""

# Detect architecture
$arch = if ([Environment]::Is64BitOperatingSystem) { "x86_64-pc-windows-msvc" } else {
    Write-Host "Error: 32-bit Windows not supported" -ForegroundColor Red; exit 1
}

# Get latest release
Write-Host "[*] Finding latest release..." -ForegroundColor Yellow
$release = Invoke-RestMethod -Uri "https://api.github.com/repos/DeepBlueDynamics/nemesis8/releases/latest" -Headers @{"User-Agent"="nemesis8-installer"}
$tag = $release.tag_name
$asset = $release.assets | Where-Object { $_.name -match $arch } | Select-Object -First 1

if (-not $asset) {
    Write-Host "Error: No Windows binary found in release $tag" -ForegroundColor Red
    exit 1
}

Write-Host "[OK] Found $tag" -ForegroundColor Green

# Download
$tmpZip = Join-Path $env:TEMP "nemesis8-$tag.zip"
Write-Host "[*] Downloading $($asset.name)..." -ForegroundColor Yellow
Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $tmpZip

# Extract
$tmpDir = Join-Path $env:TEMP "nemesis8-extract"
if (Test-Path $tmpDir) { Remove-Item -Recurse -Force $tmpDir }
Expand-Archive -Path $tmpZip -DestinationPath $tmpDir

# Install
$binDir = Join-Path $env:USERPROFILE ".local\bin"
if (-not (Test-Path $binDir)) {
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
}

$exe = Get-ChildItem -Path $tmpDir -Filter "nemisis8.exe" -Recurse | Select-Object -First 1
if (-not $exe) {
    Write-Host "Error: nemisis8.exe not found in archive" -ForegroundColor Red
    exit 1
}

Copy-Item $exe.FullName (Join-Path $binDir "nemesis8.exe") -Force
Copy-Item $exe.FullName (Join-Path $binDir "nemisis8.exe") -Force

# Add to PATH if needed
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not ($userPath -split ";" | Where-Object { $_ -ieq $binDir })) {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$binDir", "User")
    $env:Path = [System.Environment]::GetEnvironmentVariable("Path","Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path","User")
    Write-Host "[!] Added $binDir to PATH (restart terminal to take effect)" -ForegroundColor Yellow
}

# Cleanup
Remove-Item -Force $tmpZip
Remove-Item -Recurse -Force $tmpDir

# Verify
$version = & (Join-Path $binDir "nemesis8.exe") --version 2>&1
Write-Host ""
Write-Host "nemesis8 installed: $version" -ForegroundColor Green
Write-Host ""
Write-Host "Prerequisites: Docker Desktop (https://docs.docker.com/desktop/)" -ForegroundColor Gray
Write-Host ""
Write-Host "Get started:" -ForegroundColor Cyan
Write-Host "  nemesis8 interactive          # start a session"
Write-Host "  nemesis8 doctor               # check prerequisites"
Write-Host "  nemesis8 --help               # see all commands"
Write-Host ""
