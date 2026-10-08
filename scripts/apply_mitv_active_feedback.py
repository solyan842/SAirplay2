from __future__ import annotations

from pathlib import Path
import argparse
import apply_mitv_handshake_diag as handshake_diag

HAPPYCAST_PORT = 52266


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, got {count}: {old[:140]!r}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


def patch_libraop(root: Path) -> None:
    rtsp_h = root / "src" / "rtsp_client.h"
    rtsp_c = root / "src" / "rtsp_client.c"
    raop_h = root / "src" / "raop_client.h"
    raop_c = root / "src" / "raop_client.c"
    for path in (rtsp_h, rtsp_c, raop_h, raop_c):
        if not path.is_file():
            raise SystemExit(f"missing pinned libraop source: {path}")

    replace_once(
        rtsp_h,
        "bool rtspcl_options(struct rtspcl_s *p, key_data_t *rkd);\n",
        "bool rtspcl_options(struct rtspcl_s *p, key_data_t *rkd);\nbool rtspcl_feedback(struct rtspcl_s *p);\n",
    )
    replace_once(
        rtsp_c,
        '''bool rtspcl_options(struct rtspcl_s *p, key_data_t *rkd) {\n\tif (!p) return false;\n\treturn exec_request(p, "OPTIONS", NULL, NULL, 0, 1, NULL, rkd, NULL, NULL, "*");\n}\n''',
        '''bool rtspcl_options(struct rtspcl_s *p, key_data_t *rkd) {\n\tif (!p) return false;\n\treturn exec_request(p, "OPTIONS", NULL, NULL, 0, 1, NULL, rkd, NULL, NULL, "*");\n}\n\n/* AirPlay-v1 feedback heartbeat on the already established RTSP session. */\nbool rtspcl_feedback(struct rtspcl_s *p) {\n\tif (!p) return false;\n\treturn exec_request(p, "POST", NULL, NULL, 0, 1, NULL, NULL, NULL, NULL, "/feedback");\n}\n''',
    )
    replace_once(
        raop_h,
        "bool \traopcl_keepalive(struct raopcl_s *p);\n",
        "bool \traopcl_keepalive(struct raopcl_s *p);\nbool \traopcl_feedback(struct raopcl_s *p);\nbool \traopcl_refresh_record(struct raopcl_s *p, uint64_t start_time);\n",
    )
    replace_once(
        raop_c,
        '''bool raopcl_keepalive(struct raopcl_s *p) {\n\treturn rtspcl_options(p->rtspcl, NULL);\n}\n''',
        '''bool raopcl_keepalive(struct raopcl_s *p) {\n\treturn rtspcl_options(p->rtspcl, NULL);\n}\n\n/* AirPlay-v1 /feedback on the active RTSP connection. */\nbool raopcl_feedback(struct raopcl_s *p) {\n\tif (!p) return false;\n\treturn rtspcl_feedback(p->rtspcl);\n}\n\n/* MiTV/HappyCast only: refresh RECORD after a delayed live-source START so\n * RTP-Info advertises the same seq/timestamp as the first packet. The normal\n * MSA/libraop path is unchanged and never calls this helper. */\nbool raopcl_refresh_record(struct raopcl_s *p, uint64_t start_time) {\n\tkey_data_t kd[64];\n\tbool rc;\n\tuint16_t seq_number;\n\tuint32_t timestamp;\n\n\tif (!p || !p->rtspcl || !start_time) return false;\n\tkd[0].key = NULL;\n\n\tpthread_mutex_lock(&p->mutex);\n\tseq_number = p->seq_number + 1;\n\ttimestamp = (uint32_t)NTP2TS(start_time, p->sample_rate);\n\tpthread_mutex_unlock(&p->mutex);\n\n\trc = rtspcl_record(p->rtspcl, seq_number, timestamp, kd);\n\tp->diag_record_status = (uint32_t)rtspcl_last_status(p->rtspcl);\n\tif (rc) {\n\t\tp->diag_record_seq = seq_number;\n\t\tp->diag_record_ts = timestamp;\n\t\tif (kd_lookup(kd, "Audio-Latency")) {\n\t\t\tint latency = atoi(kd_lookup(kd, "Audio-Latency"));\n\t\t\tp->latency_frames = max((uint32_t) latency, p->latency_frames);\n\t\t}\n\t}\n\tkd_free(kd);\n\treturn rc;\n}\n''',
    )
    print(f"patched active-session feedback + delayed RECORD refresh into pinned libraop under {root}")


def patch_repo(root: Path) -> None:
    bridge_c = root / "native" / "raop-static" / "raop_bridge.c"
    bridge_h = root / "native" / "raop-static" / "raop_bridge.h"
    session_rs = root / "crates" / "sairplay-msa-solo" / "src" / "windows_raop_session.rs"
    worker_rs = root / "crates" / "sairplay-msa-solo" / "src" / "windows_raop_worker.rs"
    for path in (bridge_c, bridge_h, session_rs, worker_rs):
        if not path.is_file():
            raise SystemExit(f"missing repository source: {path}")

    replace_once(
        bridge_c,
        '''    uint8_t *packed24;\n    uint64_t head_audible_ms;\n    CRITICAL_SECTION lock;\n''',
        '''    uint8_t *packed24;\n    uint64_t head_audible_ms;\n    int happycast_feedback;\n    int feedback_supported;\n    uint64_t feedback_ok;\n    uint64_t feedback_fail;\n    CRITICAL_SECTION lock;\n''',
    )
    replace_once(
        bridge_c,
        '''    raopcl_diag_enable(handle->client, config->port == SR_HAPPYCAST_DIAG_PORT);\n\n    if (!raopcl_connect(handle->client, player, config->port, config->volume > 0)) {\n''',
        '''    handle->happycast_feedback = config->port == SR_HAPPYCAST_DIAG_PORT;\n    raopcl_diag_enable(handle->client, handle->happycast_feedback);\n\n    if (!raopcl_connect(handle->client, player, config->port, config->volume > 0)) {\n''',
    )
    replace_once(
        bridge_c,
        '''    sr_probe_happycast_feedback(config, player);\n\n    if (ready) {\n''',
        '''    /* MiTV/HappyCast experiment only: probe /feedback on the active RTSP\n     * session, exactly after libraop has completed ANNOUNCE/SETUP/RECORD. */\n    if (handle->happycast_feedback) {\n        sr_set_feedback_probe_result(1, 0, SR_FEEDBACK_PROBE_RTSP_RESPONSE);\n        if (raopcl_feedback(handle->client)) {\n            handle->feedback_supported = 1;\n            handle->feedback_ok = 1;\n            sr_set_feedback_probe_result(1, 200, SR_FEEDBACK_PROBE_RTSP_RESPONSE);\n        } else {\n            handle->feedback_fail = 1;\n        }\n    }\n\n    if (ready) {\n''',
    )
    replace_once(
        bridge_c,
        '''int sr_raop_keepalive(sr_raop_handle *handle)\n{\n    int ok = 0;\n    if (!handle) return 0;\n    EnterCriticalSection(&handle->lock);\n    if (handle->client) ok = raopcl_keepalive(handle->client) ? 1 : 0;\n    LeaveCriticalSection(&handle->lock);\n    return ok;\n}\n''',
        '''int sr_raop_keepalive(sr_raop_handle *handle)\n{\n    int ok = 0;\n    if (!handle) return 0;\n    EnterCriticalSection(&handle->lock);\n    if (handle->client) {\n        if (handle->happycast_feedback && handle->feedback_supported) {\n            ok = raopcl_feedback(handle->client) ? 1 : 0;\n            if (ok) handle->feedback_ok++;\n            else handle->feedback_fail++;\n        } else {\n            ok = raopcl_keepalive(handle->client) ? 1 : 0;\n        }\n    }\n    LeaveCriticalSection(&handle->lock);\n    return ok;\n}\n''',
    )
    replace_once(
        bridge_c,
        '''    uint64_t audible;\n    uint64_t latency;\n    raop_state_t state;\n''',
        '''    uint64_t audible;\n    uint64_t latency;\n    uint64_t stream_start;\n    raop_state_t state;\n''',
    )
    replace_once(
        bridge_c,
        '''    latency = TS2NTP(raopcl_latency(handle->client), raopcl_sample_rate(handle->client));\n    handle->head_audible_ms = 0;\n    ok = raopcl_start_at(handle->client, audible - latency) ? 1 : 0;\n\ndone:\n''',
        '''    latency = TS2NTP(raopcl_latency(handle->client), raopcl_sample_rate(handle->client));\n    stream_start = audible - latency;\n    handle->head_audible_ms = 0;\n    ok = raopcl_start_at(handle->client, stream_start) ? 1 : 0;\n    /* HappyCast appears to bind playback to the RTP-Info from RECORD. Because\n     * the Windows live-source lifecycle can defer START for seconds after the\n     * initial connect-time RECORD, refresh only this receiver's initial RECORD\n     * so seq/rtptime match the first packet on the re-anchored timeline. */\n    if (ok && handle->happycast_feedback && state == RAOP_FLUSHED) {\n        ok = raopcl_refresh_record(handle->client, stream_start) ? 1 : 0;\n    }\n\ndone:\n''',
    )
    replace_once(
        bridge_c,
        '''        out->first_audio_seq = raw.first_audio_seq;\n        out->last_audio_seq = raw.last_audio_seq;\n        ok = 1;\n''',
        '''        out->first_audio_seq = raw.first_audio_seq;\n        out->last_audio_seq = raw.last_audio_seq;\n        out->feedback_active = handle->happycast_feedback && handle->feedback_supported;\n        out->feedback_ok = handle->feedback_ok;\n        out->feedback_fail = handle->feedback_fail;\n        ok = 1;\n''',
    )
    replace_once(
        bridge_h,
        '''    uint32_t first_audio_timestamp, last_audio_timestamp;\n    uint16_t first_audio_seq, last_audio_seq;\n} sr_raop_wire_diag;\n''',
        '''    uint32_t first_audio_timestamp, last_audio_timestamp;\n    uint16_t first_audio_seq, last_audio_seq;\n    uint32_t feedback_active;\n    uint64_t feedback_ok, feedback_fail;\n} sr_raop_wire_diag;\n''',
    )
    replace_once(
        session_rs,
        '''const INPROC_KEEPALIVE: Duration = Duration::from_secs(20);\n''',
        '''const INPROC_KEEPALIVE: Duration = Duration::from_secs(20);\nconst INPROC_HAPPYCAST_FEEDBACK_KEEPALIVE: Duration = Duration::from_secs(25);\nconst HAPPYCAST_COMPAT_PORT: u16 = 52266;\n''',
    )
    replace_once(
        session_rs,
        '''    first_audio_timestamp: u32, last_audio_timestamp: u32,\n    first_audio_seq: u16, last_audio_seq: u16,\n}\n''',
        '''    first_audio_timestamp: u32, last_audio_timestamp: u32,\n    first_audio_seq: u16, last_audio_seq: u16,\n    feedback_active: u32,\n    feedback_ok: u64, feedback_fail: u64,\n}\n''',
    )
    replace_once(
        session_rs,
        '''    handle: RwLock<Option<usize>>,\n    last_keepalive: Mutex<Instant>,\n    _strings: InprocStrings,\n''',
        '''    handle: RwLock<Option<usize>>,\n    last_keepalive: Mutex<Instant>,\n    keepalive_interval: Duration,\n    _strings: InprocStrings,\n''',
    )
    replace_once(
        session_rs,
        '''        if last.elapsed() >= INPROC_KEEPALIVE {\n''',
        '''        if last.elapsed() >= self.keepalive_interval {\n''',
    )
    replace_once(
        session_rs,
        '''            "MSA RAOP DIAG WIRE-UDP ports=audio:{}->{} control:{}->{} timing:{}->{} state={} seq={} audio_ok={} audio_fail={} sync_ok={} sync_fail={} timing_req={} timing_rsp={} timing_rsp_fail={} control_req={} retransmit={} first_seq={} first_ts={} last_seq={} last_ts={} sane=ctrl:{} time:{} audio_avail:{} audio_select:{} audio_send:{}",\n''',
        '''            "MSA RAOP DIAG WIRE-UDP ports=audio:{}->{} control:{}->{} timing:{}->{} state={} seq={} audio_ok={} audio_fail={} sync_ok={} sync_fail={} timing_req={} timing_rsp={} timing_rsp_fail={} control_req={} retransmit={} first_seq={} first_ts={} last_seq={} last_ts={} sane=ctrl:{} time:{} audio_avail:{} audio_select:{} audio_send:{} feedback_active={} feedback_ok={} feedback_fail={}",\n''',
    )
    replace_once(
        session_rs,
        '''            diag.sane_ctrl, diag.sane_time, diag.sane_audio_avail,\n            diag.sane_audio_select, diag.sane_audio_send,\n        ))\n''',
        '''            diag.sane_ctrl, diag.sane_time, diag.sane_audio_avail,\n            diag.sane_audio_select, diag.sane_audio_send,\n            diag.feedback_active, diag.feedback_ok, diag.feedback_fail,\n        ))\n''',
    )
    replace_once(
        session_rs,
        '''        handle: RwLock::new(Some(raw as usize)),\n        last_keepalive: Mutex::new(Instant::now()),\n        _strings: InprocStrings {\n''',
        '''        handle: RwLock::new(Some(raw as usize)),\n        last_keepalive: Mutex::new(Instant::now()),\n        keepalive_interval: if config.port == HAPPYCAST_COMPAT_PORT {\n            INPROC_HAPPYCAST_FEEDBACK_KEEPALIVE\n        } else {\n            INPROC_KEEPALIVE\n        },\n        _strings: InprocStrings {\n''',
    )

    text = worker_rs.read_text(encoding="utf-8")
    count = text.count("MSA RAOP COMPAT feedback probe:")
    if count < 1:
        raise SystemExit(f"{worker_rs}: expected feedback probe log strings")
    worker_rs.write_text(
        text.replace("MSA RAOP COMPAT feedback probe:", "MSA RAOP COMPAT active feedback:"),
        encoding="utf-8",
    )

    print(f"patched MiTV active-session feedback + delayed RECORD refresh experiment under {root}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=Path)
    parser.add_argument("--libraop", type=Path)
    args = parser.parse_args()
    if bool(args.repo) == bool(args.libraop):
        raise SystemExit("pass exactly one of --repo or --libraop")
    if args.repo:
        root = args.repo.resolve()
        patch_repo(root)
        handshake_diag.patch_repo(root)
    else:
        root = args.libraop.resolve()
        patch_libraop(root)
        handshake_diag.patch_libraop(root)


if __name__ == "__main__":
    main()
