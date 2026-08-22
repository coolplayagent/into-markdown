param(
  [string]$Prefix = "$env:LOCALAPPDATA\into-markdown",
  [string]$CommandDirectory = "$env:LOCALAPPDATA\Microsoft\WindowsApps"
)
$ErrorActionPreference = "Stop"
if (-not [IO.Path]::IsPathFullyQualified($Prefix) -or -not [IO.Path]::IsPathFullyQualified($CommandDirectory)) { throw "installation paths must be absolute" }
$distribution = [IO.Path]::GetFullPath($PSScriptRoot)
if (Get-ChildItem -LiteralPath $distribution -Recurse -Force | Where-Object { $_.Attributes -band [IO.FileAttributes]::ReparsePoint }) { throw "distribution contains a reparse point" }
& "$distribution\bin\archive-check.exe" $distribution
if ($LASTEXITCODE -ne 0) { throw "distribution integrity check failed" }
$manifestHash = (Get-FileHash -Algorithm SHA256 "$distribution\archive-manifest.json").Hash.ToLowerInvariant()
$versions = Join-Path $Prefix "versions"
$destination = Join-Path $versions $manifestHash
$lock = Join-Path $Prefix ".install-lock"
New-Item -ItemType Directory -Force $Prefix, $versions, $CommandDirectory | Out-Null
try { New-Item -ItemType Directory -ErrorAction Stop $lock | Out-Null } catch { throw "another install or uninstall is active" }
try {
  if (Test-Path -LiteralPath $destination) {
    & "$destination\bin\archive-check.exe" $destination
    if ($LASTEXITCODE -ne 0) { throw "existing installed copy failed integrity check" }
  } else {
    $temporary = Join-Path $versions ".install-$manifestHash-$PID"
    New-Item -ItemType Directory $temporary | Out-Null
    Get-ChildItem -LiteralPath $distribution -Force | Copy-Item -Destination $temporary -Recurse -Force
    & "$temporary\bin\archive-check.exe" $temporary
    if ($LASTEXITCODE -ne 0) { throw "installed copy failed integrity check" }
    Move-Item -LiteralPath $temporary -Destination $destination
  }
  $current = Join-Path $Prefix "current.txt"
  [IO.File]::WriteAllText("$current.new", $manifestHash + "`n", [Text.UTF8Encoding]::new($false))
  Move-Item -Force "$current.new" $current
  $shim = Join-Path $CommandDirectory "into-md.cmd"
  $command = "@echo off`r`n`"$destination\bin\into-md.exe`" %*`r`n"
  [IO.File]::WriteAllText("$shim.new", $command, [Text.ASCIIEncoding]::new())
  Move-Item -Force "$shim.new" $shim
  Write-Output $destination
} finally {
  Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue
}
