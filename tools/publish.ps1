#Requires -Version 7.0

[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('auto', 'major', 'feature', 'bugfix')]
    [string]$Level = 'auto',
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

function Invoke-GhJson {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    $output = & gh @Arguments
    if ($LASTEXITCODE -ne 0) { throw "GitHub CLI command failed: gh $($Arguments -join ' ')" }
    return ($output -join "`n") | ConvertFrom-Json
}

function Get-ReleaseIssues {
    param(
        [string]$Repository,
        [datetime]$Since
    )

    $sinceText = $Since.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    $pages = @(Invoke-GhJson api "repos/$Repository/issues?state=closed&since=$sinceText&per_page=100" --paginate --slurp)
    $issues = foreach ($page in $pages) {
        foreach ($item in @($page)) {
            if ($item.PSObject.Properties.Name -notcontains 'pull_request' -and [datetime]$item.closed_at -gt $Since) {
                $item
            }
        }
    }
    return @($issues | Sort-Object { [int]$_.number } -Unique)
}

function Get-IssueType {
    param($Issue)

    $typeLabels = @($Issue.labels | ForEach-Object { [string]$_.name } | Where-Object { $_ -like 'type: *' })
    if ($typeLabels.Count -ne 1) {
        throw "Issue #$($Issue.number) must have exactly one 'type: *' label before release."
    }
    return $typeLabels[0]
}

function Get-AutomaticLevel {
    param([object[]]$Issues)

    if ($Issues.Count -eq 0) {
        throw 'No completed issues were found after the latest release. Nothing to publish.'
    }
    $types = @($Issues | ForEach-Object { Get-IssueType $_ })
    $supportedTypes = @('type: feature', 'type: bug', 'type: maintenance', 'type: docs')
    $unsupportedTypes = @($types | Where-Object { $_ -notin $supportedTypes } | Sort-Object -Unique)
    if ($unsupportedTypes.Count -gt 0) {
        throw "Unsupported release issue type(s): $($unsupportedTypes -join ', ')."
    }
    if ($types -contains 'type: feature') { return 'feature' }
    return 'bugfix'
}

function Get-ReleaseNotes {
    param(
        [string]$PreviousTag,
        [object[]]$Issues
    )

    $sections = [ordered]@{
        'type: feature' = '## 新功能与改进'
        'type: bug' = '## 问题修复'
        'type: maintenance' = '## 工程维护'
        'type: docs' = '## 文档'
    }
    $lines = [System.Collections.Generic.List[string]]::new()
    $lines.Add("自 $PreviousTag 以来完成的事项：")
    $lines.Add('')

    foreach ($type in $sections.Keys) {
        $matching = @($Issues | Where-Object { (Get-IssueType $_) -eq $type })
        if ($matching.Count -eq 0) { continue }
        $lines.Add($sections[$type])
        $lines.Add('')
        foreach ($issue in $matching) {
            $lines.Add("- $($issue.title) ([#$($issue.number)]($($issue.html_url)))")
        }
        $lines.Add('')
    }


    return ($lines -join "`n").TrimEnd()
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

$latestRelease = Invoke-GhJson release view --json 'tagName,createdAt'
$previousTag = [string]$latestRelease.tagName
$previousReleaseTime = [datetime]$latestRelease.createdAt
$repository = [string](Invoke-GhJson repo view --json nameWithOwner).nameWithOwner
$releaseIssues = @(Get-ReleaseIssues -Repository $repository -Since $previousReleaseTime)
$selectedLevel = if ($Level -eq 'auto') { Get-AutomaticLevel $releaseIssues } else { $Level }
$releaseNotes = Get-ReleaseNotes -PreviousTag $previousTag -Issues $releaseIssues

$currentVersionParts = $currentVersion.Split('.')
$major = [int]$currentVersionParts[0]
$minor = [int]$currentVersionParts[1]
$patch = [int]$currentVersionParts[2]
$nextVersion = switch ($selectedLevel) {
    'major' { "$($major + 1).0.0" }
    'feature' { "$major.$($minor + 1).0" }
    'bugfix' { "$major.$minor.$($patch + 1)" }
}
$tag = "v$nextVersion"

if ($DryRun) {
    [PSCustomObject]@{
        requestedLevel = $Level
        selectedLevel = $selectedLevel
        currentVersion = $currentVersion
        nextVersion = $nextVersion
        tag = $tag
        previousTag = $previousTag
        issues = @($releaseIssues | ForEach-Object { [PSCustomObject]@{ number = $_.number; title = $_.title; type = Get-IssueType $_ } })
        releaseNotes = $releaseNotes
        dryRun = $true
    } | ConvertTo-Json -Depth 5
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
    Invoke-Git tag --annotate $tag --message $releaseNotes
    $tagCreated = $true
    Invoke-Git push --atomic origin 'HEAD:refs/heads/main' ('refs/tags/{0}:refs/tags/{0}' -f $tag)

    [PSCustomObject]@{
        version = $nextVersion
        level = $selectedLevel
        tag = $tag
        commit = ([string](Get-GitOutput rev-parse HEAD)).Trim()
        issues = @($releaseIssues | ForEach-Object { $_.number })
        release = 'GitHub Action has been triggered by the tag push.'
    } | ConvertTo-Json
}
catch {
    if ($versionChanged -and -not $versionCommitted) {
        & git restore --staged --worktree -- Cargo.toml Cargo.lock 2>$null
    }
    elseif ($versionCommitted) {
        $recovery = if ($tagCreated) {
            $retryCommand = 'git push --atomic origin HEAD:refs/heads/main refs/tags/{0}:refs/tags/{0}' -f $tag
            "The local release commit and tag '$tag' were kept. Do not run publish.ps1 again. Resolve the error, then retry with: $retryCommand"
        }
        else {
            'The local release commit was kept. Resolve the error before creating or pushing the tag.'
        }
        Write-Warning $recovery
    }
    throw
}
