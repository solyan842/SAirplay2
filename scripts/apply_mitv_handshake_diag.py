from __future__ import annotations

from pathlib import Path
import argparse


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


def patch_libraop(root: Path) -> None:
    rtsp_h = root / "src" / "rtsp_client.h"
    rtsp_c = root / "src" / "rtsp_client.c"
    raop_h = root / "src" / "raop_client.h"
    raop_c = root / "src" / "raop_client.c"
    for path in (rtsp_h, rtsp_c, raop_h, raop_c):
        if not path.is_file():
            raise SystemExit(f"missing pinned libraop source: {path}")

    # Keep the actual RTSP response code from the existing request parser. This
    # is diagnostic state only; request construction and success/failure rules
    # remain untouched.
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

    replace_once(
        raop_h,
        "bool \traopcl_feedback(struct raopcl_s *p);\n",
        "bool \traopcl_feedback(struct raopcl_s *p);\nbool \traopcl_handshake_dump(struct raopcl_s *p, char *out, int out_size);\n",
    )
    replace_once(
        raop_c,
        "\tchar passwd[64];\n} raopcl_data_t;\n",
        '''\tchar passwd[64];\n\tchar diag_sid[11];\n\tchar diag_client_instance[17];\n\tchar diag_sdp[1024];\n\tchar diag_setup_transport[512];\n\tchar diag_setup_session[128];\n\tchar diag_audio_latency[64];\n\tint diag_auth_setup_attempted;\n\tint diag_auth_setup_status;\n\tint diag_announce_status;\n\tint diag_setup_status;\n\tint diag_record_status;\n\tuint16_t diag_record_seq;\n\tuint32_t diag_record_ts;\n} raopcl_data_t;\n''',
    )
    replace_once(
        raop_c,
        '''\tsprintf(sid, "%010lu", (long unsigned int) seed.sid);\n\tsprintf(sci, "%016llx", (long long int) seed.sci);\n\n\t// RTSP misc setup\n''',
        '''\tsprintf(sid, "%010lu", (long unsigned int) seed.sid);\n\tsprintf(sci, "%016llx", (long long int) seed.sci);\n\tstrncpy(p->diag_sid, sid, sizeof(p->diag_sid) - 1);\n\tstrncpy(p->diag_client_instance, sci, sizeof(p->diag_client_instance) - 1);\n\n\t// RTSP misc setup\n''',
    )
    replace_once(
        raop_c,
        '''\t// Send pubkey for MFi devices\n\tif (strchr(p->et, '4')) rtspcl_auth_setup(p->rtspcl);\n''',
        '''\t// Send pubkey for MFi devices. Diagnostic fields record the exact\n\t// response without changing upstream's decision to ignore auth-setup failure.\n\tif (strchr(p->et, '4')) {\n\t\tp->diag_auth_setup_attempted = 1;\n\t\t(void) rtspcl_auth_setup(p->rtspcl);\n\t\tp->diag_auth_setup_status = rtspcl_last_status(p->rtspcl);\n\t}\n''',
    )
    replace_once(
        raop_c,
        '''\tif (!raopcl_set_sdp(p, sdp)) goto erexit;\n\n\t// AppleTV expects now the timing port ot be opened BEFORE the setup message\n''',
        '''\tif (!raopcl_set_sdp(p, sdp)) goto erexit;\n\tstrncpy(p->diag_sdp, sdp, sizeof(p->diag_sdp) - 1);\n\n\t// AppleTV expects now the timing port ot be opened BEFORE the setup message\n''',
    )
    replace_once(
        raop_c,
        '''\t} else if (!rtspcl_announce_sdp(p->rtspcl, sdp, p->passwd)) {\n\t\tgoto erexit;\n\t}\n\n\t// open RTP sockets, need local ports here before sending SETUP\n''',
        '''\t} else if (!rtspcl_announce_sdp(p->rtspcl, sdp, p->passwd)) {\n\t\tgoto erexit;\n\t}\n\tp->diag_announce_status = rtspcl_last_status(p->rtspcl);\n\n\t// open RTP sockets, need local ports here before sending SETUP\n''',
    )
    replace_once(
        raop_c,
        '''\t// RTSP SETUP : get all RTP destination ports\n\tif (!rtspcl_setup(p->rtspcl, &p->rtp_ports, kd)) goto erexit;\n\tif (!raopcl_analyse_setup(p, kd)) goto erexit;\n''',
        '''\t// RTSP SETUP : get all RTP destination ports\n\tif (!rtspcl_setup(p->rtspcl, &p->rtp_ports, kd)) goto erexit;\n\tp->diag_setup_status = rtspcl_last_status(p->rtspcl);\n\t{\n\t\tchar *diag_value = kd_lookup(kd, "Transport");\n\t\tif (diag_value) snprintf(p->diag_setup_transport, sizeof(p->diag_setup_transport), "%s", diag_value);\n\t\tdiag_value = kd_lookup(kd, "Session");\n\t\tif (diag_value) snprintf(p->diag_setup_session, sizeof(p->diag_setup_session), "%s", diag_value);\n\t}\n\tif (!raopcl_analyse_setup(p, kd)) goto erexit;\n''',
    )
    replace_once(
        raop_c,
        '''\tif (!rtspcl_record(p->rtspcl, p->seq_number + 1, NTP2TS(raopcl_get_ntp(NULL), p->sample_rate), kd)) goto erexit;\n\n\tif (kd_lookup(kd, "Audio-Latency")) {\n\t\tint latency = atoi(kd_lookup(kd, "Audio-Latency"));\n''',
        '''\tp->diag_record_seq = p->seq_number + 1;\n\tp->diag_record_ts = NTP2TS(raopcl_get_ntp(NULL), p->sample_rate);\n\tif (!rtspcl_record(p->rtspcl, p->diag_record_seq, p->diag_record_ts, kd)) goto erexit;\n\tp->diag_record_status = rtspcl_last_status(p->rtspcl);\n\n\tif (kd_lookup(kd, "Audio-Latency")) {\n\t\tsnprintf(p->diag_audio_latency, sizeof(p->diag_audio_latency), "%s", kd_lookup(kd, "Audio-Latency"));\n\t\tint latency = atoi(kd_lookup(kd, "Audio-Latency"));\n''',
    )

    append_once(
        raop_c,
        "raopcl_handshake_dump(struct raopcl_s *p",
        r'''
/*----------------------------------------------------------------------------*/
bool raopcl_handshake_dump(struct raopcl_s *p, char *out, int out_size)
{
    char sdp_flat[1024];
    char setup_request[256];
    size_t i, j = 0;
    int written;
    if (!p || !out || out_size <= 0) return false;

    pthread_mutex_lock(&p->mutex);
    memset(sdp_flat, 0, sizeof(sdp_flat));
    for (i = 0; p->diag_sdp[i] && j + 1 < sizeof(sdp_flat); ++i) {
        if (p->diag_sdp[i] == '\r') continue;
        sdp_flat[j++] = p->diag_sdp[i] == '\n' ? '|' : p->diag_sdp[i];
    }
    snprintf(setup_request, sizeof(setup_request),
             "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=%u;timing_port=%u",
             (unsigned)p->rtp_ports.ctrl.lport, (unsigned)p->rtp_ports.time.lport);

    written = snprintf(
        out, (size_t)out_size,
        "MSA RAOP HANDSHAKE user_agent=\"iTunes/7.6.2 (Windows; N;)\" et=\"%s\" codec=%d crypto=%d encrypt=%d auth=%d auth_setup_attempted=%d auth_setup_status=%d sid=%s client_instance=%s dacp_id=%s active_remote=%s ANNOUNCE.status=%d ANNOUNCE.sdp=\"%s\" SETUP.status=%d SETUP.request_transport=\"%s\" SETUP.response_transport=\"%s\" SETUP.session=\"%s\" RECORD.status=%d RECORD.range=\"npt=0-\" RECORD.rtp_info=\"seq=%u;rtptime=%u\" RECORD.audio_latency=\"%s\"",
        p->et, (int)p->codec, (int)p->crypto, p->encrypt ? 1 : 0, p->auth ? 1 : 0,
        p->diag_auth_setup_attempted, p->diag_auth_setup_status,
        p->diag_sid, p->diag_client_instance, p->DACP_id, p->active_remote,
        p->diag_announce_status, sdp_flat,
        p->diag_setup_status, setup_request, p->diag_setup_transport, p->diag_setup_session,
        p->diag_record_status, (unsigned)p->diag_record_seq, (unsigned)p->diag_record_ts,
        p->diag_audio_latency);
    pthread_mutex_unlock(&p->mutex);
    return written > 0 && written < out_size;
}
''',
    )
    print(f"patched MiTV RTSP handshake diagnostics into pinned libraop under {root}")


def patch_repo(root: Path) -> None:
    bridge_c = root / "native" / "raop-static" / "raop_bridge.c"
    worker_rs = root / "crates" / "sairplay-msa-solo" / "src" / "windows_raop_worker.rs"
    for path in (bridge_c, worker_rs):
        if not path.is_file():
            raise SystemExit(f"missing repository source: {path}")

    replace_once(
        bridge_c,
        '''static volatile LONG g_feedback_probe_class = SR_FEEDBACK_PROBE_NONE;\n''',
        '''static volatile LONG g_feedback_probe_class = SR_FEEDBACK_PROBE_NONE;\nstatic char g_happycast_handshake_dump[4096] = {0};\n''',
    )
    replace_once(
        bridge_c,
        '''    if (handle->happycast_feedback) {\n        sr_set_feedback_probe_result(1, 0, SR_FEEDBACK_PROBE_RTSP_RESPONSE);\n        if (raopcl_feedback(handle->client)) {\n            handle->feedback_supported = 1;\n            handle->feedback_ok = 1;\n            sr_set_feedback_probe_result(1, 200, SR_FEEDBACK_PROBE_RTSP_RESPONSE);\n        } else {\n            handle->feedback_fail = 1;\n        }\n    }\n\n    if (ready) {\n''',
        '''    if (handle->happycast_feedback) {\n        sr_set_feedback_probe_result(1, 0, SR_FEEDBACK_PROBE_RTSP_RESPONSE);\n        if (raopcl_feedback(handle->client)) {\n            handle->feedback_supported = 1;\n            handle->feedback_ok = 1;\n            sr_set_feedback_probe_result(1, 200, SR_FEEDBACK_PROBE_RTSP_RESPONSE);\n        } else {\n            handle->feedback_fail = 1;\n        }\n        memset(g_happycast_handshake_dump, 0, sizeof(g_happycast_handshake_dump));\n        (void) raopcl_handshake_dump(handle->client, g_happycast_handshake_dump,\n                                     (int)sizeof(g_happycast_handshake_dump));\n    }\n\n    if (ready) {\n''',
    )
    replace_once(
        bridge_c,
        '''int sr_raop_last_feedback_probe(int *attempted, int *status, int *result_class)\n{\n    if (!attempted || !status || !result_class) return 0;\n    *attempted = (int)InterlockedCompareExchange(&g_feedback_probe_attempted, 0, 0);\n    *status = (int)InterlockedCompareExchange(&g_feedback_probe_status, 0, 0);\n    *result_class = (int)InterlockedCompareExchange(&g_feedback_probe_class, 0, 0);\n    return 1;\n}\n''',
        '''int sr_raop_last_feedback_probe(int *attempted, int *status, int *result_class)\n{\n    if (!attempted || !status || !result_class) return 0;\n    *attempted = (int)InterlockedCompareExchange(&g_feedback_probe_attempted, 0, 0);\n    *status = (int)InterlockedCompareExchange(&g_feedback_probe_status, 0, 0);\n    *result_class = (int)InterlockedCompareExchange(&g_feedback_probe_class, 0, 0);\n    return 1;\n}\n\n__declspec(dllexport)\nint sr_raop_last_handshake_dump(char *out, size_t out_size)\n{\n    size_t len;\n    if (!out || out_size == 0) return 0;\n    len = strlen(g_happycast_handshake_dump);\n    if (len + 1 > out_size) return 0;\n    memcpy(out, g_happycast_handshake_dump, len + 1);\n    return len ? 1 : 0;\n}\n''',
    )

    replace_once(
        worker_rs,
        "use std::ffi::c_int;\n",
        "use std::ffi::{c_char, c_int, CStr};\n",
    )
    replace_once(
        worker_rs,
        '''fn unix_now_ms() -> i128 {\n''',
        '''type HandshakeDumpFn = unsafe extern "C" fn(*mut c_char, usize) -> c_int;\n\nfn read_handshake_diag_event() -> String {\n    let path = match std::env::current_exe()\n        .ok()\n        .and_then(|exe| exe.parent().map(|parent| parent.join("sairplay-raop.dll")))\n    {\n        Some(path) => path,\n        None => return "MSA RAOP HANDSHAKE unavailable=ffi-path-error".into(),\n    };\n    let library = match unsafe { Library::new(&path) } {\n        Ok(library) => library,\n        Err(error) => return format!("MSA RAOP HANDSHAKE unavailable=ffi-load-error detail={error}"),\n    };\n    let dump = match unsafe { library.get::<HandshakeDumpFn>(b"sr_raop_last_handshake_dump\\0") } {\n        Ok(dump) => dump,\n        Err(error) => return format!("MSA RAOP HANDSHAKE unavailable=ffi-symbol-error detail={error}"),\n    };\n    let mut buffer = vec![0 as c_char; 4096];\n    if unsafe { dump(buffer.as_mut_ptr(), buffer.len()) } == 0 {\n        return "MSA RAOP HANDSHAKE unavailable=ffi-call-error".into();\n    }\n    unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy().into_owned()\n}\n\nfn unix_now_ms() -> i128 {\n''',
    )
    replace_once(
        worker_rs,
        '''        if feedback_probe_target {\n            if let Ok(mut events) = worker.lifecycle_events.lock() {\n                events.push(read_feedback_probe_event());\n            }\n        }\n''',
        '''        if feedback_probe_target {\n            if let Ok(mut events) = worker.lifecycle_events.lock() {\n                events.push(read_feedback_probe_event());\n                events.push(read_handshake_diag_event());\n            }\n        }\n''',
    )
    print(f"patched MiTV handshake GUI diagnostic under {root}")


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
