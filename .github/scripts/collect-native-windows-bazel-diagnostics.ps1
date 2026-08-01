<#
Collect compact, non-secret evidence for a native Windows Bazel health run.

This intentionally records only runner/Bazel identity, configured cache paths,
filesystem headroom, and the existing compact execution-log inventory. It does
not export the full runner environment or copy Bazel output trees.
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null

function Write-TextFile {
    param(
        [string]$Name,
        [object[]]$Content
    )

    $Content | Out-File -FilePath (Join-Path $OutputDirectory $Name) -Encoding utf8
}

function Invoke-CapturedCommand {
    param(
        [string]$Name,
        [scriptblock]$Command
    )

    $output = & $Command 2>&1
    $exitCode = $LASTEXITCODE
    @(
        "exit-code=$exitCode"
        ''
        $output
    ) | Out-File -FilePath (Join-Path $OutputDirectory $Name) -Encoding utf8
}

$os = Get-CimInstance Win32_OperatingSystem |
    Select-Object Caption, Version, BuildNumber, OSArchitecture
$computer = Get-CimInstance Win32_ComputerSystem |
    Select-Object SystemType, NumberOfLogicalProcessors

[ordered]@{
    runner_os = $env:RUNNER_OS
    runner_arch = $env:RUNNER_ARCH
    ci_build_root = $env:CI_BUILD_ROOT
    bazel_output_base = $env:BAZEL_OUTPUT_BASE
    bazel_output_user_root = $env:BAZEL_OUTPUT_USER_ROOT
    bazel_repository_cache = $env:BAZEL_REPOSITORY_CACHE
    bazel_repo_contents_cache = $env:BAZEL_REPO_CONTENTS_CACHE
    os = $os
    computer = $computer
} | ConvertTo-Json -Depth 3 | Out-File -FilePath (Join-Path $OutputDirectory 'runner-and-cache.json') -Encoding utf8

Get-PSDrive -PSProvider FileSystem |
    Select-Object Name, Used, Free |
    Format-Table -AutoSize |
    Out-String |
    Out-File -FilePath (Join-Path $OutputDirectory 'filesystem-headroom.txt') -Encoding utf8

$bazel = Get-Command bazel -ErrorAction SilentlyContinue
if ($null -eq $bazel) {
    Write-TextFile -Name 'bazel-version.txt' -Content 'bazel was not found on PATH.'
    Write-TextFile -Name 'bazel-info.txt' -Content 'bazel was not found on PATH.'
} else {
    Invoke-CapturedCommand -Name 'bazel-version.txt' -Command { & $bazel.Source version }

    $bazelInfoArgs = @('info')
    if (-not [string]::IsNullOrWhiteSpace($env:BAZEL_REPOSITORY_CACHE)) {
        $bazelInfoArgs += "--repository_cache=$($env:BAZEL_REPOSITORY_CACHE)"
    }
    if (-not [string]::IsNullOrWhiteSpace($env:BAZEL_REPO_CONTENTS_CACHE)) {
        $bazelInfoArgs += "--repo_contents_cache=$($env:BAZEL_REPO_CONTENTS_CACHE)"
    }
    $bazelInfoArgs += @('release', 'workspace', 'output_base', 'execution_root', 'bazel-testlogs')
    Invoke-CapturedCommand -Name 'bazel-info.txt' -Command { & $bazel.Source @bazelInfoArgs }
}

$executionLogDirectory = Join-Path $env:RUNNER_TEMP 'bazel-execution-logs'
if (Test-Path $executionLogDirectory) {
    Get-ChildItem -File -Recurse -Path $executionLogDirectory |
        Select-Object FullName, Length, LastWriteTimeUtc |
        Format-Table -AutoSize |
        Out-String |
        Out-File -FilePath (Join-Path $OutputDirectory 'execution-log-inventory.txt') -Encoding utf8
} else {
    Write-TextFile -Name 'execution-log-inventory.txt' -Content 'No compact Bazel execution-log directory was present.'
}
