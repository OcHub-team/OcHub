[CmdletBinding()]
param(
    [string]$OutDir = "dist",
    [string]$Target = "x86_64-pc-windows-msvc"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $RepoRoot

if ([IO.Path]::IsPathRooted($OutDir)) {
    $OutPath = [IO.Path]::GetFullPath($OutDir)
} else {
    $OutPath = [IO.Path]::GetFullPath((Join-Path $RepoRoot $OutDir))
}
New-Item -ItemType Directory -Force -Path $OutPath | Out-Null

$Metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) {
    throw "cargo metadata failed"
}
$Version = ($Metadata.packages | Where-Object name -eq "ochub-app").version

cargo build --release --locked --target $Target -p ochub-app -p ochcli
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed"
}

$BinaryDir = Join-Path $RepoRoot "target/$Target/release"
$BinaryPath = Join-Path $BinaryDir "ochub.exe"
& $BinaryPath --version
if ($LASTEXITCODE -ne 0) {
    throw "release binary smoke test failed"
}

$CliBinaryPath = Join-Path $BinaryDir "ochcli.exe"
$DaemonBinaryPath = Join-Path $BinaryDir "ochubd.exe"
& $CliBinaryPath version
if ($LASTEXITCODE -ne 0) {
    throw "CLI release binary smoke test failed"
}

$CertificateBase64 = $env:WINDOWS_CERTIFICATE_BASE64
if ([string]::IsNullOrWhiteSpace($CertificateBase64)) {
    cargo packager `
        --release `
        --packages ochub-app `
        --formats nsis `
        --out-dir $OutPath `
        --target $Target
    if ($LASTEXITCODE -ne 0) {
        throw "cargo packager failed"
    }
} else {
    if ([string]::IsNullOrWhiteSpace($env:WINDOWS_CERTIFICATE_PASSWORD)) {
        throw "WINDOWS_CERTIFICATE_PASSWORD is required when WINDOWS_CERTIFICATE_BASE64 is set"
    }

    $PfxPath = Join-Path ([IO.Path]::GetTempPath()) "ochub-signing-$PID.pfx"
    $ImportedByScript = $false
    $Thumbprint = $null
    try {
        $PfxBytes = [Convert]::FromBase64String(($CertificateBase64 -replace "\s", ""))
        [IO.File]::WriteAllBytes($PfxPath, $PfxBytes)
        $Flags = [Security.Cryptography.X509Certificates.X509KeyStorageFlags]::EphemeralKeySet
        $Pfx = [Security.Cryptography.X509Certificates.X509Certificate2]::new(
            $PfxPath,
            $env:WINDOWS_CERTIFICATE_PASSWORD,
            $Flags
        )
        $Thumbprint = $Pfx.Thumbprint
        $CertificatePath = "Cert:\CurrentUser\My\$Thumbprint"

        if (-not (Test-Path $CertificatePath)) {
            $SecurePassword = ConvertTo-SecureString `
                -String $env:WINDOWS_CERTIFICATE_PASSWORD `
                -AsPlainText `
                -Force
            Import-PfxCertificate `
                -FilePath $PfxPath `
                -CertStoreLocation "Cert:\CurrentUser\My" `
                -Password $SecurePassword | Out-Null
            $ImportedByScript = $true
        }

        $IconRoot = Join-Path $RepoRoot "crates/app/assets/app-icons"
        $Icons = @(
            (Join-Path $IconRoot "ochub.ico"),
            (Get-ChildItem -Path $IconRoot -Filter "ochub-*.png" | ForEach-Object FullName)
        )
        $Config = [ordered]@{
            productName = "OcHub"
            version = $Version
            identifier = "io.github.sleepstars.ochub"
            category = "DeveloperTool"
            description = "Native desktop manager for AI coding tools"
            authors = @("OcHub contributors")
            publisher = "OcHub contributors"
            binaries = @(@{ path = "ochub"; main = $true })
            binariesDir = $BinaryDir
            outDir = $OutPath
            targetTriple = $Target
            icons = $Icons
            resources = @(
                @{
                    src = (Join-Path $RepoRoot "crates/app/assets")
                    target = "assets"
                },
                @{
                    src = (Join-Path $RepoRoot "LICENSE")
                    target = "LICENSE"
                }
            )
            windows = @{
                digestAlgorithm = "sha256"
                certificateThumbprint = $Thumbprint
                tsp = $true
                timestampUrl = "http://timestamp.digicert.com"
            }
            nsis = @{
                installerIcon = (Join-Path $IconRoot "ochub.ico")
                installMode = "currentUser"
                languages = @("English", "SimpChinese", "Japanese")
            }
        }
        $ConfigJson = $Config | ConvertTo-Json -Depth 12 -Compress
        cargo packager --config $ConfigJson --formats nsis
        if ($LASTEXITCODE -ne 0) {
            throw "signed cargo packager run failed"
        }
    } finally {
        if ($ImportedByScript -and $Thumbprint) {
            Remove-Item -LiteralPath "Cert:\CurrentUser\My\$Thumbprint" -Force
        }
        if (Test-Path $PfxPath) {
            Remove-Item -LiteralPath $PfxPath -Force
        }
    }
}

$PortableRoot = Join-Path ([IO.Path]::GetTempPath()) "ochub-portable-$PID"
$PortableZip = Join-Path $OutPath "OcHub_${Version}_windows_x64_portable.zip"
try {
    New-Item -ItemType Directory -Path $PortableRoot | Out-Null
    Copy-Item -LiteralPath $BinaryPath -Destination (Join-Path $PortableRoot "OcHub.exe")
    Copy-Item `
        -LiteralPath (Join-Path $RepoRoot "crates/app/assets") `
        -Destination (Join-Path $PortableRoot "assets") `
        -Recurse
    if (Test-Path $PortableZip) {
        Remove-Item -LiteralPath $PortableZip -Force
    }
    Compress-Archive -Path (Join-Path $PortableRoot "*") -DestinationPath $PortableZip
} finally {
    if (Test-Path $PortableRoot) {
        Remove-Item -LiteralPath $PortableRoot -Recurse -Force
    }
}

$CliRoot = Join-Path ([IO.Path]::GetTempPath()) "ochcli-$PID"
$CliZip = Join-Path $OutPath "OcHub_${Version}_windows_x64_cli.zip"
try {
    New-Item -ItemType Directory -Path $CliRoot | Out-Null
    Copy-Item -LiteralPath $CliBinaryPath -Destination $CliRoot
    Copy-Item -LiteralPath $DaemonBinaryPath -Destination $CliRoot
    Copy-Item -LiteralPath (Join-Path $RepoRoot "LICENSE") -Destination $CliRoot
    Copy-Item `
        -LiteralPath (Join-Path $RepoRoot "docs/CLI-INSTALL.md") `
        -Destination (Join-Path $CliRoot "README.md")
    if (Test-Path $CliZip) {
        Remove-Item -LiteralPath $CliZip -Force
    }
    Compress-Archive -Path (Join-Path $CliRoot "*") -DestinationPath $CliZip
} finally {
    if (Test-Path $CliRoot) {
        Remove-Item -LiteralPath $CliRoot -Recurse -Force
    }
}
