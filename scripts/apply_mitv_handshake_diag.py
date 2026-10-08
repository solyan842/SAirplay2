from __future__ import annotations

from pathlib import Path
import argparse


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, got {count}: {old[:160]!r}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


def patch_libraop(root: Path) -> None:
    rtsp_h = root / "src" / "rtsp_client.h"
    rtsp_c = root / "src" / "rtsp_client.c"
    raop_h = root / "src" / "raop_client.h"
    raop_c = root / "src" / "raop_client.c"
    for path in (rtsp_h, rtsp_c, raop_h, raop_c):
        if not path.is_file():
            raise SystemExit(f"missing pinned libraop source: {path}")

    # Capture the real RTSP status parsed by upstream exec_request(). No request
    # construction, timeout, success rule or transport behavior is changed.
    replace_once(
        rtsp_h,
        "bool rtspcl_feedback(struct rtspcl_s *p);\n",
        "bool rtspcl_feedback(struct rtspcl_s *p);\nint rtspcl_last_status(struct rtspcl_s *p);\n",
    )
    replace_once(
        rtsp_c,
        "    int cseq;\n    key_data_t exthds[MAX_KD];\n",
        "    int cseq;\n    int last_status;\n    key_data_t exthds[MAX_KD];\n",
    )
    replace_once(
        rtsp_c,
        '''\ttoken = strtok(line, delimiters);\n\ttoken = strtok(NULL, delimiters);\n\n\t// ignore 501 when used with OPTIONS\n''',
        '''\ttoken = strtok(line, delimiters);\n\ttoken = strtok(NULL, delimiters);\n\trtspcld->last_status = token ? atoi(token) : 0;\n\n\t// ignore 501 when used with OPTIONS\n''',
    )
    replace_once(
        rtsp_c,
        '''bool rtspcl_feedback(struct rtspcl_s *p) {\n\tif (!p) return false;\n\treturn exec_request(p, "POST", NULL, NULL, 0, 1, NULL, NULL, NULL, NULL, "/feedback");\n}\n''',
        '''bool rtspcl_feedback(struct rtspcl_s *p) {\n\tif (!p) return false;\n\treturn exec_request(p, "POST", NULL, NULL, 0, 1, NULL, NULL, NULL, NULL, "/feedback");\n}\n\nint rtspcl_last_status(struct rtspcl_s *p) {\n\treturn p ? p->last_status : 0;\n}\n''',
    )

    # Extend the existing MiTV-only WIRE diagnostic ABI; no new transport path
    # or DLL export is introduced.
    replace_once(
        raop_h,
        '''\tuint32_t first_audio_timestamp, last_audio_timestamp;\n\tuint16_t first_audio_seq, last_audio_seq;\n} raop_diag_snapshot_t;\n''',
        '''\tuint32_t first_audio_timestamp, last_audio_timestamp;\n\tuint16_t first_audio_seq, last_audio_seq;\n\tchar handshake_sid[11];\n\tchar handshake_client_instance[17];\n\tchar handshake_et[16];\n\tchar handshake_dacp_id[17];\n\tchar handshake_active_remote[11];\n\tchar handshake_sdp[1024];\n\tchar handshake_setup_transport[512];\n\tchar handshake_setup_session[128];\n\tchar handshake_audio_latency[64];\n\tuint32_t handshake_codec;\n\tuint32_t handshake_crypto;\n\tuint32_t handshake_encrypt;\n\tuint32_t handshake_auth;\n\tuint32_t handshake_auth_setup_attempted;\n\tuint32_t handshake_auth_setup_status;\n\tuint32_t handshake_announce_status;\n\tuint32_t handshake_setup_status;\n\tuint32_t handshake_record_status;\n\tuint16_t handshake_record_seq;\n\tuint32_t handshake_record_ts;\n} raop_diag_snapshot_t;\n''',
    )
    replace_once(
        raop_c,
        "\tchar passwd[64];\n} raopcl_data_t;\n",
        '''\tchar passwd[64];\n\tchar diag_sid[11];\n\tchar diag_client_instance[17];\n\tchar diag_sdp[1024];\n\tchar diag_setup_transport[512];\n\tchar diag_setup_session[128];\n\tchar diag_audio_latency[64];\n\tuint32_t diag_auth_setup_attempted;\n\tuint32_t diag_auth_setup_status;\n\tuint32_t diag_announce_status;\n\tuint32_t diag_setup_status;\n\tuint32_t diag_record_status;\n\tuint16_t diag_record_seq;\n\tuint32_t diag_record_ts;\n} raopcl_data_t;\n''',
    )
    replace_once(
        raop_c,
        '''\tsprintf(sid, "%010lu", (long unsigned int) seed.sid);\n\tsprintf(sci, "%016llx", (long long int) seed.sci);\n\n\t// RTSP misc setup\n''',
        '''\tsprintf(sid, "%010lu", (long unsigned int) seed.sid);\n\tsprintf(sci, "%016llx", (long long int) seed.sci);\n\tstrncpy(p->diag_sid, sid, sizeof(p->diag_sid) - 1);\n\tstrncpy(p->diag_client_instance, sci, sizeof(p->diag_client_instance) - 1);\n\n\t// RTSP misc setup\n''',
    )
    replace_once(
        raop_c,
        '''\t// Send pubkey for MFi devices\n\tif (strchr(p->et, '4')) rtspcl_auth_setup(p->rtspcl);\n''',
        '''\t// Send pubkey for MFi devices. Record the response only; preserve the\n\t// upstream behavior that does not branch on auth-setup success here.\n\tif (strchr(p->et, '4')) {\n\t\tp->diag_auth_setup_attempted = 1;\n\t\t(void) rtspcl_auth_setup(p->rtspcl);\n\t\tp->diag_auth_setup_status = (uint32_t)rtspcl_last_status(p->rtspcl);\n\t}\n''',
    )
    replace_once(
        raop_c,
        '''\tif (!raopcl_set_sdp(p, sdp)) goto erexit;\n\n\t// AppleTV expects now the timing port ot be opened BEFORE the setup message\n''',
        '''\tif (!raopcl_set_sdp(p, sdp)) goto erexit;\n\tstrncpy(p->diag_sdp, sdp, sizeof(p->diag_sdp) - 1);\n\n\t// AppleTV expects now the timing port ot be opened BEFORE the setup message\n''',
    )
    replace_once(
        raop_c,
        '''\t} else if (!rtspcl_announce_sdp(p->rtspcl, sdp, p->passwd)) {\n\t\tgoto erexit;\n\t}\n\n\t// open RTP sockets, need local ports here before sending SETUP\n''',
        '''\t} else if (!rtspcl_announce_sdp(p->rtspcl, sdp, p->passwd)) {\n\t\tgoto erexit;\n\t}\n\tp->diag_announce_status = (uint32_t)rtspcl_last_status(p->rtspcl);\n\n\t// open RTP sockets, need local ports here before sending SETUP\n''',
    )
    replace_once(
        raop_c,
        '''\t// RTSP SETUP : get all RTP destination ports\n\tif (!rtspcl_setup(p->rtspcl, &p->rtp_ports, kd)) goto erexit;\n\tif (!raopcl_analyse_setup(p, kd)) goto erexit;\n''',
        '''\t// RTSP SETUP : get all RTP destination ports\n\tif (!rtspcl_setup(p->rtspcl, &p->rtp_ports, kd)) goto erexit;\n\tp->diag_setup_status = (uint32_t)rtspcl_last_status(p->rtspcl);\n\t{\n\t\tchar *diag_value = kd_lookup(kd, "Transport");\n\t\tif (diag_value) snprintf(p->diag_setup_transport, sizeof(p->diag_setup_transport), "%s", diag_value);\n\t\tdiag_value = kd_lookup(kd, "Session");\n\t\tif (diag_value) snprintf(p->diag_setup_session, sizeof(p->diag_setup_session), "%s", diag_value);\n\t}\n\tif (!raopcl_analyse_setup(p, kd)) goto erexit;\n''',
    )
    replace_once(
        raop_c,
        '''\tif (!rtspcl_record(p->rtspcl, p->seq_number + 1, NTP2TS(raopcl_get_ntp(NULL), p->sample_rate), kd)) goto erexit;\n\n\tif (kd_lookup(kd, "Audio-Latency")) {\n\t\tint latency = atoi(kd_lookup(kd, "Audio-Latency"));\n''',
        '''\tp->diag_record_seq = p->seq_number + 1;\n\tp->diag_record_ts = NTP2TS(raopcl_get_ntp(NULL), p->sample_rate);\n\tif (!rtspcl_record(p->rtspcl, p->diag_record_seq, p->diag_record_ts, kd)) goto erexit;\n\tp->diag_record_status = (uint32_t)rtspcl_last_status(p->rtspcl);\n\n\tif (kd_lookup(kd, "Audio-Latency")) {\n\t\tsnprintf(p->diag_audio_latency, sizeof(p->diag_audio_latency), "%s", kd_lookup(kd, "Audio-Latency"));\n\t\tint latency = atoi(kd_lookup(kd, "Audio-Latency"));\n''',
    )
    replace_once(
        raop_c,
        '''    out->first_audio_seq = (uint16_t)InterlockedCompareExchange(&p->diag_first_audio_seq, 0, 0);\n    out->last_audio_seq = (uint16_t)InterlockedCompareExchange(&p->diag_last_audio_seq, 0, 0);\n    pthread_mutex_unlock(&p->mutex);\n''',
        '''    out->first_audio_seq = (uint16_t)InterlockedCompareExchange(&p->diag_first_audio_seq, 0, 0);\n    out->last_audio_seq = (uint16_t)InterlockedCompareExchange(&p->diag_last_audio_seq, 0, 0);\n    snprintf(out->handshake_sid, sizeof(out->handshake_sid), "%s", p->diag_sid);\n    snprintf(out->handshake_client_instance, sizeof(out->handshake_client_instance), "%s", p->diag_client_instance);\n    snprintf(out->handshake_et, sizeof(out->handshake_et), "%s", p->et);\n    snprintf(out->handshake_dacp_id, sizeof(out->handshake_dacp_id), "%s", p->DACP_id);\n    snprintf(out->handshake_active_remote, sizeof(out->handshake_active_remote), "%s", p->active_remote);\n    snprintf(out->handshake_sdp, sizeof(out->handshake_sdp), "%s", p->diag_sdp);\n    snprintf(out->handshake_setup_transport, sizeof(out->handshake_setup_transport), "%s", p->diag_setup_transport);\n    snprintf(out->handshake_setup_session, sizeof(out->handshake_setup_session), "%s", p->diag_setup_session);\n    snprintf(out->handshake_audio_latency, sizeof(out->handshake_audio_latency), "%s", p->diag_audio_latency);\n    out->handshake_codec = (uint32_t)p->codec;\n    out->handshake_crypto = (uint32_t)p->crypto;\n    out->handshake_encrypt = p->encrypt ? 1U : 0U;\n    out->handshake_auth = p->auth ? 1U : 0U;\n    out->handshake_auth_setup_attempted = p->diag_auth_setup_attempted;\n    out->handshake_auth_setup_status = p->diag_auth_setup_status;\n    out->handshake_announce_status = p->diag_announce_status;\n    out->handshake_setup_status = p->diag_setup_status;\n    out->handshake_record_status = p->diag_record_status;\n    out->handshake_record_seq = p->diag_record_seq;\n    out->handshake_record_ts = p->diag_record_ts;\n    pthread_mutex_unlock(&p->mutex);\n''',
    )
    print(f"patched MiTV RTSP handshake diagnostics into pinned libraop under {root}")


def patch_repo(root: Path) -> None:
    bridge_h = root / "native" / "raop-static" / "raop_bridge.h"
    bridge_c = root / "native" / "raop-static" / "raop_bridge.c"
    session_rs = root / "crates" / "sairplay-msa-solo" / "src" / "windows_raop_session.rs"
    for path in (bridge_h, bridge_c, session_rs):
        if not path.is_file():
            raise SystemExit(f"missing repository source: {path}")

    replace_once(
        bridge_h,
        '''    uint32_t first_audio_timestamp, last_audio_timestamp;\n    uint16_t first_audio_seq, last_audio_seq;\n    uint32_t feedback_active;\n''',
        '''    uint32_t first_audio_timestamp, last_audio_timestamp;\n    uint16_t first_audio_seq, last_audio_seq;\n    char handshake_sid[11];\n    char handshake_client_instance[17];\n    char handshake_et[16];\n    char handshake_dacp_id[17];\n    char handshake_active_remote[11];\n    char handshake_sdp[1024];\n    char handshake_setup_transport[512];\n    char handshake_setup_session[128];\n    char handshake_audio_latency[64];\n    uint32_t handshake_codec;\n    uint32_t handshake_crypto;\n    uint32_t handshake_encrypt;\n    uint32_t handshake_auth;\n    uint32_t handshake_auth_setup_attempted;\n    uint32_t handshake_auth_setup_status;\n    uint32_t handshake_announce_status;\n    uint32_t handshake_setup_status;\n    uint32_t handshake_record_status;\n    uint16_t handshake_record_seq;\n    uint32_t handshake_record_ts;\n    uint32_t feedback_active;\n''',
    )
    replace_once(
        bridge_c,
        '''        out->first_audio_seq = raw.first_audio_seq;\n        out->last_audio_seq = raw.last_audio_seq;\n        out->feedback_active = handle->happycast_feedback && handle->feedback_supported;\n''',
        '''        out->first_audio_seq = raw.first_audio_seq;\n        out->last_audio_seq = raw.last_audio_seq;\n        memcpy(out->handshake_sid, raw.handshake_sid, sizeof(out->handshake_sid));\n        memcpy(out->handshake_client_instance, raw.handshake_client_instance, sizeof(out->handshake_client_instance));\n        memcpy(out->handshake_et, raw.handshake_et, sizeof(out->handshake_et));\n        memcpy(out->handshake_dacp_id, raw.handshake_dacp_id, sizeof(out->handshake_dacp_id));\n        memcpy(out->handshake_active_remote, raw.handshake_active_remote, sizeof(out->handshake_active_remote));\n        memcpy(out->handshake_sdp, raw.handshake_sdp, sizeof(out->handshake_sdp));\n        memcpy(out->handshake_setup_transport, raw.handshake_setup_transport, sizeof(out->handshake_setup_transport));\n        memcpy(out->handshake_setup_session, raw.handshake_setup_session, sizeof(out->handshake_setup_session));\n        memcpy(out->handshake_audio_latency, raw.handshake_audio_latency, sizeof(out->handshake_audio_latency));\n        out->handshake_codec = raw.handshake_codec;\n        out->handshake_crypto = raw.handshake_crypto;\n        out->handshake_encrypt = raw.handshake_encrypt;\n        out->handshake_auth = raw.handshake_auth;\n        out->handshake_auth_setup_attempted = raw.handshake_auth_setup_attempted;\n        out->handshake_auth_setup_status = raw.handshake_auth_setup_status;\n        out->handshake_announce_status = raw.handshake_announce_status;\n        out->handshake_setup_status = raw.handshake_setup_status;\n        out->handshake_record_status = raw.handshake_record_status;\n        out->handshake_record_seq = raw.handshake_record_seq;\n        out->handshake_record_ts = raw.handshake_record_ts;\n        out->feedback_active = handle->happycast_feedback && handle->feedback_supported;\n''',
    )
    text = session_rs.read_text(encoding="utf-8")
    import_old = "use std::ffi::{c_char, c_void, CString};\n"
    if import_old not in text:
        raise SystemExit(f"{session_rs}: expected std::ffi import not found")
    session_rs.write_text(text.replace(import_old, "use std::ffi::{c_char, c_void, CStr, CString};\n", 1), encoding="utf-8")

    replace_once(
        session_rs,
        '''#[repr(C)]\n#[derive(Default)]\nstruct SrRaopWireDiag {\n''',
        '''#[repr(C)]\nstruct SrRaopWireDiag {\n''',
    )
    replace_once(
        session_rs,
        '''    feedback_ok: u64, feedback_fail: u64,\n}\n\nfn inproc_open_stage_name''',
        '''    feedback_ok: u64, feedback_fail: u64,\n}\n\nimpl Default for SrRaopWireDiag {\n    fn default() -> Self {\n        // C ABI diagnostic snapshot: every field is an integer or c_char array,\n        // so an all-zero representation is valid and preserves the old derived\n        // Default semantics while supporting arrays larger than 32 elements.\n        unsafe { std::mem::zeroed() }\n    }\n}\n\nfn inproc_open_stage_name''',
    )

    replace_once(
        session_rs,
        '''    first_audio_timestamp: u32, last_audio_timestamp: u32,\n    first_audio_seq: u16, last_audio_seq: u16,\n    feedback_active: u32,\n''',
        '''    first_audio_timestamp: u32, last_audio_timestamp: u32,\n    first_audio_seq: u16, last_audio_seq: u16,\n    handshake_sid: [c_char; 11],\n    handshake_client_instance: [c_char; 17],\n    handshake_et: [c_char; 16],\n    handshake_dacp_id: [c_char; 17],\n    handshake_active_remote: [c_char; 11],\n    handshake_sdp: [c_char; 1024],\n    handshake_setup_transport: [c_char; 512],\n    handshake_setup_session: [c_char; 128],\n    handshake_audio_latency: [c_char; 64],\n    handshake_codec: u32,\n    handshake_crypto: u32,\n    handshake_encrypt: u32,\n    handshake_auth: u32,\n    handshake_auth_setup_attempted: u32,\n    handshake_auth_setup_status: u32,\n    handshake_announce_status: u32,\n    handshake_setup_status: u32,\n    handshake_record_status: u32,\n    handshake_record_seq: u16,\n    handshake_record_ts: u32,\n    feedback_active: u32,\n''',
    )
    replace_once(
        session_rs,
        '''        Some(format!(\n            "MSA RAOP DIAG WIRE-UDP ports=audio:{}->{} control:{}->{} timing:{}->{} state={} seq={} audio_ok={} audio_fail={} sync_ok={} sync_fail={} timing_req={} timing_rsp={} timing_rsp_fail={} control_req={} retransmit={} first_seq={} first_ts={} last_seq={} last_ts={} sane=ctrl:{} time:{} audio_avail:{} audio_select:{} audio_send:{} feedback_active={} feedback_ok={} feedback_fail={}",\n''',
        '''        let cstr = |raw: &[c_char]| unsafe { CStr::from_ptr(raw.as_ptr()) }.to_string_lossy().into_owned();\n        let sdp = cstr(&diag.handshake_sdp).replace("\\r", "").replace("\\n", "|");\n        let setup_request = format!(\n            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={}",\n            diag.control_lport, diag.timing_lport,\n        );\n        let codec_name = match diag.handshake_codec { 0 => "PCM", 1 => "ALAC_RAW", 2 => "ALAC", 3 => "AAC", 4 => "AAC_ELD", _ => "UNKNOWN" };\n        let crypto_name = match diag.handshake_crypto { 0 => "CLEAR", 1 => "RSA", 2 => "FAIRPLAY", 3 => "MFISAP", 4 => "FAIRPLAYSAP", _ => "UNKNOWN" };\n        Some(format!(\n            "MSA RAOP DIAG WIRE-UDP ports=audio:{}->{} control:{}->{} timing:{}->{} state={} seq={} audio_ok={} audio_fail={} sync_ok={} sync_fail={} timing_req={} timing_rsp={} timing_rsp_fail={} control_req={} retransmit={} first_seq={} first_ts={} last_seq={} last_ts={} sane=ctrl:{} time:{} audio_avail:{} audio_select:{} audio_send:{} feedback_active={} feedback_ok={} feedback_fail={} HANDSHAKE user_agent=\\\"iTunes/7.6.2 (Windows; N;)\\\" et=\\\"{}\\\" codec={}({}) crypto={}({}) encrypt={} auth={} auth_setup_attempted={} auth_setup_status={} sid={} client_instance={} dacp_id={} active_remote={} ANNOUNCE.status={} ANNOUNCE.sdp=\\\"{}\\\" SETUP.status={} SETUP.request_transport=\\\"{}\\\" SETUP.response_transport=\\\"{}\\\" SETUP.session=\\\"{}\\\" RECORD.status={} RECORD.range=\\\"npt=0-\\\" RECORD.rtp_info=\\\"seq={};rtptime={}\\\" RECORD.audio_latency=\\\"{}\\\"",\n''',
    )
    replace_once(
        session_rs,
        '''            diag.feedback_active, diag.feedback_ok, diag.feedback_fail,\n        ))\n''',
        '''            diag.feedback_active, diag.feedback_ok, diag.feedback_fail,\n            cstr(&diag.handshake_et), diag.handshake_codec, codec_name,\n            diag.handshake_crypto, crypto_name, diag.handshake_encrypt, diag.handshake_auth,\n            diag.handshake_auth_setup_attempted, diag.handshake_auth_setup_status,\n            cstr(&diag.handshake_sid), cstr(&diag.handshake_client_instance),\n            cstr(&diag.handshake_dacp_id), cstr(&diag.handshake_active_remote),\n            diag.handshake_announce_status, sdp,\n            diag.handshake_setup_status, setup_request,\n            cstr(&diag.handshake_setup_transport), cstr(&diag.handshake_setup_session),\n            diag.handshake_record_status, diag.handshake_record_seq, diag.handshake_record_ts,\n            cstr(&diag.handshake_audio_latency),\n        ))\n''',
    )
    print(f"patched MiTV handshake fields into existing WIRE GUI diagnostic under {root}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=Path)
    parser.add_argument("--libraop", type=Path)
    args = parser.parse_args()
    if bool(args.repo) == bool(args.libraop):
        raise SystemExit("pass exactly one of --repo or --libraop")
    if args.repo:
        patch_repo(args.repo.resolve())
    else:
        patch_libraop(args.libraop.resolve())


if __name__ == "__main__":
    main()