from __future__ import annotations

from pathlib import Path
import sys


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(
            f"{path}: expected exactly one match, got {count}: {old[:120]!r}"
        )
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


def append_once(path: Path, marker: str, text_to_append: str) -> None:
    text = path.read_text(encoding="utf-8")
    if marker in text:
        raise SystemExit(f"{path}: marker already present: {marker}")
    path.write_text(text.rstrip() + "\n\n" + text_to_append.strip() + "\n", encoding="utf-8")


if len(sys.argv) != 2:
    raise SystemExit("usage: patch_pinned_raop_wire_diag.py <libraop-source-dir>")

root = Path(sys.argv[1])
source = root / "src" / "raop_client.c"
header = root / "src" / "raop_client.h"
if not source.is_file() or not header.is_file():
    raise SystemExit(f"pinned libraop source missing under {root}")

# Public diagnostic ABI added only to the build copy of the exact pinned source.
replace_once(
    header,
    """typedef struct {\n\tint channels;\n\tint\tsample_size;\n\tint\tsample_rate;\n\traop_codec_t codec;\n\traop_crypto_t crypto;\n} raop_settings_t;\n""",
    """typedef struct {\n\tint channels;\n\tint\tsample_size;\n\tint\tsample_rate;\n\traop_codec_t codec;\n\traop_crypto_t crypto;\n} raop_settings_t;\n\ntypedef struct {\n\tuint16_t audio_lport, audio_rport;\n\tuint16_t control_lport, control_rport;\n\tuint16_t timing_lport, timing_rport;\n\tuint32_t state;\n\tuint32_t seq_number;\n\tuint32_t sane_ctrl, sane_time;\n\tuint32_t sane_audio_avail, sane_audio_select, sane_audio_send;\n\tuint64_t audio_send_ok, audio_send_fail;\n\tuint64_t sync_send_ok, sync_send_fail;\n\tuint64_t timing_requests, timing_responses, timing_response_fail;\n\tuint64_t control_requests;\n\tuint64_t retransmit;\n\tuint32_t first_audio_timestamp, last_audio_timestamp;\n\tuint16_t first_audio_seq, last_audio_seq;\n} raop_diag_snapshot_t;\n""",
)
replace_once(
    header,
    """bool \traopcl_keepalive(struct raopcl_s *p);\n""",
    """bool \traopcl_keepalive(struct raopcl_s *p);\nvoid \traopcl_diag_enable(struct raopcl_s *p, bool enabled);\nbool \traopcl_diag_snapshot(struct raopcl_s *p, raop_diag_snapshot_t *out);\n""",
)

# Private counters are dormant unless the bridge explicitly enables them.
replace_once(
    source,
    """\tunsigned int retransmit;\n\tuint8_t iv[16]; // initialization vector for aes-cbc\n""",
    """\tunsigned int retransmit;\n\tbool diag_enabled;\n\tvolatile LONG64 diag_audio_send_ok, diag_audio_send_fail;\n\tvolatile LONG64 diag_sync_send_ok, diag_sync_send_fail;\n\tvolatile LONG64 diag_timing_requests, diag_timing_responses, diag_timing_response_fail;\n\tvolatile LONG64 diag_control_requests;\n\tvolatile LONG diag_first_audio_seen;\n\tvolatile LONG diag_first_audio_timestamp, diag_last_audio_timestamp;\n\tvolatile LONG diag_first_audio_seq, diag_last_audio_seq;\n\tuint8_t iv[16]; // initialization vector for aes-cbc\n""",
)

# Count actual UDP audio send outcomes and capture first/last wire timestamps.
replace_once(
    source,
    """\t\tn = sendto(p->rtp_ports.audio.fd, (void*) packet, + size, 0, (void*) &addr, sizeof(addr));\n\t\tif (n != size) {\n\t\t\tLOG_DEBUG(\"[%p]: error sending audio packet\", p);\n\t\t\tret = false;\n\t\t\tp->sane.audio.send++;\n\t\t}\n\t\telse p->sane.audio.send = 0;\n\t\tp->sane.audio.avail = 0;\n\t}\n\telse {\n\t\tLOG_DEBUG(\"[%p]: audio socket unavailable\", p);\n\t\tret = false;\n\t\tp->sane.audio.avail++;\n\t}\n""",
    """\t\tn = sendto(p->rtp_ports.audio.fd, (void*) packet, + size, 0, (void*) &addr, sizeof(addr));\n\t\tif (n != size) {\n\t\t\tLOG_DEBUG(\"[%p]: error sending audio packet\", p);\n\t\t\tret = false;\n\t\t\tp->sane.audio.send++;\n\t\t\tif (p->diag_enabled) InterlockedIncrement64(&p->diag_audio_send_fail);\n\t\t}\n\t\telse {\n\t\t\tp->sane.audio.send = 0;\n\t\t\tif (p->diag_enabled) {\n\t\t\t\tuint16_t diag_seq = ((uint16_t)packet->hdr.seq[0] << 8) | packet->hdr.seq[1];\n\t\t\t\tuint32_t diag_ts = ntohl(packet->timestamp);\n\t\t\t\tInterlockedIncrement64(&p->diag_audio_send_ok);\n\t\t\t\tif (InterlockedCompareExchange(&p->diag_first_audio_seen, 1, 0) == 0) {\n\t\t\t\t\tInterlockedExchange(&p->diag_first_audio_timestamp, (LONG)diag_ts);\n\t\t\t\t\tInterlockedExchange(&p->diag_first_audio_seq, (LONG)diag_seq);\n\t\t\t\t}\n\t\t\t\tInterlockedExchange(&p->diag_last_audio_timestamp, (LONG)diag_ts);\n\t\t\t\tInterlockedExchange(&p->diag_last_audio_seq, (LONG)diag_seq);\n\t\t\t}\n\t\t}\n\t\tp->sane.audio.avail = 0;\n\t}\n\telse {\n\t\tLOG_DEBUG(\"[%p]: audio socket unavailable\", p);\n\t\tret = false;\n\t\tp->sane.audio.avail++;\n\t\tif (p->diag_enabled) InterlockedIncrement64(&p->diag_audio_send_fail);\n\t}\n""",
)

# Count sync packets sent on the RAOP control channel.
replace_once(
    source,
    """\tn = sendto(raopcld->rtp_ports.ctrl.fd, (void*) &rsp, sizeof(rsp), 0, (void*) &addr, sizeof(addr));\n\n\tif (!first) pthread_mutex_unlock(&raopcld->mutex);\n""",
    """\tn = sendto(raopcld->rtp_ports.ctrl.fd, (void*) &rsp, sizeof(rsp), 0, (void*) &addr, sizeof(addr));\n\tif (raopcld->diag_enabled) {\n\t\tif (n == (int)sizeof(rsp)) InterlockedIncrement64(&raopcld->diag_sync_send_ok);\n\t\telse InterlockedIncrement64(&raopcld->diag_sync_send_fail);\n\t}\n\n\tif (!first) pthread_mutex_unlock(&raopcld->mutex);\n""",
)

# Count receiver timing requests and our replies.
replace_once(
    source,
    """\t\tif( n > 0) \t{\n\t\t\trtp_time_pkt_t rsp;\n""",
    """\t\tif( n > 0) \t{\n\t\t\trtp_time_pkt_t rsp;\n\t\t\tif (raopcld->diag_enabled) InterlockedIncrement64(&raopcld->diag_timing_requests);\n""",
)
replace_once(
    source,
    """\t\t\tn = sendto(raopcld->rtp_ports.time.fd, (void*) &rsp, sizeof(rsp), 0, (void*) &addr, sizeof(addr));\n\n\t\t\tif (n != (int) sizeof(rsp)) {\n""",
    """\t\t\tn = sendto(raopcld->rtp_ports.time.fd, (void*) &rsp, sizeof(rsp), 0, (void*) &addr, sizeof(addr));\n\t\t\tif (raopcld->diag_enabled) {\n\t\t\t\tif (n == (int)sizeof(rsp)) InterlockedIncrement64(&raopcld->diag_timing_responses);\n\t\t\t\telse InterlockedIncrement64(&raopcld->diag_timing_response_fail);\n\t\t\t}\n\n\t\t\tif (n != (int) sizeof(rsp)) {\n""",
)

# Count control-channel requests (normally retransmission requests).
replace_once(
    source,
    """\t\t\tn = recv(raopcld->rtp_ports.ctrl.fd, (void*) &lost, sizeof(lost), 0);\n\n\t\t\tif (n < 0) continue;\n""",
    """\t\t\tn = recv(raopcld->rtp_ports.ctrl.fd, (void*) &lost, sizeof(lost), 0);\n\n\t\t\tif (n < 0) continue;\n\t\t\tif (n > 0 && raopcld->diag_enabled) InterlockedIncrement64(&raopcld->diag_control_requests);\n""",
)

# Diagnostic enable/snapshot accessors. No transport decisions are changed.
append_once(
    source,
    "raopcl_diag_snapshot(struct raopcl_s *p",
    r'''
/*----------------------------------------------------------------------------*/
void raopcl_diag_enable(struct raopcl_s *p, bool enabled)
{
    if (!p) return;
    pthread_mutex_lock(&p->mutex);
    p->diag_enabled = enabled;
    if (enabled) {
        InterlockedExchange64(&p->diag_audio_send_ok, 0);
        InterlockedExchange64(&p->diag_audio_send_fail, 0);
        InterlockedExchange64(&p->diag_sync_send_ok, 0);
        InterlockedExchange64(&p->diag_sync_send_fail, 0);
        InterlockedExchange64(&p->diag_timing_requests, 0);
        InterlockedExchange64(&p->diag_timing_responses, 0);
        InterlockedExchange64(&p->diag_timing_response_fail, 0);
        InterlockedExchange64(&p->diag_control_requests, 0);
        InterlockedExchange(&p->diag_first_audio_seen, 0);
        InterlockedExchange(&p->diag_first_audio_timestamp, 0);
        InterlockedExchange(&p->diag_last_audio_timestamp, 0);
        InterlockedExchange(&p->diag_first_audio_seq, 0);
        InterlockedExchange(&p->diag_last_audio_seq, 0);
    }
    pthread_mutex_unlock(&p->mutex);
}

/*----------------------------------------------------------------------------*/
bool raopcl_diag_snapshot(struct raopcl_s *p, raop_diag_snapshot_t *out)
{
    if (!p || !out || !p->diag_enabled) return false;

    pthread_mutex_lock(&p->mutex);
    memset(out, 0, sizeof(*out));
    out->audio_lport = p->rtp_ports.audio.lport;
    out->audio_rport = p->rtp_ports.audio.rport;
    out->control_lport = p->rtp_ports.ctrl.lport;
    out->control_rport = p->rtp_ports.ctrl.rport;
    out->timing_lport = p->rtp_ports.time.lport;
    out->timing_rport = p->rtp_ports.time.rport;
    out->state = (uint32_t)p->state;
    out->seq_number = p->seq_number;
    out->sane_ctrl = p->sane.ctrl;
    out->sane_time = p->sane.time;
    out->sane_audio_avail = p->sane.audio.avail;
    out->sane_audio_select = p->sane.audio.select;
    out->sane_audio_send = p->sane.audio.send;
    out->audio_send_ok = (uint64_t)InterlockedCompareExchange64(&p->diag_audio_send_ok, 0, 0);
    out->audio_send_fail = (uint64_t)InterlockedCompareExchange64(&p->diag_audio_send_fail, 0, 0);
    out->sync_send_ok = (uint64_t)InterlockedCompareExchange64(&p->diag_sync_send_ok, 0, 0);
    out->sync_send_fail = (uint64_t)InterlockedCompareExchange64(&p->diag_sync_send_fail, 0, 0);
    out->timing_requests = (uint64_t)InterlockedCompareExchange64(&p->diag_timing_requests, 0, 0);
    out->timing_responses = (uint64_t)InterlockedCompareExchange64(&p->diag_timing_responses, 0, 0);
    out->timing_response_fail = (uint64_t)InterlockedCompareExchange64(&p->diag_timing_response_fail, 0, 0);
    out->control_requests = (uint64_t)InterlockedCompareExchange64(&p->diag_control_requests, 0, 0);
    out->retransmit = p->retransmit;
    out->first_audio_timestamp = (uint32_t)InterlockedCompareExchange(&p->diag_first_audio_timestamp, 0, 0);
    out->last_audio_timestamp = (uint32_t)InterlockedCompareExchange(&p->diag_last_audio_timestamp, 0, 0);
    out->first_audio_seq = (uint16_t)InterlockedCompareExchange(&p->diag_first_audio_seq, 0, 0);
    out->last_audio_seq = (uint16_t)InterlockedCompareExchange(&p->diag_last_audio_seq, 0, 0);
    pthread_mutex_unlock(&p->mutex);
    return true;
}
''',
)

print(f"patched pinned RAOP wire diagnostics under {root}")
