$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$packageUrl = 'https://persistent.oaistatic.com/codex-app-prod/ChatGPT-x64.msix'
$workDir = Join-Path $env:RUNNER_TEMP 'chatgpt-win-binary-research'
$packagePath = Join-Path $workDir 'ChatGPT-x64.msix'
$headersPath = Join-Path $workDir 'response-headers.txt'
New-Item -ItemType Directory -Path $workDir -Force | Out-Null

& curl.exe --fail --location --silent --show-error --dump-header $headersPath --output $packagePath $packageUrl
if ($LASTEXITCODE -ne 0) {
    throw "Official MSIX download failed with curl exit code $LASTEXITCODE"
}

$signature = Get-AuthenticodeSignature -LiteralPath $packagePath
$signatureSummary = [ordered]@{
    status = [string]$signature.Status
    subject = if ($signature.SignerCertificate) { [string]$signature.SignerCertificate.Subject } else { $null }
    thumbprint = if ($signature.SignerCertificate) { [string]$signature.SignerCertificate.Thumbprint } else { $null }
}

$python = Get-Command python.exe -ErrorAction Stop
& $python.Source (Join-Path $PSScriptRoot 'analyze-package.py') `
    --package $packagePath `
    --headers $headersPath `
    --source-url $packageUrl `
    --signature-json ($signatureSummary | ConvertTo-Json -Compress)
if ($LASTEXITCODE -ne 0) {
    throw "Package inspection failed with Python exit code $LASTEXITCODE"
}
