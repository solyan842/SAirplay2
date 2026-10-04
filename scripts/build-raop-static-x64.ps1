param(
    [string]$OutDir = "target/raop-static-link"
)

$ErrorActionPreference = "Stop"
$msaPin = "431c5c582eef9307c4e39c50a0ea65e970bc1128"
$libraopPin = "81c2182649da8645ac2a58b78e9f370c79a4165b"
$workspace = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$vendor = Join-Path $workspace "target/vendor/msa-airplay-cli-static"
$nativeBuild = Join-Path $workspace "target/raop-static-build"
$pthreadBuild = Join-Path $workspace "target/raop-pthread-build"
$opensslOut = Join-Path $workspace "target/raop-openssl-x64"
$out = if ([IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $workspace $OutDir }

function Invoke-Checked([scriptblock]$Command, [string]$Message) {
    & $Command
    if ($LASTEXITCODE -ne 0) { throw $Message }
}

if (-not (Test-Path (Join-Path $vendor ".git"))) {
    New-Item -ItemType Directory -Force -Path (Split-Path $vendor) | Out-Null
    Invoke-Checked { git clone --filter=blob:none https://github.com/music-assistant/airplay-cli.git $vendor } "clone pinned MSA airplay-cli failed"
}
Invoke-Checked { git -C $vendor fetch --depth=1 origin $msaPin } "fetch pinned MSA airplay-cli failed"
Invoke-Checked { git -C $vendor checkout --detach $msaPin } "checkout pinned MSA airplay-cli failed"
Invoke-Checked { git -C $vendor submodule sync --recursive } "MSA submodule sync failed"
Invoke-Checked { git -C $vendor submodule update --init --recursive libraop } "MSA libraop submodules failed"

$actualLibraop = (git -C (Join-Path $vendor "libraop") rev-parse HEAD).Trim()
if ($actualLibraop -ne $libraopPin) {
    throw "MSA libraop pin mismatch: expected $libraopPin, got $actualLibraop"
}

$libraop = Join-Path $vendor "libraop"
$opensslSrc = Join-Path $libraop "libopenssl/openssl"
$pthreadSrc = Join-Path $libraop "libpthreads4w"

# Exact pinned OpenSSL source, built static for the same x64 process as Rust.
$cryptoDst = Join-Path $opensslOut "sairplay_crypto.lib"
$sslDst = Join-Path $opensslOut "sairplay_ssl.lib"
if (-not (Test-Path $cryptoDst) -or -not (Test-Path $sslDst)) {
    New-Item -ItemType Directory -Force -Path $opensslOut | Out-Null
    Push-Location $opensslSrc
    try {
        # Configure in-place exactly once for x64 static output. If a cached
        # source tree was previously configured for another target, reset only
        # generated OpenSSL build state, never the pinned source revision.
        if (Test-Path "Makefile") {
            cmd /c "nmake clean" | Out-Host
        }
        Invoke-Checked { perl Configure VC-WIN64A no-shared no-tests } "OpenSSL VC-WIN64A configure failed"
        Invoke-Checked { cmd /c "nmake build_libs" } "OpenSSL x64 static build failed"
        $crypto = Get-ChildItem -Recurse -File -Filter "libcrypto.lib" | Select-Object -First 1
        $ssl = Get-ChildItem -Recurse -File -Filter "libssl.lib" | Select-Object -First 1
        if (-not $crypto -or -not $ssl) { throw "OpenSSL static libraries not found" }
        Copy-Item $crypto.FullName $cryptoDst -Force
        Copy-Item $ssl.FullName $sslDst -Force
    }
    finally { Pop-Location }
}

# pthreads4w already provides a dedicated static target; build that target as x64.
cmake -S $pthreadSrc -B $pthreadBuild -A x64 -DBUILD_TESTING=OFF -DPTHREADS4W_BUILD_C=ON -DPTHREADS4W_BUILD_CE=OFF -DPTHREADS4W_BUILD_SE=OFF
if ($LASTEXITCODE -ne 0) { throw "pthreads4w x64 configure failed" }
cmake --build $pthreadBuild --config Release --target pthreadVC-static
if ($LASTEXITCODE -ne 0) { throw "pthreads4w x64 static build failed" }
$pthread = Get-ChildItem $pthreadBuild -Recurse -File -Filter "libpthreadVC3.lib" | Select-Object -First 1
if (-not $pthread) { throw "pthreads4w static library not found" }

# Build only the RAOP source set used by the pinned MSA route, plus the thin
# SAirplay2 C ABI. No cliraop main(), child process, pipe or sidecar protocol.
$opensslInclude = Join-Path $opensslSrc "include"
cmake -S (Join-Path $workspace "native/raop-static") -B $nativeBuild -A x64 `
    "-DMSA_AIRPLAY_CLI_ROOT=$vendor" `
    "-DOPENSSL_INCLUDE_DIR=$opensslInclude" `
    "-DPTHREAD_INCLUDE_DIR=$pthreadSrc"
if ($LASTEXITCODE -ne 0) { throw "SAirplay2 RAOP static configure failed" }
cmake --build $nativeBuild --config Release --target sairplay_raop_bridge
if ($LASTEXITCODE -ne 0) { throw "SAirplay2 RAOP static build failed" }
$bridge = Get-ChildItem $nativeBuild -Recurse -File -Filter "sairplay_raop_bridge.lib" | Select-Object -First 1
if (-not $bridge) { throw "sairplay_raop_bridge.lib not found" }

New-Item -ItemType Directory -Force -Path $out | Out-Null
Copy-Item $bridge.FullName (Join-Path $out "sairplay_raop_bridge.lib") -Force
Copy-Item $pthread.FullName (Join-Path $out "sairplay_pthread.lib") -Force
Copy-Item $cryptoDst (Join-Path $out "sairplay_crypto.lib") -Force
Copy-Item $sslDst (Join-Path $out "sairplay_ssl.lib") -Force

$stamp = @(
    "msa_airplay_cli=$msaPin",
    "libraop=$libraopPin",
    "arch=x64",
    "integration=in-process-static"
) -join "`n"
Set-Content -Path (Join-Path $out "SOURCE-PIN.txt") -Value $stamp -Encoding ascii

Write-Host "RAOP static link set ready: $out"
Get-ChildItem $out | Format-Table Name,Length
