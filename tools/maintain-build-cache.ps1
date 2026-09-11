#Requires -Version 7.0

[CmdletBinding(SupportsShouldProcess)]
param(
    [ValidateRange(1, 4096)]
    [int]$ThresholdGB = 30,
    [switch]$Force,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repositoryRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$target = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'target'))
$expectedTarget = "$($repositoryRoot.TrimEnd([IO.Path]::DirectorySeparatorChar))$([IO.Path]::DirectorySeparatorChar)target"
if ($target -ne $expectedTarget) {
    throw "Refusing to clean an unexpected target path: $target"
}

function Get-DirectoryBytes([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) { return [long]0 }
    $total = [long]0
    foreach ($file in [IO.Directory]::EnumerateFiles($Path, '*', [IO.SearchOption]::AllDirectories)) {
        try { $total += [IO.FileInfo]::new($file).Length } catch { }
    }
    return $total
}

$beforeBytes = Get-DirectoryBytes $target
$thresholdBytes = [long]$ThresholdGB * 1GB
$shouldClean = $Force -or $beforeBytes -gt $thresholdBytes
$result = [ordered]@{
    schema_version = 1
    target = $target
    threshold_gb = $ThresholdGB
    before_gb = [math]::Round($beforeBytes / 1GB, 2)
    action = 'skipped'
    after_gb = [math]::Round($beforeBytes / 1GB, 2)
    reclaimed_gb = 0
}

if (-not $shouldClean) {
    $result.reason = 'below-threshold'
    $result | ConvertTo-Json
    exit 0
}

if ($DryRun) {
    $result.action = 'dry-run'
    $result.reason = if ($Force) { 'forced' } else { 'above-threshold' }
    $result | ConvertTo-Json
    exit 0
}

if (Get-Process -Name cargo, rustc -ErrorAction SilentlyContinue) {
    throw 'Cargo or rustc is running; build cache cleanup was not started.'
}

if ($PSCmdlet.ShouldProcess($target, 'Clean AsterFiles package build artifacts')) {
    & cargo clean --package asterfiles
    if ($LASTEXITCODE -ne 0) { throw "cargo clean failed with exit code $LASTEXITCODE" }
}

$afterBytes = Get-DirectoryBytes $target
$result.action = 'cleaned'
$result.reason = if ($Force) { 'forced' } else { 'above-threshold' }
$result.after_gb = [math]::Round($afterBytes / 1GB, 2)
$result.reclaimed_gb = [math]::Round(($beforeBytes - $afterBytes) / 1GB, 2)
$result | ConvertTo-Json