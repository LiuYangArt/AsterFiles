param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^v\d+\.\d+\.\d+$')]
    [string]$Tag,
    [switch]$SkipBuild,
    [string]$OutputDirectory = 'artifacts/release'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repositoryRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repositoryRoot

$metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
$releasePackage = $metadata.packages | Where-Object name -eq 'asterfiles' | Select-Object -First 1
if (-not $releasePackage) {
    throw 'Cargo metadata does not contain the asterfiles package.'
}

$version = [string]$releasePackage.version
if ($Tag -ne "v$version") {
    throw "Release tag '$Tag' does not match Cargo version 'v$version'."
}

if (-not $SkipBuild) {
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
}

$executable = Join-Path $repositoryRoot 'target/release/asterfiles.exe'
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
    throw "Release executable does not exist: $executable"
}

$outputRoot = [System.IO.Path]::GetFullPath((Join-Path $repositoryRoot $OutputDirectory))
$repositoryPrefix = $repositoryRoot.TrimEnd([System.IO.Path]::DirectorySeparatorChar) + [System.IO.Path]::DirectorySeparatorChar
if (-not $outputRoot.StartsWith($repositoryPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw 'Output directory must stay inside the repository.'
}
$staging = Join-Path $outputRoot "AsterFiles-$version-windows-x64-portable"
if (Test-Path -LiteralPath $staging) {
    Remove-Item -LiteralPath $staging -Recurse -Force
}
New-Item -ItemType Directory -Path $staging -Force | Out-Null

Copy-Item -LiteralPath $executable -Destination $staging
Copy-Item -LiteralPath (Join-Path $repositoryRoot 'README.md') -Destination $staging
Copy-Item -LiteralPath (Join-Path $repositoryRoot 'LICENSE') -Destination $staging
Copy-Item -LiteralPath (Join-Path $repositoryRoot 'THIRD_PARTY_LICENSES.md') -Destination $staging

$archive = Join-Path $outputRoot "AsterFiles-$version-windows-x64-portable.zip"
if (Test-Path -LiteralPath $archive) {
    Remove-Item -LiteralPath $archive -Force
}
Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $archive -CompressionLevel Optimal
Remove-Item -LiteralPath $staging -Recurse -Force

[PSCustomObject]@{
    version = $version
    archive = $archive
} | ConvertTo-Json
