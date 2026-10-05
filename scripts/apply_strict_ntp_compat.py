from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(
            f"{path}: expected exactly one match, got {count}: {old[:100]!r}"
        )
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


def append_once(path: str, marker: str, addition: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if marker in text:
        raise SystemExit(f"{path}: marker already present: {marker}")
    p.write_text(text.rstrip() + "\n\n" + addition.strip() + "\n", encoding="utf-8")


def write_new(path: str, content: str) -> None:
    p = Path(path)
    if p.exists():
        raise SystemExit(f"{path}: already exists")
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(content, encoding="utf-8")


session = "crates/sairplay-msa-solo/src/windows_raop_session.rs"
replace_once(
    session,
    """    pub bit_depth: u16,\n    pub channels: u16,\n    pub lead_ms: u32,\n""",
    """    pub bit_depth: u16,\n    pub channels: u16,\n    /// Select the isolated strict-NTP DLL for AppleTV3-class RAOP receivers.\n    /// False preserves the hardware-locked legacy Windows clock path exactly.\n    pub strict_ntp_clock: bool,\n    pub lead_ms: u32,\n""",
)
replace_once(
    session,
    """            bit_depth: 16,\n            channels: 2,\n            lead_ms: 2_000,\n""",
    """            bit_depth: 16,\n            channels: 2,\n            strict_ntp_clock: false,\n            lead_ms: 2_000,\n""",
)
replace_once(
    session,
    """type HeadFn = unsafe extern \"C\" fn(*mut c_void) -> u64;\n""",
    """type HeadFn = unsafe extern \"C\" fn(*mut c_void) -> u64;\ntype ClockModeFn = unsafe extern \"C\" fn(i32);\n""",
)
replace_once(
    session,
    """    open: OpenFn,\n    close: CloseFn,\n    healthy: BoolFn,\n""",
    """    open: OpenFn,\n    close: CloseFn,\n    set_strict_ntp_clock: ClockModeFn,\n    healthy: BoolFn,\n""",
)
replace_once(
    session,
    """        let open = symbol!(\"sr_raop_open\", OpenFn);\n        let close = symbol!(\"sr_raop_close\", CloseFn);\n        let healthy = symbol!(\"sr_raop_healthy\", BoolFn);\n""",
    """        let open = symbol!(\"sr_raop_open\", OpenFn);\n        let close = symbol!(\"sr_raop_close\", CloseFn);\n        let set_strict_ntp_clock = symbol!(\"sr_raop_set_strict_ntp_clock\", ClockModeFn);\n        let healthy = symbol!(\"sr_raop_healthy\", BoolFn);\n""",
)
replace_once(
    session,
    """            _library: library, open, close, healthy, keepalive, commit_start,\n            start_after_flush, flush, standby, pause, play, stop, set_volume,\n""",
    """            _library: library, open, close, set_strict_ntp_clock, healthy, keepalive, commit_start,\n            start_after_flush, flush, standby, pause, play, stop, set_volume,\n""",
)
replace_once(
    session,
    """fn inproc_dll_path() -> Result<Option<PathBuf>, MsaRaopError> {\n    if let Some(path) = std::env::var_os(\"SAIRPLAY_RAOP_INPROC_DLL\") {\n        let path = PathBuf::from(path);\n        if path.is_file() { return Ok(Some(path)); }\n        return Err(MsaRaopError::InProcess(format!(\"configured DLL missing: {}\", path.display())));\n    }\n    let exe = std::env::current_exe().map_err(MsaRaopError::Io)?;\n    let path = exe.parent().unwrap_or(Path::new(\".\")).join(\"sairplay-raop.dll\");\n    Ok(path.is_file().then_some(path))\n}\n""",
    """fn inproc_dll_path(strict_ntp_clock: bool) -> Result<Option<PathBuf>, MsaRaopError> {\n    let override_name = if strict_ntp_clock {\n        \"SAIRPLAY_RAOP_STRICT_INPROC_DLL\"\n    } else {\n        \"SAIRPLAY_RAOP_INPROC_DLL\"\n    };\n    if let Some(path) = std::env::var_os(override_name) {\n        let path = PathBuf::from(path);\n        if path.is_file() { return Ok(Some(path)); }\n        return Err(MsaRaopError::InProcess(format!(\n            \"configured {} missing: {}\",\n            if strict_ntp_clock { \"strict-NTP DLL\" } else { \"DLL\" },\n            path.display(),\n        )));\n    }\n    let exe = std::env::current_exe().map_err(MsaRaopError::Io)?;\n    let file_name = if strict_ntp_clock {\n        \"sairplay-raop-strict.dll\"\n    } else {\n        \"sairplay-raop.dll\"\n    };\n    let path = exe.parent().unwrap_or(Path::new(\".\")).join(file_name);\n    if strict_ntp_clock && !path.is_file() {\n        return Err(MsaRaopError::InProcess(format!(\n            \"strict-NTP RAOP DLL missing: {}\",\n            path.display(),\n        )));\n    }\n    Ok(path.is_file().then_some(path))\n}\n""",
)
replace_once(
    session,
    """    let Some(path) = inproc_dll_path()? else { return Ok(None) };\n    let api = unsafe { InprocApi::load(&path)? };\n\n    let host = cstring(&config.host, \"host\")?;\n""",
    """    let Some(path) = inproc_dll_path(config.strict_ntp_clock)? else { return Ok(None) };\n    let api = unsafe { InprocApi::load(&path)? };\n    unsafe {\n        (api.set_strict_ntp_clock)(if config.strict_ntp_clock { 1 } else { 0 });\n    }\n\n    let host = cstring(&config.host, \"host\")?;\n""",
)
replace_once(
    session,
    """                log: Arc::new(Mutex::new(vec![format!(\n                    \"MSA-RAOP backend=in-process dll={}\", path.display()\n                )])),\n""",
    """                log: Arc::new(Mutex::new(vec![format!(\n                    \"MSA-RAOP backend=in-process dll={} clock={}\",\n                    path.display(),\n                    if config.strict_ntp_clock { \"strict-ntp\" } else { \"pinned-windows\" },\n                )])),\n""",
)

solo = "crates/sairplay-msa-solo/src/windows_solo_client.rs"
replace_once(
    solo,
    """impl WindowsMsaSoloClient {\n    pub fn connect(mut config: WindowsMsaSoloConfig) -> Result<Self, SoloConnectError> {\n""",
    """fn raop_strict_ntp_compat(txt: Option<&str>, am: Option<&str>) -> bool {\n    let txt_model = txt.and_then(|txt| {\n        txt.split_whitespace()\n            .find_map(|token| token.strip_prefix(\"model=\"))\n    });\n    txt_model.is_some_and(|model| model.starts_with(\"AppleTV3,\"))\n        || am.is_some_and(|model| model.starts_with(\"AppleTV3,\"))\n}\n\nimpl WindowsMsaSoloClient {\n    pub fn connect(mut config: WindowsMsaSoloConfig) -> Result<Self, SoloConnectError> {\n""",
)
replace_once(
    solo,
    """        config.raop.bind_ip = config.native.control.bind_ip;\n        config.raop.mfi_auth = config.raop.mfi_auth\n""",
    """        config.raop.bind_ip = config.native.control.bind_ip;\n        // AppleTV3-class receivers (including common embedded clones) require\n        // standards-correct absolute NTP on Windows. Keep this selector model-\n        // based rather than port-based so equivalent receivers on other ports\n        // use the same compatibility path. Other RAOP receivers remain on the\n        // hardware-locked pinned-Windows clock module.\n        config.raop.strict_ntp_clock =\n            raop_strict_ntp_compat(config.txt.as_deref(), config.am.as_deref());\n        config.raop.mfi_auth = config.raop.mfi_auth\n""",
)
append_once(
    solo,
    "raop_strict_ntp_compat_selects_appletv3_family",
    r'''
#[cfg(test)]
mod strict_ntp_compat_tests {
    use super::raop_strict_ntp_compat;

    #[test]
    fn raop_strict_ntp_compat_selects_appletv3_family() {
        assert!(raop_strict_ntp_compat(
            Some("features=0x1e527ffff7 model=AppleTV3,1 flags=0x4"),
            None,
        ));
        assert!(raop_strict_ntp_compat(None, Some("AppleTV3,2")));
    }

    #[test]
    fn raop_strict_ntp_compat_does_not_touch_locked_non_appletv3_lanes() {
        assert!(!raop_strict_ntp_compat(None, Some("ShairportSync")));
        assert!(!raop_strict_ntp_compat(
            Some("model=AppleTV5,3 features=0x123"),
            None,
        ));
        assert!(!raop_strict_ntp_compat(
            Some("model=AudioAccessory5,1 features=0x123"),
            None,
        ));
    }
}
''',
)

worker = "crates/sairplay-msa-solo/src/windows_raop_worker.rs"
replace_once(
    worker,
    """        let now_unix_ms = unix_now_ms();\n        let (pinned_windows_ntp_sec, expected_ntp_sec, clock_delta_sec) =\n            pinned_windows_clock_diag(now_unix_ms);\n        let codec = if config.compressed_alac { \"ALAC\" } else { \"ALAC-raw\" };\n        let crypto = if config.encrypt && config.et.contains('1') { \"RSA\" } else { \"clear\" };\n        let wire_diag = format!(\n            \"MSA RAOP DIAG WIRE clock=pinned-windows-crosstools source_ntp_sec={} expected_ntp_sec={} delta={}s et={} md={} codec={} crypto={} mfi_auth={} sample_rate={} bit_depth={} channels={}; observation-only, transport unchanged.\",\n            pinned_windows_ntp_sec,\n            expected_ntp_sec,\n            clock_delta_sec,\n            config.et,\n            config.md,\n            codec,\n            crypto,\n            config.mfi_auth,\n            config.sample_rate,\n            config.bit_depth,\n            config.channels,\n        );\n""",
    """        let now_unix_ms = unix_now_ms();\n        let (pinned_windows_ntp_sec, expected_ntp_sec, clock_delta_sec) =\n            pinned_windows_clock_diag(now_unix_ms);\n        let codec = if config.compressed_alac { \"ALAC\" } else { \"ALAC-raw\" };\n        let crypto = if config.encrypt && config.et.contains('1') { \"RSA\" } else { \"clear\" };\n        // Preserve the existing diagnostic text byte-for-byte for every normal\n        // RAOP receiver. Only the isolated AppleTV3 strict-NTP module reports\n        // the corrected active clock; the legacy delta remains visible for A/B.\n        let wire_diag = if config.strict_ntp_clock {\n            format!(\n                \"MSA RAOP DIAG WIRE clock=strict-ntp-compat source_ntp_sec={} expected_ntp_sec={} delta=0s legacy_source_ntp_sec={} legacy_delta={}s et={} md={} codec={} crypto={} mfi_auth={} sample_rate={} bit_depth={} channels={}; isolated AppleTV3 RAOP compatibility path active.\",\n                expected_ntp_sec,\n                expected_ntp_sec,\n                pinned_windows_ntp_sec,\n                clock_delta_sec,\n                config.et,\n                config.md,\n                codec,\n                crypto,\n                config.mfi_auth,\n                config.sample_rate,\n                config.bit_depth,\n                config.channels,\n            )\n        } else {\n            format!(\n                \"MSA RAOP DIAG WIRE clock=pinned-windows-crosstools source_ntp_sec={} expected_ntp_sec={} delta={}s et={} md={} codec={} crypto={} mfi_auth={} sample_rate={} bit_depth={} channels={}; observation-only, transport unchanged.\",\n                pinned_windows_ntp_sec,\n                expected_ntp_sec,\n                clock_delta_sec,\n                config.et,\n                config.md,\n                codec,\n                crypto,\n                config.mfi_auth,\n                config.sample_rate,\n                config.bit_depth,\n                config.channels,\n            )\n        };\n""",
)

shim_h = r'''#ifndef SAIRPLAY_RAOP_STRICT_CLOCK_SHIM_H
#define SAIRPLAY_RAOP_STRICT_CLOCK_SHIM_H

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>

#ifdef __cplusplus
extern "C" {
#endif

/* crosstools cross_util.c is compiled with its Windows FILETIME call redirected
 * here. Default mode returns the raw FILETIME value exactly, preserving the
 * hardware-locked SOtM path. A separately loaded strict DLL instance can opt in
 * to standards-correct NTP by subtracting the FILETIME->Unix epoch offset. */
void WINAPI sr_clock_GetSystemTimeAsFileTime(LPFILETIME file_time);
__declspec(dllexport) void sr_raop_set_strict_ntp_clock(int enabled);

#ifdef __cplusplus
}
#endif

#endif /* SAIRPLAY_RAOP_STRICT_CLOCK_SHIM_H */
'''
shim_c = r'''#include "strict_clock_shim.h"

#include <stdint.h>

static volatile LONG g_strict_ntp_clock = 0;

__declspec(dllexport) void sr_raop_set_strict_ntp_clock(int enabled)
{
    InterlockedExchange(&g_strict_ntp_clock, enabled ? 1 : 0);
}

void WINAPI sr_clock_GetSystemTimeAsFileTime(LPFILETIME file_time)
{
    FILETIME raw;
    ULARGE_INTEGER ticks;
    const ULONGLONG unix_epoch_filetime = 116444736000000000ULL;

    GetSystemTimeAsFileTime(&raw);
    if (!file_time) return;

    if (InterlockedCompareExchange(&g_strict_ntp_clock, 0, 0) == 0) {
        *file_time = raw;
        return;
    }

    ticks.LowPart = raw.dwLowDateTime;
    ticks.HighPart = raw.dwHighDateTime;
    if (ticks.QuadPart > unix_epoch_filetime) {
        ticks.QuadPart -= unix_epoch_filetime;
    } else {
        ticks.QuadPart = 0;
    }
    file_time->dwLowDateTime = ticks.LowPart;
    file_time->dwHighDateTime = ticks.HighPart;
}
'''
write_new("native/raop-static/strict_clock_shim.h", shim_h)
write_new("native/raop-static/strict_clock_shim.c", shim_c)

windows = ".github/workflows/windows.yml"
replace_once(
    windows,
    """          $alacWrapperInclude = (Resolve-Path \"target/vendor/libcodecs-x64-stage/include/addons\").Path\n\n          $includes = @(\n""",
    """          $alacWrapperInclude = (Resolve-Path \"target/vendor/libcodecs-x64-stage/include/addons\").Path\n          $strictClockHeader = (Resolve-Path \"native/raop-static/strict_clock_shim.h\").Path\n\n          $includes = @(\n""",
)
replace_once(
    windows,
    """          foreach ($source in $sources) {\n            if (-not (Test-Path $source)) { throw \"Pinned libraop client source missing: $source\" }\n            $objPath = Join-Path $objAbs (([IO.Path]::GetFileNameWithoutExtension($source)) + \".obj\")\n            $cmd = \"call `\"$vcvars64`\" && cl.exe /nologo /c /O2 /MD /std:clatest /DNDEBUG /DSSL_STATIC_LIB /DWIN32 /D_CRT_SECURE_NO_WARNINGS /D_WINSOCK_DEPRECATED_NO_WARNINGS /D_CRT_NONSTDC_NO_DEPRECATE /DOPENSSL_SUPPRESS_DEPRECATED $includeArgs /Fo`\"$objPath`\" `\"$source`\"\"\n""",
    """          foreach ($source in $sources) {\n            if (-not (Test-Path $source)) { throw \"Pinned libraop client source missing: $source\" }\n            $objPath = Join-Path $objAbs (([IO.Path]::GetFileNameWithoutExtension($source)) + \".obj\")\n            $sourceExtraArgs = \"\"\n            if ([IO.Path]::GetFileName($source) -eq \"cross_util.c\") {\n              # Redirect only the pinned Windows clock call through our module-\n              # local shim. Default mode returns raw FILETIME exactly; the\n              # separately loaded strict DLL opts into corrected NTP.\n              $sourceExtraArgs = \"/DGetSystemTimeAsFileTime=sr_clock_GetSystemTimeAsFileTime /FI`\"$strictClockHeader`\"\"\n            }\n            $cmd = \"call `\"$vcvars64`\" && cl.exe /nologo /c /O2 /MD /std:clatest /DNDEBUG /DSSL_STATIC_LIB /DWIN32 /D_CRT_SECURE_NO_WARNINGS /D_WINSOCK_DEPRECATED_NO_WARNINGS /D_CRT_NONSTDC_NO_DEPRECATE /DOPENSSL_SUPPRESS_DEPRECATED $sourceExtraArgs $includeArgs /Fo`\"$objPath`\" `\"$source`\"\"\n""",
)
replace_once(
    windows,
    """          $bridge = (Resolve-Path \"native/raop-static/raop_bridge.c\").Path\n          $bridgeInclude = (Resolve-Path \"native/raop-static\").Path\n          $bridgeObj = Join-Path $stageAbs \"raop_bridge.obj\"\n          $logGlobals = Join-Path $stageAbs \"raop_log_globals.c\"\n          $logGlobalsObj = Join-Path $stageAbs \"raop_log_globals.obj\"\n          $bridgeDll = Join-Path $stageAbs \"sairplay-raop.dll\"\n""",
    """          $bridge = (Resolve-Path \"native/raop-static/raop_bridge.c\").Path\n          $clockShim = (Resolve-Path \"native/raop-static/strict_clock_shim.c\").Path\n          $bridgeInclude = (Resolve-Path \"native/raop-static\").Path\n          $bridgeObj = Join-Path $stageAbs \"raop_bridge.obj\"\n          $clockShimObj = Join-Path $stageAbs \"strict_clock_shim.obj\"\n          $logGlobals = Join-Path $stageAbs \"raop_log_globals.c\"\n          $logGlobalsObj = Join-Path $stageAbs \"raop_log_globals.obj\"\n          $bridgeDll = Join-Path $stageAbs \"sairplay-raop.dll\"\n          $strictBridgeDll = Join-Path $stageAbs \"sairplay-raop-strict.dll\"\n""",
)
replace_once(
    windows,
    """          $compileGlobals = \"call `\"$vcvars64`\" && cl.exe /nologo /c /O2 /MD /std:clatest /DNDEBUG /DWIN32 $includeArgs /Fo`\"$logGlobalsObj`\" `\"$logGlobals`\"\"\n""",
    """          $compileClockShim = \"call `\"$vcvars64`\" && cl.exe /nologo /c /O2 /MD /std:clatest /DNDEBUG /DWIN32 $includeArgs /Fo`\"$clockShimObj`\" `\"$clockShim`\"\"\n          & cmd.exe /d /s /c $compileClockShim\n          if ($LASTEXITCODE -ne 0) { throw \"Strict NTP clock shim x64 compile failed\" }\n\n          $compileGlobals = \"call `\"$vcvars64`\" && cl.exe /nologo /c /O2 /MD /std:clatest /DNDEBUG /DWIN32 $includeArgs /Fo`\"$logGlobalsObj`\" `\"$logGlobals`\"\"\n""",
)
replace_once(
    windows,
    """          $link = \"call `\"$vcvars64`\" && link.exe /nologo /DLL /OUT:`\"$bridgeDll`\" `\"$bridgeObj`\" `\"$logGlobalsObj`\" `\"$libraop/libraop-client-x64.lib`\" `\"$alac/libalac-wrapper.lib`\" `\"$alac/libalac.lib`\" `\"$pthread/libpthread-static.lib`\" `\"$openssl/libcrypto.lib`\" `\"$openssl/libssl.lib`\" ws2_32.lib wsock32.lib iphlpapi.lib $exportArgs\"\n""",
    """          $link = \"call `\"$vcvars64`\" && link.exe /nologo /DLL /OUT:`\"$bridgeDll`\" `\"$bridgeObj`\" `\"$clockShimObj`\" `\"$logGlobalsObj`\" `\"$libraop/libraop-client-x64.lib`\" `\"$alac/libalac-wrapper.lib`\" `\"$alac/libalac.lib`\" `\"$pthread/libpthread-static.lib`\" `\"$openssl/libcrypto.lib`\" `\"$openssl/libssl.lib`\" ws2_32.lib wsock32.lib iphlpapi.lib $exportArgs\"\n""",
)
replace_once(
    windows,
    """          if (-not (Test-Path $bridgeDll)) { throw \"sairplay-raop.dll missing\" }\n\n          Get-FileHash $bridgeDll -Algorithm SHA256\n          Get-ChildItem $stageAbs\n""",
    """          if (-not (Test-Path $bridgeDll)) { throw \"sairplay-raop.dll missing\" }\n          # Loading the same binary under a second filename gives Windows a\n          # separate module instance and therefore a separate clock-mode global.\n          # Normal SOtM/RAOP sessions use sairplay-raop.dll (mode 0); only the\n          # AppleTV3 compatibility selector loads this strict copy and enables 1.\n          Copy-Item $bridgeDll $strictBridgeDll -Force\n          if (-not (Test-Path $strictBridgeDll)) { throw \"sairplay-raop-strict.dll missing\" }\n\n          Get-FileHash $bridgeDll -Algorithm SHA256\n          Get-FileHash $strictBridgeDll -Algorithm SHA256\n          Get-ChildItem $stageAbs\n""",
)
replace_once(
    windows,
    """          Copy-Item \"target/vendor/sairplay-raop-x64-stage/sairplay-raop.dll\" \"$out/sairplay-raop.dll\" -Force\n          Copy-Item \"target/vendor/libopenssl-x64-stage/libcrypto-1_1-x64.dll\" \"$out/libcrypto-1_1-x64.dll\" -Force\n""",
    """          Copy-Item \"target/vendor/sairplay-raop-x64-stage/sairplay-raop.dll\" \"$out/sairplay-raop.dll\" -Force\n          Copy-Item \"target/vendor/sairplay-raop-x64-stage/sairplay-raop-strict.dll\" \"$out/sairplay-raop-strict.dll\" -Force\n          Copy-Item \"target/vendor/libopenssl-x64-stage/libcrypto-1_1-x64.dll\" \"$out/libcrypto-1_1-x64.dll\" -Force\n""",
)
replace_once(
    windows,
    """          Get-FileHash \"$out/sairplay-raop.dll\" -Algorithm SHA256\n          Get-FileHash \"$out/libcrypto-1_1-x64.dll\" -Algorithm SHA256\n""",
    """          Get-FileHash \"$out/sairplay-raop.dll\" -Algorithm SHA256\n          Get-FileHash \"$out/sairplay-raop-strict.dll\" -Algorithm SHA256\n          Get-FileHash \"$out/libcrypto-1_1-x64.dll\" -Algorithm SHA256\n""",
)
