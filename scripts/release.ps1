# Builds release binaries, the portable zip and the installer into .\dist.
#
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1
#
# Needs: Rust (stable, x86_64-pc-windows-msvc) and Inno Setup 6 (winget install JRSoftware.InnoSetup).

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "Hush $version"

$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null

# A running copy locks the exe.
$cli = Join-Path $root 'target\release\hush.exe'
if (Test-Path $cli) { & $cli quit 2>$null | Out-Null; Start-Sleep -Milliseconds 1500 }

cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

# The icon build.rs generated.
$ico = Get-ChildItem 'target\release\build\hush-*\out\hush.ico' | Sort-Object LastWriteTime -Descending | Select-Object -First 1
Copy-Item $ico.FullName (Join-Path $dist 'hush.ico') -Force

# PawnIO driver installer (redistributable unmodified), bundled into the setup.
$pawn = Join-Path $dist 'PawnIO_setup.exe'
if (-not (Test-Path $pawn)) {
    Invoke-WebRequest 'https://github.com/namazso/PawnIO.Setup/releases/latest/download/PawnIO_setup.exe' -OutFile $pawn
}

# Portable zip.
$stage = Join-Path $dist 'Hush'
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory $stage | Out-Null
Copy-Item 'target\release\hush-app.exe', 'target\release\hush.exe' $stage
Copy-Item 'LICENSE' (Join-Path $stage 'LICENSE.txt')
$zip = Join-Path $dist "Hush-$version-portable.zip"
Remove-Item $zip -ErrorAction SilentlyContinue
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
Remove-Item $stage -Recurse -Force

# Installer.
$iscc = @(
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 not found (winget install JRSoftware.InnoSetup)' }
& $iscc "/DAppVersion=$version" "/DIconFile=$dist\hush.ico" 'installer\hush.iss'
if ($LASTEXITCODE -ne 0) { throw 'ISCC failed' }

Get-ChildItem $dist -File | Where-Object { $_.Name -like 'Hush-*' } | ForEach-Object {
    '{0,-36} {1,8:N0} KB  {2}' -f $_.Name, ($_.Length / 1KB), (Get-FileHash $_.FullName -Algorithm SHA256).Hash
}
