param(
  [string]$Prefix = "$env:LOCALAPPDATA\into-markdown",
  [string]$CommandDirectory = "$env:LOCALAPPDATA\Microsoft\WindowsApps"
)
$ErrorActionPreference = "Stop"
if (-not [IO.Path]::IsPathFullyQualified($Prefix) -or -not [IO.Path]::IsPathFullyQualified($CommandDirectory)) { throw "installation paths must be absolute" }
$lock = Join-Path $Prefix ".install-lock"
New-Item -ItemType Directory -Force $Prefix | Out-Null
try { New-Item -ItemType Directory -ErrorAction Stop $lock | Out-Null } catch { throw "another install or uninstall is active" }
try {
  $shim = Join-Path $CommandDirectory "into-md.cmd"
  if (Test-Path -LiteralPath $shim) { Remove-Item -LiteralPath $shim -Force }
  foreach ($name in @("versions", "current.txt")) {
    $path = Join-Path $Prefix $name
    if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Recurse -Force }
  }
  Write-Output "into-md removed"
} finally {
  Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue
}
