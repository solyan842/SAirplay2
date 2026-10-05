from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one match, got {count}: {old[:120]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


# Narrow strict-NTP from broad AppleTV3 family to the exact MiTV profile
# observed on hardware. Port is intentionally not part of the selector.
solo = "crates/sairplay-msa-solo/src/windows_solo_client.rs"
replace_once(
    solo,
    """use crate::route::{\n    apple_model, buffered_route, follow_receiver_clock, resolve_route_from_txt,\n    Flow, ProtocolPreference, RouteDecision, Timing as RouteTiming,\n};\n""",
    """use crate::route::{\n    apple_model, buffered_route, follow_receiver_clock, resolve_route_from_txt,\n    txt_features, txt_flags, Flow, ProtocolPreference, RouteDecision, Timing as RouteTiming,\n};\n""",
)
replace_once(
    solo,
    """fn raop_strict_ntp_compat(txt: Option<&str>, am: Option<&str>) -> bool {\n    let txt_model = txt.and_then(|txt| {\n        txt.split_whitespace()\n            .find_map(|token| token.strip_prefix(\"model=\"))\n    });\n    txt_model.is_some_and(|model| model.starts_with(\"AppleTV3,\"))\n        || am.is_some_and(|model| model.starts_with(\"AppleTV3,\"))\n}\n""",
    """fn raop_strict_ntp_compat(txt: Option<&str>, am: Option<&str>) -> bool {\n    const MITV_FEATURES: u64 = 0x0000001e527ffff7;\n    const MITV_FLAGS: u64 = 0x4;\n\n    let txt_model = txt.and_then(|txt| {\n        txt.split_whitespace()\n            .find_map(|token| token.strip_prefix(\"model=\"))\n    });\n    let model_match = txt_model.is_some_and(|model| model == \"AppleTV3,1\")\n        || am.is_some_and(|model| model == \"AppleTV3,1\");\n\n    model_match\n        && txt_features(txt) == MITV_FEATURES\n        && txt_flags(txt) == MITV_FLAGS\n}\n""",
)
replace_once(
    solo,
    """#[cfg(test)]\nmod strict_ntp_compat_tests {\n    use super::raop_strict_ntp_compat;\n\n    #[test]\n    fn raop_strict_ntp_compat_selects_appletv3_family() {\n        assert!(raop_strict_ntp_compat(\n            Some(\"features=0x1e527ffff7 model=AppleTV3,1 flags=0x4\"),\n            None,\n        ));\n        assert!(raop_strict_ntp_compat(None, Some(\"AppleTV3,2\")));\n    }\n\n    #[test]\n    fn raop_strict_ntp_compat_does_not_touch_locked_non_appletv3_lanes() {\n        assert!(!raop_strict_ntp_compat(None, Some(\"ShairportSync\")));\n        assert!(!raop_strict_ntp_compat(\n            Some(\"model=AppleTV5,3 features=0x123\"),\n            None,\n        ));\n        assert!(!raop_strict_ntp_compat(\n            Some(\"model=AudioAccessory5,1 features=0x123\"),\n            None,\n        ));\n    }\n}\n""",
    """#[cfg(test)]\nmod strict_ntp_compat_tests {\n    use super::raop_strict_ntp_compat;\n\n    #[test]\n    fn raop_strict_ntp_compat_selects_exact_mitv_profile_without_port() {\n        assert!(raop_strict_ntp_compat(\n            Some(\"features=0x1e527ffff7 model=AppleTV3,1 flags=0x4\"),\n            None,\n        ));\n    }\n\n    #[test]\n    fn raop_strict_ntp_compat_does_not_touch_other_appletv3_or_locked_lanes() {\n        assert!(!raop_strict_ntp_compat(\n            Some(\"features=0x1e527ffff7 model=AppleTV3,2 flags=0x4\"),\n            Some(\"AppleTV3,2\"),\n        ));\n        assert!(!raop_strict_ntp_compat(\n            Some(\"features=0x123 model=AppleTV3,1 flags=0x4\"),\n            Some(\"AppleTV3,1\"),\n        ));\n        assert!(!raop_strict_ntp_compat(None, Some(\"ShairportSync\")));\n        assert!(!raop_strict_ntp_compat(\n            Some(\"model=AppleTV5,3 features=0x123 flags=0\"),\n            None,\n        ));\n        assert!(!raop_strict_ntp_compat(\n            Some(\"model=AudioAccessory5,1 features=0x123 flags=0\"),\n            None,\n        ));\n    }\n}\n""",
)

# Expose whether this DLL instance is the strict module so only that path
# enables the pinned-libraop wire counters.
shim_h = "native/raop-static/strict_clock_shim.h"
replace_once(
    shim_h,
    """void WINAPI sr_clock_GetSystemTimeAsFileTime(LPFILETIME file_time);\n__declspec(dllexport) void sr_raop_set_strict_ntp_clock(int enabled);\n""",
    """void WINAPI sr_clock_GetSystemTimeAsFileTime(LPFILETIME file_time);\n__declspec(dllexport) void sr_raop_set_strict_ntp_clock(int enabled);\nint sr_raop_strict_ntp_clock_enabled(void);\n""",
)
shim_c = "native/raop-static/strict_clock_shim.c"
replace_once(
    shim_c,
    """__declspec(dllexport) void sr_raop_set_strict_ntp_clock(int enabled)\n{\n    InterlockedExchange(&g_strict_ntp_clock, enabled ? 1 : 0);\n}\n\nvoid WINAPI sr_clock_GetSystemTimeAsFileTime(LPFILETIME file_time)\n""",
    """__declspec(dllexport) void sr_raop_set_strict_ntp_clock(int enabled)\n{\n    InterlockedExchange(&g_strict_ntp_clock, enabled ? 1 : 0);\n}\n\nint sr_raop_strict_ntp_clock_enabled(void)\n{\n    return InterlockedCompareExchange(&g_strict_ntp_clock, 0, 0) != 0;\n}\n\nvoid WINAPI sr_clock_GetSystemTimeAsFileTime(LPFILETIME file_time)\n""",
)

# Bridge ABI for observation-only wire telemetry.
bridge_h = "native/raop-static/raop_bridge.h"
replace_once(
    bridge_h,
    """typedef struct sr_raop_ready {\n    uint32_t latency_frames;\n    uint32_t sample_rate;\n    uint16_t bit_depth;\n    uint16_t channels;\n    uint32_t open_error_stage;\n} sr_raop_ready;\n""",
    """typedef struct sr_raop_ready {\n    uint32_t latency_frames;\n    uint32_t sample_rate;\n    uint16_t bit_depth;\n    uint16_t channels;\n    uint32_t open_error_stage;\n} sr_raop_ready;\n\ntypedef struct sr_raop_wire_diag {\n    uint16_t audio_lport, audio_rport;\n    uint16_t control_lport, control_rport;\n    uint16_t timing_lport, timing_rport;\n    uint32_t state;\n    uint32_t seq_number;\n    uint32_t sane_ctrl, sane_time;\n    uint32_t sane_audio_avail, sane_audio_select, sane_audio_send;\n    uint64_t audio_send_ok, audio_send_fail;\n    uint64_t sync_send_ok, sync_send_fail;\n    uint64_t timing_requests, timing_responses, timing_response_fail;\n    uint64_t control_requests;\n    uint64_t retransmit;\n    uint32_t first_audio_timestamp, last_audio_timestamp;\n    uint16_t first_audio_seq, last_audio_seq;\n} sr_raop_wire_diag;\n""",
)
replace_once(
    bridge_h,
    """uint64_t sr_raop_head_audible_ms(sr_raop_handle *handle);\n""",
    """uint64_t sr_raop_head_audible_ms(sr_raop_handle *handle);\nint sr_raop_diag_snapshot(sr_raop_handle *handle, sr_raop_wire_diag *out);\n""",
)

bridge_c = "native/raop-static/raop_bridge.c"
replace_once(
    bridge_c,
    """#include \"raop_bridge.h\"\n\n#include <limits.h>\n""",
    """#include \"raop_bridge.h\"\n#include \"strict_clock_shim.h\"\n\n#include <limits.h>\n""",
)
replace_once(
    bridge_c,
    """    if (!handle->client) {\n        sr_set_open_error(ready, SR_RAOP_OPEN_RAOPCL_CREATE);\n        free(handle->packed24);\n        DeleteCriticalSection(&handle->lock);\n        free(handle);\n        return NULL;\n    }\n\n    if (!raopcl_connect(handle->client, player, config->port, config->volume > 0)) {\n""",
    """    if (!handle->client) {\n        sr_set_open_error(ready, SR_RAOP_OPEN_RAOPCL_CREATE);\n        free(handle->packed24);\n        DeleteCriticalSection(&handle->lock);\n        free(handle);\n        return NULL;\n    }\n\n    /* Wire counters are enabled only in the isolated strict-NTP DLL instance.\n     * Normal SOtM/RAOP sessions keep this diagnostic path dormant. */\n    raopcl_diag_enable(handle->client, sr_raop_strict_ntp_clock_enabled() != 0);\n\n    if (!raopcl_connect(handle->client, player, config->port, config->volume > 0)) {\n""",
)
replace_once(
    bridge_c,
    """uint64_t sr_raop_head_audible_ms(sr_raop_handle *handle)\n{\n    uint64_t value = 0;\n    if (!handle) return 0;\n    EnterCriticalSection(&handle->lock);\n    value = handle->head_audible_ms;\n    LeaveCriticalSection(&handle->lock);\n    return value;\n}\n""",
    """uint64_t sr_raop_head_audible_ms(sr_raop_handle *handle)\n{\n    uint64_t value = 0;\n    if (!handle) return 0;\n    EnterCriticalSection(&handle->lock);\n    value = handle->head_audible_ms;\n    LeaveCriticalSection(&handle->lock);\n    return value;\n}\n\nint sr_raop_diag_snapshot(sr_raop_handle *handle, sr_raop_wire_diag *out)\n{\n    raop_diag_snapshot_t raw;\n    int ok = 0;\n    if (!handle || !out) return 0;\n    memset(out, 0, sizeof(*out));\n\n    EnterCriticalSection(&handle->lock);\n    if (handle->client && raopcl_diag_snapshot(handle->client, &raw)) {\n        out->audio_lport = raw.audio_lport;\n        out->audio_rport = raw.audio_rport;\n        out->control_lport = raw.control_lport;\n        out->control_rport = raw.control_rport;\n        out->timing_lport = raw.timing_lport;\n        out->timing_rport = raw.timing_rport;\n        out->state = raw.state;\n        out->seq_number = raw.seq_number;\n        out->sane_ctrl = raw.sane_ctrl;\n        out->sane_time = raw.sane_time;\n        out->sane_audio_avail = raw.sane_audio_avail;\n        out->sane_audio_select = raw.sane_audio_select;\n        out->sane_audio_send = raw.sane_audio_send;\n        out->audio_send_ok = raw.audio_send_ok;\n        out->audio_send_fail = raw.audio_send_fail;\n        out->sync_send_ok = raw.sync_send_ok;\n        out->sync_send_fail = raw.sync_send_fail;\n        out->timing_requests = raw.timing_requests;\n        out->timing_responses = raw.timing_responses;\n        out->timing_response_fail = raw.timing_response_fail;\n        out->control_requests = raw.control_requests;\n        out->retransmit = raw.retransmit;\n        out->first_audio_timestamp = raw.first_audio_timestamp;\n        out->last_audio_timestamp = raw.last_audio_timestamp;\n        out->first_audio_seq = raw.first_audio_seq;\n        out->last_audio_seq = raw.last_audio_seq;\n        ok = 1;\n    }\n    LeaveCriticalSection(&handle->lock);\n    return ok;\n}\n""",
)

# Rust FFI and formatting. Normal DLL returns no snapshot because diagnostics are
# disabled there, so locked RAOP logs are untouched.
session = "crates/sairplay-msa-solo/src/windows_raop_session.rs"
replace_once(
    session,
    """#[repr(C)]\n#[derive(Default)]\nstruct SrRaopReady {\n    latency_frames: u32,\n    sample_rate: u32,\n    bit_depth: u16,\n    channels: u16,\n    open_error_stage: u32,\n}\n""",
    """#[repr(C)]\n#[derive(Default)]\nstruct SrRaopReady {\n    latency_frames: u32,\n    sample_rate: u32,\n    bit_depth: u16,\n    channels: u16,\n    open_error_stage: u32,\n}\n\n#[repr(C)]\n#[derive(Default)]\nstruct SrRaopWireDiag {\n    audio_lport: u16, audio_rport: u16,\n    control_lport: u16, control_rport: u16,\n    timing_lport: u16, timing_rport: u16,\n    state: u32,\n    seq_number: u32,\n    sane_ctrl: u32, sane_time: u32,\n    sane_audio_avail: u32, sane_audio_select: u32, sane_audio_send: u32,\n    audio_send_ok: u64, audio_send_fail: u64,\n    sync_send_ok: u64, sync_send_fail: u64,\n    timing_requests: u64, timing_responses: u64, timing_response_fail: u64,\n    control_requests: u64,\n    retransmit: u64,\n    first_audio_timestamp: u32, last_audio_timestamp: u32,\n    first_audio_seq: u16, last_audio_seq: u16,\n}\n""",
)
replace_once(
    session,
    """type HeadFn = unsafe extern \"C\" fn(*mut c_void) -> u64;\ntype ClockModeFn = unsafe extern \"C\" fn(i32);\n""",
    """type HeadFn = unsafe extern \"C\" fn(*mut c_void) -> u64;\ntype WireDiagFn = unsafe extern \"C\" fn(*mut c_void, *mut SrRaopWireDiag) -> i32;\ntype ClockModeFn = unsafe extern \"C\" fn(i32);\n""",
)
replace_once(
    session,
    """    write_packet: WriteFn,\n    head_audible_ms: HeadFn,\n}\n""",
    """    write_packet: WriteFn,\n    head_audible_ms: HeadFn,\n    wire_diag: WireDiagFn,\n}\n""",
)
replace_once(
    session,
    """        let write_packet = symbol!(\"sr_raop_write_packet\", WriteFn);\n        let head_audible_ms = symbol!(\"sr_raop_head_audible_ms\", HeadFn);\n        Ok(Self {\n""",
    """        let write_packet = symbol!(\"sr_raop_write_packet\", WriteFn);\n        let head_audible_ms = symbol!(\"sr_raop_head_audible_ms\", HeadFn);\n        let wire_diag = symbol!(\"sr_raop_diag_snapshot\", WireDiagFn);\n        Ok(Self {\n""",
)
replace_once(
    session,
    """            set_progress, set_metadata, set_artwork, write_packet, head_audible_ms,\n        })\n""",
    """            set_progress, set_metadata, set_artwork, write_packet, head_audible_ms, wire_diag,\n        })\n""",
)
replace_once(
    session,
    """    fn head_audible_ms(&self) -> u64 {\n        self.with_handle(|p| unsafe { (self.api.head_audible_ms)(p) }).unwrap_or(0)\n    }\n}\n""",
    """    fn head_audible_ms(&self) -> u64 {\n        self.with_handle(|p| unsafe { (self.api.head_audible_ms)(p) }).unwrap_or(0)\n    }\n\n    fn wire_diagnostic_line(&self) -> Option<String> {\n        let mut diag = SrRaopWireDiag::default();\n        let ok = self\n            .with_handle(|p| unsafe { (self.api.wire_diag)(p, &mut diag) })\n            .ok()?;\n        if ok == 0 { return None; }\n        Some(format!(\n            \"MSA RAOP DIAG WIRE-UDP ports=audio:{}->{} control:{}->{} timing:{}->{} state={} seq={} audio_ok={} audio_fail={} sync_ok={} sync_fail={} timing_req={} timing_rsp={} timing_rsp_fail={} control_req={} retransmit={} first_seq={} first_ts={} last_seq={} last_ts={} sane=ctrl:{} time:{} audio_avail:{} audio_select:{} audio_send:{}\",\n            diag.audio_lport, diag.audio_rport,\n            diag.control_lport, diag.control_rport,\n            diag.timing_lport, diag.timing_rport,\n            diag.state, diag.seq_number,\n            diag.audio_send_ok, diag.audio_send_fail,\n            diag.sync_send_ok, diag.sync_send_fail,\n            diag.timing_requests, diag.timing_responses, diag.timing_response_fail,\n            diag.control_requests, diag.retransmit,\n            diag.first_audio_seq, diag.first_audio_timestamp,\n            diag.last_audio_seq, diag.last_audio_timestamp,\n            diag.sane_ctrl, diag.sane_time, diag.sane_audio_avail,\n            diag.sane_audio_select, diag.sane_audio_send,\n        ))\n    }\n}\n""",
)
replace_once(
    session,
    """    pub fn head_audible_unix_ms(&self) -> u64 {\n        if let Some(core) = self.inproc.as_ref() { core.head_audible_ms() }\n        else { self.head_audible_ms.load(Ordering::SeqCst) }\n    }\n\n    pub fn commit_start(&mut self, requested_unix_ms: u64) -> Result<StartResolution, MsaRaopError> {\n""",
    """    pub fn head_audible_unix_ms(&self) -> u64 {\n        if let Some(core) = self.inproc.as_ref() { core.head_audible_ms() }\n        else { self.head_audible_ms.load(Ordering::SeqCst) }\n    }\n    pub fn wire_diagnostic_line(&self) -> Option<String> {\n        self.inproc.as_ref()?.wire_diagnostic_line()\n    }\n\n    pub fn commit_start(&mut self, requested_unix_ms: u64) -> Result<StartResolution, MsaRaopError> {\n""",
)

worker = "crates/sairplay-msa-solo/src/windows_raop_worker.rs"
replace_once(
    worker,
    """                    if last_summary.elapsed() >= RAOP_DIAG_SUMMARY_INTERVAL {\n                        let queue_ms = pending_before_frames.saturating_mul(1000) / sample_rate_w;\n                        let min_frames = if min_queue_frames == usize::MAX { 0 } else { min_queue_frames };\n                        let min_ms = min_frames.saturating_mul(1000) / sample_rate_w;\n                        let max_ms = max_queue_frames.saturating_mul(1000) / sample_rate_w;\n                        if let Ok(mut events) = events_w.lock() {\n""",
    """                    if last_summary.elapsed() >= RAOP_DIAG_SUMMARY_INTERVAL {\n                        let queue_ms = pending_before_frames.saturating_mul(1000) / sample_rate_w;\n                        let min_frames = if min_queue_frames == usize::MAX { 0 } else { min_queue_frames };\n                        let min_ms = min_frames.saturating_mul(1000) / sample_rate_w;\n                        let max_ms = max_queue_frames.saturating_mul(1000) / sample_rate_w;\n                        let wire_diag = session_w\n                            .try_lock()\n                            .ok()\n                            .and_then(|session| session.wire_diagnostic_line());\n                        if let Ok(mut events) = events_w.lock() {\n""",
)
replace_once(
    worker,
    """                            events.push(format!(\n                                \"MSA RAOP DIAG 10s sent_total={} queue_now={}f/{}ms queue_min={}f/{}ms queue_max={}f/{}ms starvation_total={} max_empty={}ms slow_write_total={} max_write={}ms max_send_gap={}ms capture_idle={}ms capture_gen={} source_present={} reservoir_primed={} reservoir_target={}f/{}ms head_ahead_ms={:?}.\",\n                                sent_packets_total,\n                                pending_before_frames,\n                                queue_ms,\n                                min_frames,\n                                min_ms,\n                                max_queue_frames,\n                                max_ms,\n                                starvation_events_total,\n                                max_starvation_ms,\n                                slow_writes_total,\n                                max_write_ms,\n                                max_send_gap_ms,\n                                last_capture_progress.elapsed().as_millis(),\n                                last_capture_generation,\n                                hub_w.source_present(),\n                                reservoir_primed,\n                                RAOP_RESERVOIR_FRAMES,\n                                reservoir_ms_w,\n                                diagnostic_head_ahead_ms(&session_w),\n                            ));\n                        }\n""",
    """                            events.push(format!(\n                                \"MSA RAOP DIAG 10s sent_total={} queue_now={}f/{}ms queue_min={}f/{}ms queue_max={}f/{}ms starvation_total={} max_empty={}ms slow_write_total={} max_write={}ms max_send_gap={}ms capture_idle={}ms capture_gen={} source_present={} reservoir_primed={} reservoir_target={}f/{}ms head_ahead_ms={:?}.\",\n                                sent_packets_total,\n                                pending_before_frames,\n                                queue_ms,\n                                min_frames,\n                                min_ms,\n                                max_queue_frames,\n                                max_ms,\n                                starvation_events_total,\n                                max_starvation_ms,\n                                slow_writes_total,\n                                max_write_ms,\n                                max_send_gap_ms,\n                                last_capture_progress.elapsed().as_millis(),\n                                last_capture_generation,\n                                hub_w.source_present(),\n                                reservoir_primed,\n                                RAOP_RESERVOIR_FRAMES,\n                                reservoir_ms_w,\n                                diagnostic_head_ahead_ms(&session_w),\n                            ));\n                            if let Some(line) = wire_diag { events.push(line); }\n                        }\n""",
)

# Build-time hook for exact pinned libraop diagnostics + bridge export.
workflow = ".github/workflows/windows.yml"
replace_once(
    workflow,
    """          $dmapHead = (git -C \"$src/dmap-parser\" rev-parse HEAD).Trim()\n          if ($dmapHead -ne $dmapPin) { throw \"Pinned dmap-parser mismatch: $dmapHead\" }\n\n          $vswhere = \"${env:ProgramFiles(x86)}\\Microsoft Visual Studio\\Installer\\vswhere.exe\"\n""",
    """          $dmapHead = (git -C \"$src/dmap-parser\" rev-parse HEAD).Trim()\n          if ($dmapHead -ne $dmapPin) { throw \"Pinned dmap-parser mismatch: $dmapHead\" }\n          python scripts/patch_pinned_raop_wire_diag.py $src\n          if ($LASTEXITCODE -ne 0) { throw \"Pinned RAOP wire diagnostic patch failed\" }\n\n          $vswhere = \"${env:ProgramFiles(x86)}\\Microsoft Visual Studio\\Installer\\vswhere.exe\"\n""",
)
replace_once(
    workflow,
    """            \"sr_raop_set_artwork\", \"sr_raop_write_packet\", \"sr_raop_head_audible_ms\"\n""",
    """            \"sr_raop_set_artwork\", \"sr_raop_write_packet\", \"sr_raop_head_audible_ms\",\n            \"sr_raop_diag_snapshot\"\n""",
)

print("applied RAOP wire diagnostic source patch")
