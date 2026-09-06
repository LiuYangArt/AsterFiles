$scriptContent = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'publish.ps1') -Raw
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
}