[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$PublishDir
)

$ErrorActionPreference = 'Stop'
$PfxPath = $env:AIRFERRY_WINDOWS_PFX
$Password = $env:AIRFERRY_WINDOWS_PFX_PASSWORD
$ExpectedThumbprint = $env:AIRFERRY_WINDOWS_CERT_THUMBPRINT

foreach ($Pair in @(
    @('AIRFERRY_WINDOWS_PFX', $PfxPath),
    @('AIRFERRY_WINDOWS_PFX_PASSWORD', $Password),
    @('AIRFERRY_WINDOWS_CERT_THUMBPRINT', $ExpectedThumbprint)
)) {
    if ([string]::IsNullOrWhiteSpace($Pair[1])) {
        throw "Required Windows release-signing setting is missing: $($Pair[0])"
    }
}
if (-not (Test-Path -LiteralPath $PfxPath -PathType Leaf)) {
    throw "Windows signing PFX not found: $PfxPath"
}
if (-not (Test-Path -LiteralPath $PublishDir -PathType Container)) {
    throw "Windows publish directory not found: $PublishDir"
}

$SecurePassword = ConvertTo-SecureString $Password -AsPlainText -Force
# Do not let signtool or any later child process inherit the plaintext secret.
Remove-Item Env:AIRFERRY_WINDOWS_PFX_PASSWORD -ErrorAction SilentlyContinue
$Password = $null
$Certificate = Get-PfxCertificate -FilePath $PfxPath -Password $SecurePassword
$Expected = $ExpectedThumbprint.Replace(' ', '').ToUpperInvariant()
$Actual = $Certificate.Thumbprint.Replace(' ', '').ToUpperInvariant()
if ($Actual -ne $Expected) {
    throw "Windows signing certificate thumbprint mismatch: $Actual"
}
$StorePath = "Cert:\CurrentUser\My\$Actual"
$WasInstalled = Test-Path -LiteralPath $StorePath
try {
    if (-not $WasInstalled) {
        $Imported = Import-PfxCertificate `
            -FilePath $PfxPath `
            -CertStoreLocation 'Cert:\CurrentUser\My' `
            -Password $SecurePassword `
            -Exportable:$false
        if ($null -eq $Imported -or -not (Test-Path -LiteralPath $StorePath)) {
            throw 'Windows signing certificate could not be imported into the current-user store'
        }
    }

    $WindowsKits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $SignTool = Get-ChildItem $WindowsKits -Recurse -File -Filter signtool.exe |
        Where-Object FullName -Match '\\x64\\signtool\.exe$' |
        Sort-Object FullName -Descending |
        Select-Object -First 1
    if ($null -eq $SignTool) { throw 'signtool.exe not found' }

    foreach ($Name in @('AirFerry.exe', 'transfer_engine.dll', 'airferry_zxing.dll')) {
        $Target = Join-Path $PublishDir $Name
        if (-not (Test-Path -LiteralPath $Target -PathType Leaf)) {
            throw "Windows signing target missing: $Target"
        }
        # Select the temporary/current-user certificate by pinned thumbprint.
        # The PFX password never appears in signtool's process arguments.
        & $SignTool.FullName sign /fd SHA256 /td SHA256 /tr http://timestamp.digicert.com /s My /sha1 $Actual $Target
        if ($LASTEXITCODE -ne 0) { throw "Authenticode signing failed: $Target" }
        $Signature = Get-AuthenticodeSignature -LiteralPath $Target
        if ($Signature.Status -ne 'Valid') {
            throw "Authenticode verification failed for $Target`: $($Signature.StatusMessage)"
        }
        $Signer = $Signature.SignerCertificate.Thumbprint.Replace(' ', '').ToUpperInvariant()
        if ($Signer -ne $Expected) { throw "Unexpected Authenticode signer for $Target" }
    }
} finally {
    # Preserve a certificate that was already installed by the user; remove
    # only the private-key-bearing entry imported by this invocation.
    if (-not $WasInstalled -and (Test-Path -LiteralPath $StorePath)) {
        Remove-Item -LiteralPath $StorePath -Force
    }
}

Write-Host 'Authenticode signatures verified for AirFerry.exe and native DLLs'
