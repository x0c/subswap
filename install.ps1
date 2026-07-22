[CmdletBinding()]
param(
    [string]$Version,
    [switch]$SkipPathUpdate
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Write-Step {
    param([string]$Message)
    Write-Host "subswap: $Message"
}

function Get-RequestHeaders {
    $headers = @{
        Accept = "application/vnd.github+json"
        "User-Agent" = "subswap-installer"
        "X-GitHub-Api-Version" = "2022-11-28"
    }
    $token = if ($env:GH_TOKEN) { $env:GH_TOKEN } elseif ($env:GITHUB_TOKEN) { $env:GITHUB_TOKEN } else { $null }
    if ($token) {
        $headers.Authorization = "Bearer $token"
    }
    return $headers
}

function Add-ToUserPath {
    param([string]$Directory)

    $trimCharacters = [char[]]"\/"
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $entries = @()
    if ($userPath) {
        $entries = @($userPath -split ";" | Where-Object { $_ })
    }
    $normalizedDirectory = $Directory.TrimEnd($trimCharacters)
    $alreadyPresent = $entries | Where-Object {
        $_.Trim().TrimEnd($trimCharacters).Equals($normalizedDirectory, [StringComparison]::OrdinalIgnoreCase)
    }
    if (-not $alreadyPresent) {
        $newPath = (@($entries) + $Directory) -join ";"
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        Write-Step "Added $Directory to your user PATH. Open a new terminal to use it."
    }
    $processEntries = @($env:Path -split ";" | Where-Object { $_ })
    $processPresent = $processEntries | Where-Object {
        $_.Trim().TrimEnd($trimCharacters).Equals($normalizedDirectory, [StringComparison]::OrdinalIgnoreCase)
    }
    if (-not $processPresent) {
        $env:Path = (@($processEntries) + $Directory) -join ";"
    }
}

try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

    if (-not $Version -and $env:SUBSWAP_VERSION) {
        $Version = $env:SUBSWAP_VERSION
    }
    if ($Version -and $Version -notmatch '^v?\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$') {
        throw "Invalid version '$Version'. Expected a value such as 1.4.0 or v1.4.0."
    }

    $repository = if ($env:SUBSWAP_REPOSITORY) { $env:SUBSWAP_REPOSITORY } else { "x0c/subswap" }
    if ($repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
        throw "Invalid repository '$repository'. Expected owner/name."
    }
    $headers = Get-RequestHeaders
    $apiBase = "https://api.github.com/repos/$repository/releases"

    if ($Version) {
        $tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
        $encodedTag = [Uri]::EscapeDataString($tag)
        Write-Step "Resolving release $tag..."
        $release = Invoke-RestMethod -Uri "$apiBase/tags/$encodedTag" -Headers $headers
    }
    else {
        Write-Step "Resolving the latest release..."
        $release = Invoke-RestMethod -Uri "$apiBase/latest" -Headers $headers
        $tag = [string]$release.tag_name
    }

    if ($release.draft) {
        throw "Release $tag is still a draft and cannot be installed."
    }

    $target = "x86_64-pc-windows-msvc"
    $archiveName = "subswap-$tag-$target.zip"
    $checksumName = "$archiveName.sha256"
    $archiveAsset = @($release.assets | Where-Object { $_.name -eq $archiveName }) | Select-Object -First 1
    $checksumAsset = @($release.assets | Where-Object { $_.name -eq $checksumName }) | Select-Object -First 1
    if (-not $archiveAsset -or -not $checksumAsset) {
        throw "Release $tag does not contain the required Windows files."
    }

    $installDirectory = if ($env:SUBSWAP_INSTALL_DIR) {
        [IO.Path]::GetFullPath($env:SUBSWAP_INSTALL_DIR)
    }
    else {
        if (-not $env:LOCALAPPDATA) {
            throw "LOCALAPPDATA is unavailable. Set SUBSWAP_INSTALL_DIR to choose an installation directory."
        }
        Join-Path $env:LOCALAPPDATA "Programs\subswap\bin"
    }

    $temporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) ("subswap-install-" + [Guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null
    try {
        $archivePath = Join-Path $temporaryDirectory $archiveName
        $checksumPath = Join-Path $temporaryDirectory $checksumName
        Write-Step "Downloading $tag for Windows..."
        Invoke-WebRequest -Uri $archiveAsset.browser_download_url -Headers $headers -OutFile $archivePath -UseBasicParsing
        Invoke-WebRequest -Uri $checksumAsset.browser_download_url -Headers $headers -OutFile $checksumPath -UseBasicParsing

        $checksumText = Get-Content -LiteralPath $checksumPath -Raw
        $checksumMatch = [Regex]::Match($checksumText, '(?i)\b[0-9a-f]{64}\b')
        if (-not $checksumMatch.Success) {
            throw "The release checksum file is invalid."
        }
        $expectedHash = $checksumMatch.Value.ToLowerInvariant()
        $actualHash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualHash -ne $expectedHash) {
            throw "SHA256 verification failed. The downloaded file was not installed."
        }
        Write-Step "SHA256 verification passed."

        $extractDirectory = Join-Path $temporaryDirectory "extracted"
        Expand-Archive -LiteralPath $archivePath -DestinationPath $extractDirectory
        $executables = @(Get-ChildItem -LiteralPath $extractDirectory -Filter "subswap.exe" -File -Recurse)
        if ($executables.Count -ne 1) {
            throw "The release archive does not contain exactly one subswap.exe."
        }

        New-Item -ItemType Directory -Force -Path $installDirectory | Out-Null
        $destination = Join-Path $installDirectory "subswap.exe"
        $staging = Join-Path $installDirectory ("subswap.exe.new-" + [Guid]::NewGuid().ToString("N"))
        try {
            Copy-Item -LiteralPath $executables[0].FullName -Destination $staging
            Move-Item -LiteralPath $staging -Destination $destination -Force
        }
        finally {
            if (Test-Path -LiteralPath $staging) {
                Remove-Item -LiteralPath $staging -Force
            }
        }

        $skipPath = $SkipPathUpdate -or $env:SUBSWAP_SKIP_PATH_UPDATE -eq "1"
        if (-not $skipPath) {
            Add-ToUserPath -Directory $installDirectory
        }
        Write-Step "Installed $tag to $destination"
        Write-Step "Run 'subswap --version' to verify the installation."
    }
    finally {
        if ($temporaryDirectory -and (Test-Path -LiteralPath $temporaryDirectory)) {
            Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force
        }
    }
}
catch {
    Write-Error "subswap installation failed: $($_.Exception.Message)"
    exit 1
}
