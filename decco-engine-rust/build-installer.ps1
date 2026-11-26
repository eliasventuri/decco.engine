param(
    [switch]$NoBump,
    [string]$SetVersion
)

$ErrorActionPreference = "Stop"

$Root = $PSScriptRoot
$CargoTomlPath = Join-Path $Root "Cargo.toml"
$NsiPath = Join-Path $Root "installer\installer.nsi"
$CargoToml = Get-Content $CargoTomlPath -Raw

if ($CargoToml -match '(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"') {
    $Major = [int]$Matches[1]
    $Minor = [int]$Matches[2]
    $Patch = [int]$Matches[3]
    $CurrentVersion = "$Major.$Minor.$Patch"
} else {
    throw "Could not parse [workspace.package] version in $CargoTomlPath"
}

if ($SetVersion) {
    $Version = $SetVersion
} elseif ($NoBump) {
    $Version = $CurrentVersion
} else {
    $Version = "$Major.$Minor.$($Patch + 1)"
}

if ($Version -ne $CurrentVersion) {
    Write-Host "[Decco] Bumping Rust engine version: $CurrentVersion -> $Version"
    $UpdatedCargo = [regex]::Replace($CargoToml, '(?m)^version\s*=\s*"[^"]+"', "version = `"$Version`"", 1)
    Set-Content -Path $CargoTomlPath -Value $UpdatedCargo -NoNewline

    if (Test-Path $NsiPath) {
        $NsiRaw = Get-Content $NsiPath -Raw
        $UpdatedNsi = [regex]::Replace($NsiRaw, '(?m)^(\s*!define\s+APP_VERSION\s+)"[^"]+"', "`$1`"$Version`"", 1)
        Set-Content -Path $NsiPath -Value $UpdatedNsi -NoNewline
    }
}

$ReleaseDir = Join-Path $Root "target\release"
$ExePath = Join-Path $ReleaseDir "decco-engine.exe"

Write-Host "[Decco] Building decco-desktop release v$Version..."
$env:PATH = "C:\Program Files\NASM;$env:PATH"
Push-Location $Root
try {
    cargo build --release -p decco-desktop
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
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
Write-Host "[Decco] Installer ready (v$Version):"
Write-Host "  - $SetupOut"
Write-Host "  - $LatestOut"
Write-Host "  - Silent test: `"$SetupOut`" /S"
