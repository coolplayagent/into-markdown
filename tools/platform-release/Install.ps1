param(
  [string]$Prefix = "$env:LOCALAPPDATA\into-markdown",
  [string]$CommandDirectory = "$env:LOCALAPPDATA\Microsoft\WindowsApps"
)
$ErrorActionPreference = "Stop"

function Resolve-AbsoluteNormalized([string]$Path, [string]$Label) {
  if (-not [IO.Path]::IsPathFullyQualified($Path)) { throw "installPathUnsafe: $Label must be absolute" }
  if (-not [StringComparer]::Ordinal.Equals($Path, $Path.Normalize([Text.NormalizationForm]::FormC))) {
    throw "installPathUnsafe: $Label must use normalized Unicode"
  }
  $segments = $Path -split '[\\/]'
  if ($segments -contains '.' -or $segments -contains '..') {
    throw "installPathUnsafe: $Label must be normalized"
  }
  return [IO.Path]::GetFullPath($Path)
}

function Assert-NoReparseChain([string]$Path) {
  $candidate = [IO.Path]::GetFullPath($Path)
  while (-not (Test-Path -LiteralPath $candidate)) {
    $parent = [IO.Directory]::GetParent($candidate)
    if ($null -eq $parent) { throw "installPathUnsafe: no existing path ancestor" }
    $candidate = $parent.FullName
  }
  while ($candidate) {
    $item = Get-Item -LiteralPath $candidate -Force
    if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
      throw "installPathUnsafe: path chain contains a reparse point or non-directory"
    }
    $parent = [IO.Directory]::GetParent($candidate)
    if ($null -eq $parent) { break }
    $candidate = $parent.FullName
  }
}

function Set-OrAssertPrivateDirectory([string]$Path) {
  $created = -not (Test-Path -LiteralPath $Path)
  if ($created) {
    [IO.Directory]::CreateDirectory($Path) | Out-Null
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent().User
    # Start from an empty descriptor instead of editing the inherited one. Some
    # Windows volumes materialize parent ACEs as explicit entries on creation;
    # protecting that inherited descriptor would retain those mutation grants.
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetOwner($identity)
    $acl.SetAccessRuleProtection($true, $false)
    $rule = [Security.AccessControl.FileSystemAccessRule]::new(
      $identity,
      [Security.AccessControl.FileSystemRights]::FullControl,
      [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor [Security.AccessControl.InheritanceFlags]::ObjectInherit,
      [Security.AccessControl.PropagationFlags]::None,
      [Security.AccessControl.AccessControlType]::Allow
    )
    $acl.AddAccessRule($rule) | Out-Null
    Set-Acl -LiteralPath $Path -AclObject $acl
  }
  $acl = Get-Acl -LiteralPath $Path
  $current = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
  $owner = $acl.Owner
  try { $owner = ([Security.Principal.NTAccount]$owner).Translate([Security.Principal.SecurityIdentifier]).Value } catch {}
  if (-not $acl.AreAccessRulesProtected -or $owner -ne $current) {
    throw "installPathUnsafe: installation prefix must have a protected current-user DACL"
  }
  $mutation = [Security.AccessControl.FileSystemRights]::Write -bor [Security.AccessControl.FileSystemRights]::Modify -bor [Security.AccessControl.FileSystemRights]::FullControl -bor [Security.AccessControl.FileSystemRights]::ChangePermissions -bor [Security.AccessControl.FileSystemRights]::TakeOwnership -bor [Security.AccessControl.FileSystemRights]::Delete
  foreach ($entry in $acl.Access) {
    $sid = $entry.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value
    if ($entry.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow -and ($entry.FileSystemRights -band $mutation) -and $sid -ne $current) {
      throw "installPathUnsafe: $Path grants mutation to another identity ($sid)"
    }
  }
}

$Prefix = Resolve-AbsoluteNormalized $Prefix "installation prefix"
$CommandDirectory = Resolve-AbsoluteNormalized $CommandDirectory "command directory"
Assert-NoReparseChain $Prefix
Assert-NoReparseChain $CommandDirectory
Set-OrAssertPrivateDirectory $Prefix
if (-not (Test-Path -LiteralPath $CommandDirectory)) {
  Set-OrAssertPrivateDirectory $CommandDirectory
}
Assert-NoReparseChain $Prefix
Assert-NoReparseChain $CommandDirectory
$distribution = [IO.Path]::GetFullPath($PSScriptRoot)
if (Get-ChildItem -LiteralPath $distribution -Recurse -Force | Where-Object { $_.Attributes -band [IO.FileAttributes]::ReparsePoint }) {
  throw "installIntegrityFailed: distribution contains a reparse point"
}
& "$distribution\bin\archive-check.exe" $distribution
if ($LASTEXITCODE -ne 0) { throw "installIntegrityFailed: distribution integrity check failed" }
$manifestHash = (Get-FileHash -Algorithm SHA256 "$distribution\archive-manifest.json").Hash.ToLowerInvariant()
& "$distribution\bin\into-md-installer.exe" install $distribution $Prefix $CommandDirectory $manifestHash
if ($LASTEXITCODE -ne 0) { throw "installFailed: native installation transaction failed" }
