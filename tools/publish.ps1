#Requires -Version 7.0

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet('major', 'feature', 'bugfix')]
    [string]$Level,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-Git {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    & git @Arguments
    if ($LASTEXITCODE -ne 0) { throw "Git command failed: git $($Arguments -join ' ')" }
}

function Get-GitOutput {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    $output = & git @Arguments
    if ($LASTEXITCODE -ne 0) { throw "Git command failed: git $($Arguments -join ' ')" }
    return $output
}

$repositoryRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repositoryRoot

$metadataJson = & cargo metadata --locked --no-deps --format-version 1
if ($LASTEXITCODE -ne 0) { throw 'Unable to read Cargo metadata.' }
$metadata = $metadataJson | ConvertFrom-Json
$releasePackage = $metadata.packages | Where-Object name -eq 'asterfiles' | Select-Object -First 1
if (-not $releasePackage) { throw 'Cargo metadata does not contain the asterfiles package.' }

$currentVersion = [string]$releasePackage.version
if ($currentVersion -notmatch '^(?<major>\d+)\.(?<minor>\d+)\.(?<patch>\d+)$') {
    throw "Cargo version '$currentVersion' is not a stable semantic version."
}

$major = [int]$Matches.major
$minor = [int]$Matches.minor
$patch = [int]$Matches.patch
$nextVersion = switch ($Level) {
    'major' { "$($major + 1).0.0" }
    'feature' { "$major.$($minor + 1).0" }
    'bugfix' { "$major.$minor.$($patch + 1)" }
}
$tag = "v$nextVersion"

if ($DryRun) {
    [PSCustomObject]@{
        level = $Level
        currentVersion = $currentVersion
        nextVersion = $nextVersion
        tag = $tag
        dryRun = $true
    } | ConvertTo-Json
    exit 0
}

$branch = ([string](Get-GitOutput branch --show-current)).Trim()
if ($branch -ne 'main') { throw "Releases must be created from main. Current branch: '$branch'." }

$worktreeStatus = @(Get-GitOutput status --porcelain=v1)
if ($worktreeStatus.Count -gt 0) {
    throw "The working tree is not clean. Commit or stash all changes before releasing. $($worktreeStatus -join [Environment]::NewLine)"
}

Invoke-Git remote get-url origin | Out-Null
Invoke-Git fetch origin main --tags
& git merge-base --is-ancestor refs/remotes/origin/main HEAD
if ($LASTEXITCODE -ne 0) {
    throw 'Local main is behind or has diverged from origin/main. Synchronize it before releasing.'
}

& git show-ref --verify --quiet "refs/tags/$tag"
if ($LASTEXITCODE -eq 0) { throw "Tag '$tag' already exists." }
if ($LASTEXITCODE -ne 1) { throw "Unable to check whether tag '$tag' exists." }

$manifestPath = Join-Path $repositoryRoot 'Cargo.toml'
$versionCommitted = $false
$versionChanged = $false
$tagCreated = $false

try {
    $manifest = Get-Content -LiteralPath $manifestPath -Raw
    $packageSectionMatch = [regex]::Match($manifest, '(?ms)^\[package\]\s*.*?(?=^\[|\z)')
    if (-not $packageSectionMatch.Success) {
        throw 'Cargo.toml does not contain a [package] section.'
    }

    $versionPattern = '(?m)^(version\s*=\s*")' + [regex]::Escape($currentVersion) + '("\s*)$'
    $versionMatches = [regex]::Matches($packageSectionMatch.Value, $versionPattern)
    if ($versionMatches.Count -ne 1) {
        throw "Expected package version '$currentVersion' exactly once in Cargo.toml."
    }

    $updatedPackageSection = [regex]::Replace(
        $packageSectionMatch.Value,
        $versionPattern,
        { param($match) $match.Groups[1].Value + $nextVersion + $match.Groups[2].Value },
        1
    )
    $updatedManifest = $manifest.Remove($packageSectionMatch.Index, $packageSectionMatch.Length)
    $updatedManifest = $updatedManifest.Insert($packageSectionMatch.Index, $updatedPackageSection)
    [System.IO.File]::WriteAllText($manifestPath, $updatedManifest, [System.Text.UTF8Encoding]::new($false))
    $versionChanged = $true

    & python tools/verify.py
    if ($LASTEXITCODE -ne 0) { throw 'Project verification failed. Release was cancelled.' }

    $updatedMetadataJson = & cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'Cargo.lock was not updated for the new version.' }
    $updatedMetadata = $updatedMetadataJson | ConvertFrom-Json
    $updatedPackage = $updatedMetadata.packages | Where-Object name -eq 'asterfiles' | Select-Object -First 1
    if ([string]$updatedPackage.version -ne $nextVersion) {
        throw "Cargo metadata does not report the expected version '$nextVersion'."
    }

    $changedPaths = @(Get-GitOutput status --porcelain=v1 | ForEach-Object { $_.Substring(3) })
    $unexpectedPaths = @($changedPaths | Where-Object { $_ -notin @('Cargo.toml', 'Cargo.lock') })
    if ($unexpectedPaths.Count -gt 0) {
        throw "Verification changed unexpected files: $($unexpectedPaths -join ', ')"
    }

    Invoke-Git add -- Cargo.toml Cargo.lock
    Invoke-Git diff --cached --check
    Invoke-Git commit -m "chore: release $tag"
    $versionCommitted = $true
    Invoke-Git tag --annotate $tag --message "AsterFiles $tag"
    $tagCreated = $true
    Invoke-Git push --atomic origin 'HEAD:refs/heads/main' "refs/tags/$tag:refs/tags/$tag"

    [PSCustomObject]@{
        version = $nextVersion
        tag = $tag
        commit = ([string](Get-GitOutput rev-parse HEAD)).Trim()
        release = 'GitHub Action has been triggered by the tag push.'
    } | ConvertTo-Json
}
catch {
    if ($versionChanged -and -not $versionCommitted) {
        & git restore --staged --worktree -- Cargo.toml Cargo.lock 2>$null
    }
    elseif ($versionCommitted) {
        $recovery = if ($tagCreated) {
            $retryCommand = "git push --atomic origin HEAD:refs/heads/main refs/tags/$tag:refs/tags/$tag"
            "The local release commit and tag '$tag' were kept. Do not run publish.ps1 again. Resolve the error, then retry with: $retryCommand"
        }
        else {
            'The local release commit was kept. Resolve the error before creating or pushing the tag.'
        }
        Write-Warning $recovery
    }
    throw
}
