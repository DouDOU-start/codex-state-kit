$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$downloadDir = Join-Path $projectRoot 'target/mihomo-download'
$resourceDir = Join-Path $projectRoot 'src-tauri/resources/mihomo'
$version = 'v1.19.31'
$asset = "mihomo-windows-amd64-$version.zip"
$archive = Join-Path $downloadDir $asset
$expected = '38b2420799d9e7cde77ec1a19c7150dd17ca77f7fb82d9f62cb8763a307eee67'
$url = "https://github.com/MetaCubeX/mihomo/releases/download/$version/$asset"
New-Item -ItemType Directory -Force $downloadDir, $resourceDir | Out-Null
function Get-Sha256([string] $path) {
    $stream = [System.IO.File]::OpenRead($path)
    try {
        $hash = [System.Security.Cryptography.SHA256]::Create().ComputeHash($stream)
        return ([System.BitConverter]::ToString($hash).Replace('-', '')).ToLowerInvariant()
    } finally {
        $stream.Dispose()
    }
}
if (!(Test-Path -LiteralPath $archive) -or (Get-Sha256 $archive) -ne $expected) {
    & curl.exe -fL --retry 3 --connect-timeout 15 --max-time 180 $url -o $archive
    if ($LASTEXITCODE -ne 0) { throw 'Mihomo archive download failed' }
}
if ((Get-Sha256 $archive) -ne $expected) {
    throw 'Mihomo archive checksum mismatch; refusing to bundle it'
}
$unpacked = Join-Path $downloadDir 'unpacked-windows'
if (Test-Path -LiteralPath $unpacked) { Remove-Item -LiteralPath $unpacked -Recurse -Force }
Expand-Archive -LiteralPath $archive -DestinationPath $unpacked -Force
$exe = Get-ChildItem -LiteralPath $unpacked -Recurse -Filter *.exe | Select-Object -First 1
if (-not $exe) { throw 'Mihomo Windows archive did not contain an executable' }
Copy-Item -LiteralPath $exe.FullName -Destination (Join-Path $resourceDir 'mihomo.exe') -Force
$unixBinary = Join-Path $resourceDir 'mihomo'
if (Test-Path -LiteralPath $unixBinary) { Remove-Item -LiteralPath $unixBinary -Force }
Write-Host "Bundled mihomo $version (Windows x64); SHA-256 verified."
