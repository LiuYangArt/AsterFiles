$scriptContent = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'publish.ps1') -Raw
$releaseWorkflowContent = Get-Content -LiteralPath (Join-Path (Split-Path -Parent $PSScriptRoot) '.github/workflows/release.yml') -Raw
$ciWorkflowContent = Get-Content -LiteralPath (Join-Path (Split-Path -Parent $PSScriptRoot) '.github/workflows/ci.yml') -Raw
$tokens = $null
$errors = $null
[System.Management.Automation.Language.Parser]::ParseInput($scriptContent, [ref]$tokens, [ref]$errors) | Out-Null

Describe 'publish.ps1' {
    It 'has valid PowerShell syntax' {
        @($errors) | Should BeNullOrEmpty
    }

    It 'defaults to automatic release selection' {
        $scriptContent | Should Match "\[string\]\`$Level = 'auto'"
        $scriptContent | Should Match "if \(\`$Level -eq 'auto'\)"
    }

    It 'promotes any feature issue to a feature release' {
        $scriptContent | Should Match "\`$types -contains 'type: feature'"
    }

    It 'rejects issues without exactly one supported type label' {
        $scriptContent | Should Match "\`$typeLabels.Count -ne 1"
        $scriptContent | Should Match 'Unsupported release issue type'
    }

    It 'writes generated notes into the annotated release tag' {
        $scriptContent | Should Match 'tag --annotate \$tag --message \$releaseNotes'
    }

    It 'uses an explicit valid tag refspec' {
        $scriptContent | Should Match "refs/tags/\{0\}:refs/tags/\{0\}"
    }
    It 'skips Windows CI for version-bump release commits without skipping the tag workflow' {
        $ciWorkflowContent | Should Match "if: `"`\$\{\{ github\.event_name != 'push' \|\| !startsWith\(github\.event\.head_commit\.message, 'chore: release '"
        $ciWorkflowContent | Should Not Match '\[skip ci\]'
        $scriptContent | Should Match 'chore: release \$tag'
        $scriptContent | Should Not Match '\[skip ci\]'
        $releaseWorkflowContent | Should Match 'tags:'
        $releaseWorkflowContent | Should Match '"v\*"'
        $releaseWorkflowContent | Should Not Match 'chore: release'
    }

    It 'publishes tag notes through GH_REPO for CLI compatibility' {
        $releaseWorkflowContent | Should Match 'GH_REPO: \$\{\{ github\.repository \}\}'
        ([regex]::Matches($releaseWorkflowContent, 'uses: actions/checkout@')).Count | Should Be 2
        $releaseWorkflowContent | Should Match 'gh release create .*--notes-from-tag'
        $releaseWorkflowContent | Should Not Match 'gh release create .*--repo'
    }
}