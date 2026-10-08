from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, got {count}: {old[:160]!r}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


def append_once(path: Path, marker: str, text_to_append: str) -> None:
    text = path.read_text(encoding="utf-8")
    if marker in text:
        raise SystemExit(f"{path}: marker already present: {marker}")
    path.write_text(text.rstrip() + "\n\n" + text_to_append.strip() + "\n", encoding="utf-8")


workflow = ROOT / ".github" / "workflows" / "windows.yml"
bridge_h = ROOT / "native" / "raop-static" / "raop_bridge.h"
bridge_c = ROOT / "native" / "raop-static" / "raop_bridge.c"
session = ROOT / "crates" / "sairplay-msa-solo" / "src" / "windows_raop_session.rs"
worker = ROOT / "crates" / "sairplay-msa-solo" / "src" / "windows_raop_worker_base.rs"

# Build-copy only: patch the exact pinned libraop source after checkout and
# submodule initialization. No pin, codec, timing, pacing or transport choice
# is changed.
replace_once(
    workflow,
    """          git -C $src checkout $pin\n          git -C $src submodule update --init crosstools dmap-parser\n          $dmapHead = (git -C \"$src/dmap-parser\" rev-parse HEAD).Trim()\n""",
    """          git -C $src checkout $pin\n          git -C $src submodule update --init crosstools dmap-parser\n          python scripts/patch_pinned_raop_wire_diag.py $src\n          $dmapHead = (git -C \"$src/dmap-parser\" rev-parse HEAD).Trim()\n""",
)

# Bridge ABI: expose a read-only snapshot from the pinned libraop counters.
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

# Enable counters only for the known Xiaomi/HappyCast 52266 diagnostic target.
# All other RAOP receivers (including hardware-locked SOtM) leave the counters
# dormant; this call changes no transport decision.
replace_once(
    bridge_c,
    """    if (!handle->client) {\n        sr_set_open_error(ready, SR_RAOP_OPEN_RAOPCL_CREATE);\n        free(handle->packed24);\n        DeleteCriticalSection(&handle->lock);\n        free(handle);\n        return NULL;\n    }\n\n    if (!raopcl_connect(handle->client, player, config->port, config->volume > 0)) {\n""",
    """    if (!handle->client) {\n        sr_set_open_error(ready, SR_RAOP_OPEN_RAOPCL_CREATE);\n        free(handle->packed24);\n        DeleteCriticalSection(&handle->lock);\n        free(handle);\n        return NULL;\n    }\n\n    raopcl_diag_enable(handle->client, config->port == SR_HAPPYCAST_DIAG_PORT);\n\n    if (!raopcl_connect(handle->client, player, config->port, config->volume > 0)) {\n""",
)
append_once(
    bridge_c,
    "int sr_raop_diag_snapshot(sr_raop_handle *handle",
    r'''
int sr_raop_diag_snapshot(sr_raop_handle *handle, sr_raop_wire_diag *out)
{
    raop_diag_snapshot_t raw;
    int ok = 0;
    if (!handle || !out) return 0;
    memset(out, 0, sizeof(*out));

    EnterCriticalSection(&handle->lock);
    if (handle->client && raopcl_diag_snapshot(handle->client, &raw)) {
        out->audio_lport = raw.audio_lport;
        out->audio_rport = raw.audio_rport;
        out->control_lport = raw.control_lport;
        out->control_rport = raw.control_rport;
        out->timing_lport = raw.timing_lport;
        out->timing_rport = raw.timing_rport;
        out->state = raw.state;
        out->seq_number = raw.seq_number;
        out->sane_ctrl = raw.sane_ctrl;
        out->sane_time = raw.sane_time;
        out->sane_audio_avail = raw.sane_audio_avail;
        out->sane_audio_select = raw.sane_audio_select;
        out->sane_audio_send = raw.sane_audio_send;
        out->audio_send_ok = raw.audio_send_ok;
        out->audio_send_fail = raw.audio_send_fail;
        out->sync_send_ok = raw.sync_send_ok;
        out->sync_send_fail = raw.sync_send_fail;
        out->timing_requests = raw.timing_requests;
        out->timing_responses = raw.timing_responses;
        out->timing_response_fail = raw.timing_response_fail;
        out->control_requests = raw.control_requests;
        out->retransmit = raw.retransmit;
        out->first_audio_timestamp = raw.first_audio_timestamp;
        out->last_audio_timestamp = raw.last_audio_timestamp;
        out->first_audio_seq = raw.first_audio_seq;
        out->last_audio_seq = raw.last_audio_seq;
        ok = 1;
    }
    LeaveCriticalSection(&handle->lock);
    return ok;
}
''',
)

# Rust FFI: read-only diagnostic struct/symbol and one formatted log line.
replace_once(
    session,
    """#[repr(C)]\n#[derive(Default)]\nstruct SrRaopReady {\n    latency_frames: u32,\n    sample_rate: u32,\n    bit_depth: u16,\n    channels: u16,\n    open_error_stage: u32,\n}\n""",
    """#[repr(C)]\n#[derive(Default)]\nstruct SrRaopReady {\n    latency_frames: u32,\n    sample_rate: u32,\n    bit_depth: u16,\n    channels: u16,\n    open_error_stage: u32,\n}\n\n#[repr(C)]\n#[derive(Default)]\nstruct SrRaopWireDiag {\n    audio_lport: u16, audio_rport: u16,\n    control_lport: u16, control_rport: u16,\n    timing_lport: u16, timing_rport: u16,\n    state: u32,\n    seq_number: u32,\n    sane_ctrl: u32, sane_time: u32,\n    sane_audio_avail: u32, sane_audio_select: u32, sane_audio_send: u32,\n    audio_send_ok: u64, audio_send_fail: u64,\n    sync_send_ok: u64, sync_send_fail: u64,\n    timing_requests: u64, timing_responses: u64, timing_response_fail: u64,\n    control_requests: u64,\n    retransmit: u64,\n    first_audio_timestamp: u32, last_audio_timestamp: u32,\n    first_audio_seq: u16, last_audio_seq: u16,\n}\n""",
)
replace_once(
    session,
    """type HeadFn = unsafe extern \"C\" fn(*mut c_void) -> u64;\n""",
    """type HeadFn = unsafe extern \"C\" fn(*mut c_void) -> u64;\ntype WireDiagFn = unsafe extern \"C\" fn(*mut c_void, *mut SrRaopWireDiag) -> i32;\n""",
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
    """    pub fn head_audible_unix_ms(&self) -> u64 {\n        if let Some(core) = self.inproc.as_ref() { core.head_audible_ms() }\n        else { self.head_audible_ms.load(Ordering::SeqCst) }\n    }\n\n    pub fn commit_start""",
    """    pub fn head_audible_unix_ms(&self) -> u64 {\n        if let Some(core) = self.inproc.as_ref() { core.head_audible_ms() }\n        else { self.head_audible_ms.load(Ordering::SeqCst) }\n    }\n    pub fn wire_diagnostic_line(&self) -> Option<String> {\n        self.inproc.as_ref()?.wire_diagnostic_line()\n    }\n\n    pub fn commit_start""",
)

# Emit a current wire snapshot next to the existing 10-second RAOP diagnostic.
replace_once(
    worker,
    """                        let max_ms = max_queue_frames.saturating_mul(1000) / sample_rate_w;\n                        if let Ok(mut events) = events_w.lock() {\n""",
    """                        let max_ms = max_queue_frames.saturating_mul(1000) / sample_rate_w;\n                        let wire_diag = session_w\n                            .try_lock()\n                            .ok()\n                            .and_then(|session| session.wire_diagnostic_line());\n                        if let Ok(mut events) = events_w.lock() {\n""",
)
replace_once(
    worker,
    """                                diagnostic_head_ahead_ms(&session_w),\n                            ));\n                        }\n                        last_summary = Instant::now();\n""",
    """                                diagnostic_head_ahead_ms(&session_w),\n                            ));\n                            if let Some(line) = wire_diag { events.push(line); }\n                        }\n                        last_summary = Instant::now();\n""",
)

# Guardrails: this diagnostic port must not re-introduce the old strict-NTP
# experiment or any transport selection changes.
for path in (bridge_h, bridge_c, session, worker):
    text = path.read_text(encoding="utf-8")
    if "strict_ntp" in text.lower() or "strict ntp" in text.lower():
        raise SystemExit(f"{path}: strict-NTP text unexpectedly present after pure diagnostic patch")

print("applied pure MiTV RAOP wire diagnostics; no transport behavior changed")
