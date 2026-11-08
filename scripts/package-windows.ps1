<#
.SYNOPSIS
  Compila (opcional) y empaqueta los ejecutables de CrossScreen para Windows x64
  en dist\crossscreen-<versión>-windows-x64.zip.

.EXAMPLE
  .\scripts\package-windows.ps1            # compila en release y empaqueta
  .\scripts\package-windows.ps1 -NoBuild   # solo empaqueta lo ya compilado
#>
param(
  [switch]$NoBuild,
  [string]$Version = ""
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not $Version) {
  $line = Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
  $Version = $line.Matches[0].Groups[1].Value
}

$bins = @("crossscreen", "crossscreen-viewer", "crossscreen-helper", "crossscreen-agent", "crossscreen-desktop")
if (-not $NoBuild) {
  Push-Location (Join-Path $root "apps\desktop")
  npm.cmd ci
  if ($LASTEXITCODE -ne 0) { Pop-Location; throw "npm ci falló" }
  npm.cmd run build
  if ($LASTEXITCODE -ne 0) { Pop-Location; throw "npm run build falló" }
  Pop-Location

  # Identificador de build: el agente lo anuncia y el panel lo compara para
  # no usar por error un agente de otra versión (ver cs_ipc::BUILD_ID).
  if ($env:GITHUB_SHA) {
    $env:CROSSSCREEN_BUILD_ID = "$($env:GITHUB_REF_NAME)-$($env:GITHUB_SHA.Substring(0, 7))"
  } else {
    $env:CROSSSCREEN_BUILD_ID = (git describe --tags --always --dirty 2>$null)
    if (-not $env:CROSSSCREEN_BUILD_ID) { $env:CROSSSCREEN_BUILD_ID = "local" }
  }
  "Build: $env:CROSSSCREEN_BUILD_ID"

  cargo build --release -p crossscreen-cli -p crossscreen-viewer -p crossscreen-helper -p crossscreen-agent
  if ($LASTEXITCODE -ne 0) { throw "cargo build falló" }
  cargo build --release -p crossscreen-desktop --features custom-protocol
  if ($LASTEXITCODE -ne 0) { throw "cargo build de crossscreen-desktop falló" }
}

$name = "crossscreen-$Version-windows-x64"
$out = Join-Path $root "dist\$name"
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force $out | Out-Null

foreach ($b in $bins) {
  $src = Join-Path $root "target\release\$b.exe"
  if (-not (Test-Path $src)) { throw "falta $src (compila primero)" }
  Copy-Item $src $out
}
Copy-Item "docs\10-prueba-dos-equipos.md" (Join-Path $out "PRUEBA-DOS-EQUIPOS.md")
Copy-Item "LICENSE-MIT", "LICENSE-APACHE" $out
Copy-Item "drivers\windows\vdd-manifest.toml" $out

# Suma de verificación de cada archivo para que el receptor pueda comprobar la descarga.
Get-ChildItem $out -File | ForEach-Object {
  "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name
} | Set-Content -Encoding utf8 (Join-Path $out "SHA256SUMS.txt")

$zip = Join-Path $root "dist\$name.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path "$out\*" -DestinationPath $zip
"Paquete: $zip"
(Get-FileHash $zip -Algorithm SHA256).Hash.ToLower() + "  $name.zip"
