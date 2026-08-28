param(
  [string]$Prefix = "$env:LOCALAPPDATA\into-markdown",
  [string]$CommandDirectory = "$env:LOCALAPPDATA\Microsoft\WindowsApps"
)
$ErrorActionPreference = "Stop"
$securityModule = Join-Path $PSHOME "Modules\Microsoft.PowerShell.Security\Microsoft.PowerShell.Security.psd1"
if (-not (Test-Path -LiteralPath $securityModule -PathType Leaf)) {
  throw "installEnvironmentUnsupported: Windows PowerShell security module is unavailable"
}
Import-Module -Name $securityModule -ErrorAction Stop

function Test-FullyQualifiedPath([string]$Path) {
  if ([String]::IsNullOrWhiteSpace($Path) -or -not [IO.Path]::IsPathRooted($Path)) { return $false }
  if ($Path -match '^[A-Za-z]:[^\\/]' -or ($Path.StartsWith('\') -and -not $Path.StartsWith('\\'))) {
    return $false
  }
  try { [void][IO.Path]::GetFullPath($Path) } catch { return $false }
  return $true
}

if (-not (Test-FullyQualifiedPath $Prefix) -or -not (Test-FullyQualifiedPath $CommandDirectory)) {
  throw "installPathUnsafe: installation paths must be absolute"
}
$Prefix = [IO.Path]::GetFullPath($Prefix)
$CommandDirectory = [IO.Path]::GetFullPath($CommandDirectory)
if (-not (Test-Path -LiteralPath $Prefix)) {
  if ((Test-Path -LiteralPath (Join-Path $CommandDirectory "into-md.exe")) -or (Test-Path -LiteralPath (Join-Path $CommandDirectory "into-md.prefix"))) {
    throw "installPathUnsafe: command authority exists without its installation prefix"
  }
  return
}
foreach ($path in @($Prefix, $CommandDirectory)) {
  $item = Get-Item -LiteralPath $path -Force
  if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
    throw "installPathUnsafe: installation path is a reparse point or non-directory"
  }
}
$acl = Get-Acl -LiteralPath $Prefix
$current = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$owner = $acl.Owner
try { $owner = ([Security.Principal.NTAccount]$owner).Translate([Security.Principal.SecurityIdentifier]).Value } catch {}
if (-not $acl.AreAccessRulesProtected -or $owner -ne $current) {
  throw "installPathUnsafe: installation prefix is not owned by the installer identity"
}
$mutation = [Security.AccessControl.FileSystemRights]::Write -bor [Security.AccessControl.FileSystemRights]::Modify -bor [Security.AccessControl.FileSystemRights]::FullControl -bor [Security.AccessControl.FileSystemRights]::ChangePermissions -bor [Security.AccessControl.FileSystemRights]::TakeOwnership -bor [Security.AccessControl.FileSystemRights]::Delete
foreach ($entry in $acl.Access) {
  $sid = $entry.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value
  if ($entry.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow -and ($entry.FileSystemRights -band $mutation) -and $sid -ne $current) {
    throw "installPathUnsafe: installation prefix grants mutation to another identity"
  }
}
$helper = Join-Path $PSScriptRoot "bin\into-md-installer.exe"
if (-not (Test-Path -LiteralPath $helper -PathType Leaf)) {
  $current = Join-Path $Prefix "current.txt"
  if (-not (Test-Path -LiteralPath $current -PathType Leaf)) { throw "installAuthorityInvalid: installed version is unavailable" }
  $identity = (Get-Content -LiteralPath $current -Raw).Trim()
  if ($identity -notmatch '^[0-9a-f]{64}$') { throw "installAuthorityInvalid: installed version authority is invalid" }
  $helper = Join-Path $Prefix "versions\$identity\bin\into-md-installer.exe"
}
& $helper uninstall $Prefix $CommandDirectory | Out-Null
if ($LASTEXITCODE -ne 0) { throw "uninstallFailed: native uninstall transaction failed" }
Write-Output "Into Markdown was removed successfully."
