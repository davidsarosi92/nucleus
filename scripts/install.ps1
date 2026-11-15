# Installs nucleus on Windows (PLAN.md §M14.17 E1):
#
#   irm https://raw.githubusercontent.com/davidsarosi92/nucleus/main/scripts/install.ps1 | iex
#
# The release's Windows archive, its SHA-256 checked against the release's
# SHA256SUMS, its binaries (nucleus.exe, ncsql.exe) put in
# $env:NUCLEUS_INSTALL\bin (default %LOCALAPPDATA%\nucleus\bin), which is
# added to the user's PATH.
#
#   $env:NUCLEUS_VERSION        a version (0.1.0) instead of the latest release
#   $env:NUCLEUS_INSTALL        where to install
#   $env:NUCLEUS_DOWNLOAD_URL   where the archives and SHA256SUMS are (a
#                               mirror, or a directory of scripts/package.sh's)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'davidsarosi92/nucleus'
$install = if ($env:NUCLEUS_INSTALL) { $env:NUCLEUS_INSTALL } else { Join-Path $env:LOCALAPPDATA 'nucleus' }

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { 'x86_64' }
    # Windows on Arm runs the x86_64 build emulated.
    'ARM64' { 'x86_64' }
    default { throw "install.ps1: no archive for the $($env:PROCESSOR_ARCHITECTURE) architecture" }
}
$suffix = "-windows-$arch.zip"

if ($env:NUCLEUS_DOWNLOAD_URL) {
    $base = $env:NUCLEUS_DOWNLOAD_URL.TrimEnd('/', '\')
} elseif ($env:NUCLEUS_VERSION) {
    $base = "https://github.com/$repo/releases/download/v$($env:NUCLEUS_VERSION.TrimStart('v'))"
} else {
    $base = "https://github.com/$repo/releases/latest/download"
}

function Get-File($from, $to) {
    if ($from -match '^[a-z]+://') {
        if ($from.StartsWith('file://')) {
            Copy-Item ([Uri]$from).LocalPath $to
        } else {
            Invoke-WebRequest -UseBasicParsing -Uri $from -OutFile $to
        }
    } else {
        Copy-Item $from $to
    }
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("nucleus-install-" + [Guid]::NewGuid())
New-Item -ItemType Directory $tmp | Out-Null
try {
    Get-File "$base/SHA256SUMS" "$tmp\SHA256SUMS"
    $line = Get-Content "$tmp\SHA256SUMS" |
        Where-Object { $_ -match "^([0-9a-f]{64}) [ *](nucleus-[0-9][^ ]*$([regex]::Escape($suffix)))$" } |
        Select-Object -First 1
    if (-not $line) { throw "install.ps1: the release has no archive *$suffix" }
    # "  " or " *" (binary mode, as Windows' sha256sum writes).
    $null = $line -match "^([0-9a-f]{64}) [ *](\S+)$"
    $sum, $file = $Matches[1], $Matches[2]
    $version = $file.Substring('nucleus-'.Length, $file.Length - 'nucleus-'.Length - $suffix.Length)

    Write-Host "downloading nucleus $version ($file)"
    Get-File "$base/$file" "$tmp\$file"
    $actual = (Get-FileHash -Algorithm SHA256 "$tmp\$file").Hash.ToLowerInvariant()
    if ($actual -ne $sum) { throw "install.ps1: $file does not match its SHA-256 in SHA256SUMS" }

    Expand-Archive -Path "$tmp\$file" -DestinationPath "$tmp\unpacked"
    $bin = Join-Path $install 'bin'
    New-Item -ItemType Directory -Force $bin | Out-Null
    Copy-Item "$tmp\unpacked\nucleus-$version\bin\*" $bin -Force
    & (Join-Path $bin 'nucleus.exe') --version | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'install.ps1: the installed nucleus does not run' }

    $path = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (($path -split ';') -contains $bin)) {
        [Environment]::SetEnvironmentVariable('Path', ($(if ($path) { "$path;" } else { '' }) + $bin), 'User')
        Write-Host "added $bin to your PATH (new terminals see it)"
    }
    $env:Path = "$bin;$env:Path"
    Write-Host "installed nucleus $version in $bin"
    Write-Host ''
    Write-Host 'a first query, in a database in memory:'
    Write-Host "  nucleus sql :memory: -c `"SELECT 'hello, nucleus'`""
    Write-Host 'a server (its data in .\data): nucleus server data - then: nucleus sql'
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
