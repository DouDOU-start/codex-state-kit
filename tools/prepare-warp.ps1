$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$downloadDir = Join-Path $projectRoot 'target/warp-download'
$resourceDir = Join-Path $projectRoot 'src-tauri/resources/warp'
$archive = Join-Path $downloadDir 'usque.zip'
$expected = 'f6f7f0a1a2bc9bcc15cf563ec1f892d00690a92c086b23ed3211b802209099e7'
$url = 'https://github.com/Diniboy1123/usque/releases/download/v4.2.1/usque_4.2.1_windows_amd64.zip'
New-Item -ItemType Directory -Force $downloadDir, $resourceDir | Out-Null
if (!(Test-Path -LiteralPath $archive) -or (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expected) {
    & curl.exe -fL --retry 3 --connect-timeout 15 --max-time 120 $url -o $archive
    if ($LASTEXITCODE -ne 0) { throw 'WARP archive download failed' }
}
if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expected) {
    throw 'WARP archive checksum mismatch; refusing to bundle it'
}
$unpacked = Join-Path $downloadDir 'unpacked'
Expand-Archive -LiteralPath $archive -DestinationPath $unpacked -Force
Copy-Item -LiteralPath (Join-Path $unpacked 'usque.exe') -Destination $resourceDir
$unixBinary = Join-Path $resourceDir 'usque'
if (Test-Path -LiteralPath $unixBinary) { Remove-Item -LiteralPath $unixBinary -Force }
Copy-Item -LiteralPath (Join-Path $unpacked 'LICENSE.md') -Destination $resourceDir
Write-Host 'Bundled usque v4.2.1 (Windows x64); SHA-256 verified.'
