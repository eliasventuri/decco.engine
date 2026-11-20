$ErrorActionPreference = "Stop"

$Root = $PSScriptRoot
$CargoToml = Get-Content (Join-Path $Root "Cargo.toml") -Raw
if ($CargoToml -match '(?m)^version\s*=\s*"([^"]+)"') {
    $Version = $Matches[1]
} else {
    $Version = "1.1.82"
}

$ReleaseDir = Join-Path $Root "target\release"
$ExePath = Join-Path $ReleaseDir "decco-engine.exe"

if (-not (Test-Path $ExePath)) {
    Write-Host "[Decco] Building decco-desktop release..."
    $env:PATH = "C:\Program Files\NASM;$env:PATH"
    Push-Location $Root
    try {
        cargo build --release -p decco-desktop
    } finally {
        Pop-Location
    }
}

$Makensis = @(
    "makensis.exe",
    "$env:LOCALAPPDATA\electron-builder\Cache\nsis\nsis-3.0.4.1\makensis.exe",
    "C:\Program Files (x86)\NSIS\makensis.exe"
) | ForEach-Object {
    $cmd = Get-Command $_ -ErrorAction SilentlyContinue
    if ($cmd) { $cmd.Source }
    elseif (Test-Path $_) { $_ }
} | Select-Object -First 1

if (-not $Makensis) {
    throw "makensis.exe not found. Install NSIS or ensure electron-builder NSIS cache exists."
}

$SetupOut = Join-Path $ReleaseDir "decco-engine-Setup-$Version.exe"
$LatestOut = Join-Path $ReleaseDir "decco-engine-latest.exe"
$NsiPath = Join-Path $Root "installer\installer.nsi"

Write-Host "[Decco] Packaging silent-capable NSIS installer v$Version using $Makensis..."
& $Makensis `
    "/DAPP_VERSION=$Version" `
    "/DEXE_PATH=$ExePath" `
    "/DOUT_FILE=$SetupOut" `
    $NsiPath

if ($LASTEXITCODE -ne 0) {
    throw "makensis failed with exit code $LASTEXITCODE"
}

Copy-Item -Path $SetupOut -Destination $LatestOut -Force
Write-Host "[Decco] Installer ready:"
Write-Host "  - $SetupOut"
Write-Host "  - $LatestOut"
Write-Host "  - Silent test: `"$SetupOut`" /S"
