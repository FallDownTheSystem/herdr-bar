# Put the herdr-bar binary at bin\herdr-bar.exe for this checkout.
#
# Herdr runs this as the plugin's Windows [[build]] step on `herdr plugin
# install` and `herdr plugin update` (not on `plugin link`: run it yourself for
# a local checkout). It downloads the prebuilt binary of the manifest's version
# from the GitHub release, checks it against the release's SHA256SUMS, and
# falls back to `cargo build --release` when there is no binary for this
# machine or the download or the check fails.
#
#   HERDR_BAR_BUILD=source     skip the download and build with cargo
#   HERDR_BAR_DOWNLOAD_BASE=   release download URL to use instead of GitHub's
#
# The manifest's shared commands name `bin/herdr-bar`, so bin\herdr-bar (no
# extension) is installed beside bin\herdr-bar.exe: launchers that append .exe
# find the one, launchers that take the path as written find the other.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

$Root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $Root
$BinDir = Join-Path $Root 'bin'
$Target = Join-Path $BinDir 'herdr-bar.exe'
$Alias = Join-Path $BinDir 'herdr-bar'
$RepoUrl = 'https://github.com/jeffarese/herdr-bar'

function Say([string]$Message) { [Console]::Error.WriteLine("herdr-bar: $Message") }

$Version = $null
foreach ($Line in Get-Content -LiteralPath (Join-Path $Root 'herdr-plugin.toml')) {
    if ($Line -match '^version\s*=\s*"([^"]*)"') { $Version = $Matches[1]; break }
}
if (-not $Version) {
    Say 'cannot read the version from herdr-plugin.toml'
    exit 1
}

function Get-AssetName {
    $Arch = $null
    try {
        $Arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    } catch {
        # Windows PowerShell on an old .NET: a 32-bit shell on 64-bit Windows
        # reports the native architecture in PROCESSOR_ARCHITEW6432.
        $Arch = $env:PROCESSOR_ARCHITEW6432
        if (-not $Arch) { $Arch = $env:PROCESSOR_ARCHITECTURE }
    }
    switch -Regex ($Arch) {
        '^(X64|AMD64)$' { return 'herdr-bar-windows-x86_64.exe' }
        '^ARM64$' { return 'herdr-bar-windows-aarch64.exe' }
        default { return $null }
    }
}

# Replace in one rename, so a running watcher never sees half a binary.
function Install-Binary([string]$Source) {
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    foreach ($Destination in @($Target, $Alias)) {
        $Temp = "$Destination.tmp.$PID"
        Copy-Item -LiteralPath $Source -Destination $Temp -Force
        Move-Item -LiteralPath $Temp -Destination $Destination -Force
    }
}

function Invoke-Download {
    $Asset = Get-AssetName
    if (-not $Asset) {
        Say "no prebuilt binary for this architecture"
        return $false
    }
    $Base = $env:HERDR_BAR_DOWNLOAD_BASE
    if (-not $Base) { $Base = "$RepoUrl/releases/download/v$Version" }
    $Temp = Join-Path ([System.IO.Path]::GetTempPath()) ("herdr-bar-" + [System.Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $Temp | Out-Null
    try {
        Say "downloading $Asset $Version"
        try {
            # Windows PowerShell 5.1 still defaults to TLS 1.0 on some systems.
            [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        } catch {}
        $ProgressPreference = 'SilentlyContinue'
        $File = Join-Path $Temp $Asset
        $Sums = Join-Path $Temp 'SHA256SUMS'
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Asset" -OutFile $File
            Invoke-WebRequest -UseBasicParsing -Uri "$Base/SHA256SUMS" -OutFile $Sums
        } catch {
            Say "download failed: $($_.Exception.Message)"
            return $false
        }
        $Expected = $null
        foreach ($Line in Get-Content -LiteralPath $Sums) {
            $Fields = $Line.Trim() -split '\s+', 2
            if ($Fields.Count -eq 2 -and ($Fields[1] -eq $Asset -or $Fields[1] -eq "*$Asset")) {
                $Expected = $Fields[0].ToLowerInvariant()
                break
            }
        }
        if (-not $Expected) {
            Say "SHA256SUMS has no entry for $Asset"
            return $false
        }
        $Actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $File).Hash.ToLowerInvariant()
        if ($Expected -ne $Actual) {
            Say "checksum mismatch for $Asset (expected $Expected, got $Actual)"
            return $false
        }
        & $File --version *> $null
        if ($LASTEXITCODE -ne 0) {
            Say "the downloaded $Asset does not run on this machine"
            return $false
        }
        Install-Binary $File
        return $true
    } catch {
        Say "download failed: $($_.Exception.Message)"
        return $false
    } finally {
        Remove-Item -LiteralPath $Temp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Invoke-SourceBuild {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Say 'cargo is not installed'
        return $false
    }
    Say 'building from source with cargo'
    $CargoArgs = @('build', '--release')
    if (Test-Path -LiteralPath (Join-Path $Root 'Cargo.lock')) { $CargoArgs += '--locked' }
    try {
        & cargo @CargoArgs | Out-Host
        if ($LASTEXITCODE -ne 0) { return $false }
        Install-Binary (Join-Path $Root 'target\release\herdr-bar.exe')
        return $true
    } catch {
        Say "cargo build failed: $($_.Exception.Message)"
        return $false
    }
}

if ($env:HERDR_BAR_BUILD -ne 'source' -and (Invoke-Download)) {
    Say "installed $Target"
    exit 0
}
if (Invoke-SourceBuild) {
    Say "installed $Target"
    exit 0
}
Say "could not install herdr-bar ${Version}: no prebuilt binary could be downloaded"
Say 'and building from source failed. Install Rust (https://rustup.rs) and run'
Say '  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\install.ps1'
Say "in $Root, or reinstall the plugin."
exit 1
