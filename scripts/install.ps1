# install.ps1 — install prebuilt mdrv-oc on Windows in one command.
# Downloads the right archive from GitHub Releases, verifies it against the
# release's SHA256SUMS.txt, and installs mdrv-oc.exe into
# %LOCALAPPDATA%\Programs\mdrv-oc, then adds that directory to the user PATH.
#
# Usage (PowerShell):
#   irm https://github.com/mdrv/mdrv-oc/releases/latest/download/install.ps1 | iex
#
# Piped runs use the defaults below; pass parameters via environment instead:
#   $env:MDRV_OC_VERSION = "0.2.3"; irm ... | iex
# Or as a downloaded file:
#   ./install.ps1 -Version 0.2.3 -Destination D:\tools\mdrv-oc -NoModifyPath

<#
.SYNOPSIS
Installer for mdrv-oc on Windows.

.DESCRIPTION
Detects the architecture, downloads the matching release archive from
GitHub, verifies its SHA-256 against SHA256SUMS.txt, installs mdrv-oc.exe
and offers to add the install directory to the user PATH.

.PARAMETER Version
Install a specific release (e.g. "0.2.3" or "v0.2.3"). Default: latest.

.PARAMETER Destination
Install directory. Default: $env:LOCALAPPDATA\Programs\mdrv-oc
(also configurable via $env:MDRV_OC_INSTALL_DIR).

.PARAMETER NoModifyPath
Do not add the install directory to the user PATH.

.PARAMETER Help
Print help.
#>
param (
	[Parameter(HelpMessage = 'Release to install, e.g. 0.2.3 (default: latest)')]
	[string]$Version,
	[Parameter(HelpMessage = 'Install directory')]
	[string]$Destination,
	[Parameter(HelpMessage = 'Do not add the install directory to PATH')]
	[switch]$NoModifyPath,
	[Parameter(HelpMessage = 'Print Help')]
	[switch]$Help
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # hides Invoke-WebRequest's progress bar (much faster on PS 5.1)
$app_name = 'mdrv-oc'
$repo_url = 'https://github.com/mdrv/mdrv-oc'
$api_url = 'https://api.github.com/repos/mdrv/mdrv-oc'

if ($Help) {
	Get-Help $PSCommandPath -Detailed
	Exit
}

# ---- pre-flight (same checks cargo-dist installers make) -------------------
if ($PSVersionTable.PSVersion.Major -lt 5) {
	throw @"
PowerShell 5 or later is required to install $app_name.
Upgrade PowerShell: https://docs.microsoft.com/en-us/powershell/scripting/setup/installing-windows-powershell
"@
}

$allowedExecutionPolicy = @('Unrestricted', 'RemoteSigned', 'Bypass')
if ((Get-ExecutionPolicy).ToString() -notin $allowedExecutionPolicy) {
	throw @"
PowerShell requires an execution policy in [$($allowedExecutionPolicy -join ', ')] to run this installer. For example:

    Set-ExecutionPolicy RemoteSigned -scope CurrentUser
"@
}

# GitHub requires TLS 1.2
if ([System.Enum]::GetNames([System.Net.SecurityProtocolType]) -notcontains 'Tls12') {
	[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
}

# ---- options ----------------------------------------------------------------
if (-not $Version -and $env:MDRV_OC_VERSION) { $Version = $env:MDRV_OC_VERSION }
if (-not $Destination -and $env:MDRV_OC_INSTALL_DIR) { $Destination = $env:MDRV_OC_INSTALL_DIR }
if (-not $Destination) { $Destination = Join-Path $env:LOCALAPPDATA 'Programs\mdrv-oc' }
if ($env:MDRV_OC_NO_MODIFY_PATH) { $NoModifyPath = $true }

function Get-TargetTriple {
	# $env:PROCESSOR_ARCHITECTURE: AMD64 on x64 (incl. x64 emulation), ARM64 on
	# native ARM64, X86/x86 on 32-bit. 32-bit Windows is not a supported target.
	switch ($env:PROCESSOR_ARCHITECTURE) {
		'AMD64' { return 'x86_64-pc-windows-msvc' }
		'ARM64' { return 'aarch64-pc-windows-msvc' }
		default { throw "ERROR: unsupported architecture '$($env:PROCESSOR_ARCHITECTURE)' (supported: x64, ARM64)" }
	}
}

# ---- resolve the release tag --------------------------------------------------
if ($Version) {
	$tag = "v$($Version.TrimStart('v'))"
} else {
	Write-Information "==> resolving latest $app_name release"
	$tag = (Invoke-RestMethod -Uri "$api_url/releases/latest").tag_name
}
$target = Get-TargetTriple
$artifact = "$app_name-$tag-$target.zip"
$base_url = "$repo_url/releases/download/$tag"

Write-Information "==> downloading $artifact"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
	$zip_path = Join-Path $tmp $artifact
	Invoke-WebRequest -Uri "$base_url/$artifact" -OutFile $zip_path -UseBasicParsing

	# ---- verify against SHA256SUMS.txt --------------------------------------
	$sums_path = Join-Path $tmp 'SHA256SUMS.txt'
	try {
		Invoke-WebRequest -Uri "$base_url/SHA256SUMS.txt" -OutFile $sums_path -UseBasicParsing
	} catch {
		Write-Information "==> no SHA256SUMS.txt published for this release — skipping verification"
		$sums_path = $null
	}
	if ($sums_path) {
		$line = (Get-Content $sums_path) | Where-Object { $_ -like "*$artifact" } | Select-Object -First 1
		if ($line) {
			$expected = ($line -split '\s+') | Select-Object -First 1
			$actual = (Get-FileHash -Path $zip_path -Algorithm SHA256).Hash.ToLower()
			if ($actual -ne $expected) {
				throw "ERROR: checksum mismatch — the download is corrupted or was tampered with"
			}
			Write-Information "==> verifying sha256 ($expected)"
		} else {
			Write-Information "==> no checksum for $artifact in SHA256SUMS.txt — skipping verification"
		}
	}

	# ---- unpack & install ------------------------------------------------------
	Write-Information "==> extracting"
	Expand-Archive -Path $zip_path -DestinationPath $tmp -Force
	$exe = Join-Path $tmp "$app_name\$app_name.exe"
	if (-not (Test-Path $exe)) {
		throw "ERROR: unexpected archive layout ($app_name\$app_name.exe not found)"
	}

	$old_version = $null
	$dest_exe = Join-Path $Destination "$app_name.exe"
	if (Test-Path $dest_exe) {
		try { $old_version = (& $dest_exe --version) -join '' } catch { }
	}

	Write-Information "==> installing into $Destination"
	New-Item -ItemType Directory -Force -Path $Destination | Out-Null
	Copy-Item $exe $dest_exe -Force

	$new_version = (& $dest_exe --version) -join ''
	Write-Information ''
	Write-Information "==> $new_version installed ($dest_exe)"
	if ($old_version -and $old_version -ne $new_version) {
		Write-Information "==> upgraded from $old_version"
	}
} finally {
	Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

# ---- PATH ----------------------------------------------------------------------
# Add the directory to the user-level PATH via the registry.
# Returns true if the registry was modified, false if it was already on PATH.
# Adapted from j178/prek's installer (MIT), which adapted
# https://stackoverflow.com/questions/69236623
function Add-Path($LiteralPath) {
	$registry_path = 'registry::HKEY_CURRENT_USER\Environment'
	# .GetValue() with DoNotExpandEnvironmentNames keeps the *unexpanded* value.
	$current = (Get-Item -LiteralPath $registry_path).GetValue('Path', '', 'DoNotExpandEnvironmentNames') -split ';' -ne ''
	if ($LiteralPath -in $current) {
		return $false
	}
	# Prepend; the ',' makes $LiteralPath the first element of a new array.
	$new_path = (, $LiteralPath + $current) -join ';'
	# ExpandString keeps %VAR% entries expandable for everything else on PATH.
	Set-ItemProperty -Type ExpandString -LiteralPath $registry_path Path $new_path
	# Broadcast WM_SETTINGCHANGE so running shells pick the change up.
	$dummy = 'mdrv-oc-' + [guid]::NewGuid().ToString()
	[Environment]::SetEnvironmentVariable($dummy, 'mdrv-oc-dummy', 'User')
	[Environment]::SetEnvironmentVariable($dummy, $null, 'User')
	return $true
}

if (-not $NoModifyPath) {
	if (Add-Path $Destination) {
		Write-Information ''
		Write-Information "PATH updated (user) — open a new terminal, then run: mdrv-oc --help"
	} else {
		Write-Information "    $Destination is already on PATH — run: mdrv-oc --help"
	}
} else {
	Write-Information ''
	Write-Information "    Add $Destination to your PATH, then run: mdrv-oc --help"
}
Write-Information '    Shell completions (carapace):  mdrv-oc completion'
Write-Information '    Upgrade:   run this script again'
Exit 0
