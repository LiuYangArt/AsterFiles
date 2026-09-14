#Requires -Version 7.0

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateRange(1, [int]::MaxValue)]
    [int]$Issue,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$Message,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string[]]$Paths,
    [switch]$NoReuse
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'finish-issue-support.ps1')

$repositoryRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repositoryRoot
$repository = Invoke-GhText -Arguments @('repo', 'view', '--json', 'nameWithOwner', '--jq', '.nameWithOwner') `
    -FailureContext 'Unable to resolve the GitHub repository.'
if (-not $repository) { throw 'Unable to resolve the GitHub repository.' }

$issueState = Invoke-GhText -Arguments @(
    'issue', 'view', [string]$Issue, '--repo', $repository, '--json', 'state', '--jq', '.state'
) -FailureContext "Unable to read Issue #$Issue."
if ($issueState -ne 'OPEN') { throw "Issue #$Issue is not open." }

$projectContext = Get-FinishIssueProjectContext -Owner 'LiuYangArt' -Repository $repository -Issue $Issue

& git diff --cached --quiet
if ($LASTEXITCODE -eq 1) { throw 'Staged changes already exist; finish or unstage them first.' }
if ($LASTEXITCODE -ne 0) { throw 'Unable to inspect staged changes.' }

& git add -- $Paths
if ($LASTEXITCODE -ne 0) { throw 'Unable to stage the requested paths.' }
& git diff --cached --quiet
if ($LASTEXITCODE -eq 0) { throw 'The requested paths contain no changes.' }
if ($LASTEXITCODE -ne 1) { throw 'Unable to inspect staged changes.' }
& git diff --quiet
if ($LASTEXITCODE -ne 0) { throw 'Unstaged tracked changes remain; include them explicitly or finish them separately.' }
$untracked = @(& git ls-files --others --exclude-standard)
if ($LASTEXITCODE -ne 0) { throw 'Unable to inspect untracked files.' }
if ($untracked.Count -gt 0) { throw "Untracked files remain: $($untracked -join ', ')" }

$verifyArguments = @('tools/verify.py', '--release')
if ($NoReuse) { $verifyArguments += '--no-reuse' }
& python @verifyArguments
if ($LASTEXITCODE -ne 0) { throw 'Release validation failed; finish was cancelled.' }

& git diff --cached --check
if ($LASTEXITCODE -ne 0) { throw 'Staged changes failed whitespace validation.' }
& git commit -m "$Message (#$Issue)"
if ($LASTEXITCODE -ne 0) { throw 'Git commit failed.' }
$commit = ([string](& git rev-parse --short HEAD)).Trim()

$summary = Join-Path $repositoryRoot 'artifacts/verify/summary.json'
$comment = @"
用户已确认验收完成。

- Release 收尾验证通过：格式、Clippy、测试、全部无界面场景与 Release 构建。
- 验证汇总：``artifacts/verify/summary.json``
- Release 程序：``target/release/asterfiles.exe``
- Commit：``$commit``
"@
Complete-GitHubIssue -Issue $Issue -Repository $repository -Comment $comment `
    -ProjectContext $projectContext -Commit $commit

[PSCustomObject]@{
    issue = $Issue
    commit = $commit
    verification = $summary
    projectStatus = 'Done'
    issueState = 'CLOSED'
} | ConvertTo-Json