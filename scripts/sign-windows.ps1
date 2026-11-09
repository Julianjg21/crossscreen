<#
.SYNOPSIS
  Firma ejecutables de Windows con signtool (Authenticode SHA-256 + sello de
  tiempo) si hay certificado; si no, no hace nada y lo dice.

.DESCRIPTION
  Certificado en variables de entorno (secretos de GitHub Actions):
    WINDOWS_CERT_PFX_BASE64  archivo .pfx en Base64
    WINDOWS_CERT_PASSWORD    contraseña del .pfx
  Opcional: WINDOWS_TIMESTAMP_URL (por defecto http://timestamp.digicert.com).

  Tauri lo usa también como bundle.windows.signCommand (un archivo por
  llamada) para el panel y el instalador.

.EXAMPLE
  .\scripts\sign-windows.ps1 target\release\crossscreen-agent.exe target\release\crossscreen.exe
#>
param([Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)][string[]]$Files)
$ErrorActionPreference = "Stop"

if (-not $env:WINDOWS_CERT_PFX_BASE64) {
  "Firma: sin certificado (WINDOWS_CERT_PFX_BASE64 vacío); se omite: $($Files -join ', ')"
  exit 0
}

$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" -ErrorAction SilentlyContinue |
  Sort-Object FullName | Select-Object -Last 1
if (-not $signtool) { throw "no se encontró signtool.exe (Windows SDK)" }

$ts = if ($env:WINDOWS_TIMESTAMP_URL) { $env:WINDOWS_TIMESTAMP_URL } else { "http://timestamp.digicert.com" }
$pfx = Join-Path ([IO.Path]::GetTempPath()) ("cs-sign-" + [Guid]::NewGuid().ToString("N") + ".pfx")
try {
  [IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($env:WINDOWS_CERT_PFX_BASE64))
  & $signtool.FullName sign /fd SHA256 /tr $ts /td SHA256 /f $pfx /p $env:WINDOWS_CERT_PASSWORD /d "CrossScreen" @Files
  if ($LASTEXITCODE -ne 0) { throw "signtool sign falló ($LASTEXITCODE)" }
  & $signtool.FullName verify /pa @Files
  if ($LASTEXITCODE -ne 0) { throw "signtool verify falló ($LASTEXITCODE)" }
  "Firmado: $($Files -join ', ')"
} finally {
  if (Test-Path $pfx) { Remove-Item -Force $pfx }
}
