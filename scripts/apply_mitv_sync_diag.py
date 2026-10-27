"""MiTV-only NTP wire epoch adapter plus the existing #23 SYNC telemetry."""
from pathlib import Path
import argparse
import hashlib


# crosstools Windows clock already includes the NTP epoch addition but retains
# FILETIME's 1601 origin. Remove only that origin difference at the wire edge.
# Internal RTP/head/pacing and every other receiver retain their pinned clocks.
WIRE_HELPER = r'''
static uint32_t mitv_wire_ntp_seconds(uint32_t seconds, uint16_t port)
{
    return port == 52266 ? seconds - (uint32_t)11644473600ULL : seconds;
}
'''
SYNC_OLD = "rsp.curr_time.seconds = htonl(now >> 32);"
SYNC_NEW = "rsp.curr_time.seconds = htonl(mitv_wire_ntp_seconds((uint32_t)(now >> 32), raopcld->rtsp_port));"
TIME_OLD = "rsp.recv_time.seconds = htonl(rsp.recv_time.seconds);"
TIME_NEW = "rsp.recv_time.seconds = htonl(mitv_wire_ntp_seconds(rsp.recv_time.seconds, raopcld->rtsp_port));"


def replace(path, old, new):
    text = path.read_text(encoding="utf-8")
    if text.count(old) != 1:
        raise SystemExit(f"{path}: expected one anchor: {old[:100]!r}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


CAPTURE = r'''
    /* MITV_SYNC_CAPTURE_BEGIN: mutex already held by both callers. */
    if (raopcld->diag_enabled && raopcld->rtsp_port == 52266) {
        const uint8_t *wire = (const uint8_t *)&rsp;
        char hex[41];
        char *dst = first ? raopcld->diag_sync_first : raopcld->diag_sync_periodic;
        FILETIME ft;
        ULARGE_INTEGER ticks;
        unsigned long long unix_ms;
        unsigned int i;
        GetSystemTimeAsFileTime(&ft);
        ticks.LowPart = ft.dwLowDateTime;
        ticks.HighPart = ft.dwHighDateTime;
        unix_ms = (ticks.QuadPart - 116444736000000000ULL) / 10000ULL;
        for (i = 0; i < sizeof(rsp); ++i) snprintf(hex + i * 2, 3, "%02x", wire[i]);
        snprintf(dst, 512,
            "wire=%s sent=%d first=%u rtp_latency=%u ntp=%u.%u rtp_head=%u latency_frames=%u total_latency=%u rate=%d head64=%llu clock=%llu audio_ok=%llu record_ts=%u unix_ms=%llu rfc_sec=%u",
            hex, n, first ? 1U : 0U,
            ntohl(rsp.rtp_timestamp_latency), ntohl(rsp.curr_time.seconds),
            ntohl(rsp.curr_time.fraction), ntohl(rsp.rtp_timestamp),
            raopcld->latency_frames, raopcl_latency(raopcld), raopcld->sample_rate,
            (unsigned long long)timestamp, (unsigned long long)raopcl_get_ntp(NULL),
            (unsigned long long)InterlockedCompareExchange64(&raopcld->diag_audio_send_ok, 0, 0),
            raopcld->diag_record_ts, unix_ms, (uint32_t)(unix_ms / 1000ULL + 2208988800ULL));
    }
    /* MITV_SYNC_CAPTURE_END */
'''


def patch_libraop(root):
    source = root / "src/raop_client.c"
    header = root / "src/raop_client.h"
    replace(header, "} raop_diag_snapshot_t;", "\tchar sync_first[512], sync_periodic[512];\n} raop_diag_snapshot_t;")
    replace(source, "\tbool diag_enabled;", "\tbool diag_enabled;\n\tchar diag_sync_first[512], diag_sync_periodic[512];")
    # Capture the actual serialized packet after sendto, before releasing mutex.
    replace(source, "\n\tif (!first) pthread_mutex_unlock(&raopcld->mutex);", CAPTURE + "\n\tif (!first) pthread_mutex_unlock(&raopcld->mutex);")
    replace(source, "    out->state = (uint32_t)p->state;", "    memcpy(out->sync_first, p->diag_sync_first, sizeof(out->sync_first));\n    memcpy(out->sync_periodic, p->diag_sync_periodic, sizeof(out->sync_periodic));\n    out->state = (uint32_t)p->state;")
    # C compile gate: offsets/size must match the bytes decoded by telemetry.
    replace(source, '#include "raop_client.h"', '#include "raop_client.h"\n#include <stddef.h>\n_Static_assert(sizeof(rtp_sync_pkt_t) == 20, "SYNC size");\n_Static_assert(offsetof(rtp_sync_pkt_t, rtp_timestamp_latency) == 4, "SYNC latency offset");\n_Static_assert(offsetof(rtp_sync_pkt_t, curr_time) == 8, "SYNC NTP offset");\n_Static_assert(offsetof(rtp_sync_pkt_t, rtp_timestamp) == 16, "SYNC RTP offset");')
    replace(source, "void _raopcl_send_sync(struct raopcl_s *raopcld, bool first)", WIRE_HELPER + "\nvoid _raopcl_send_sync(struct raopcl_s *raopcld, bool first)")
    replace(source, SYNC_OLD, SYNC_NEW)
    replace(source, TIME_OLD, TIME_NEW)
    print("MiTV SYNC/timing NTP wire epoch corrected; private scheduling clock unchanged")


def patch_repo(root):
    bridge = root / "native/raop-static"
    session = root / "crates/sairplay-msa-solo/src/windows_raop_session.rs"
    replace(bridge / "raop_bridge.h", "} sr_raop_wire_diag;", "    char sync_first[512], sync_periodic[512];\n} sr_raop_wire_diag;")
    replace(bridge / "raop_bridge.c", "        out->state = raw.state;", "        memcpy(out->sync_first, raw.sync_first, sizeof(out->sync_first));\n        memcpy(out->sync_periodic, raw.sync_periodic, sizeof(out->sync_periodic));\n        out->state = raw.state;")
    replace(session, "    refresh_record_ts: u32,\n}", "    refresh_record_ts: u32,\n    sync_first: [c_char; 512],\n    sync_periodic: [c_char; 512],\n}")
    replace(session, 'RECORD.audio_latency=\\\"{}\\\"",', 'RECORD.audio_latency=\\\"{}\\\"\\nMSA RAOP DIAG SYNC first=[{}] periodic=[{}]",')
    replace(session, "            cstr(&diag.handshake_audio_latency),", "            cstr(&diag.handshake_audio_latency),\n            cstr(&diag.sync_first), cstr(&diag.sync_periodic),")
    print("MiTV SYNC snapshot connected to existing 10-second GUI diagnostic")


def verify(root, vendor):
    s = (vendor / "src/raop_client.c").read_text(encoding="utf-8")
    assert s.count(CAPTURE) == 1, "SYNC capture missing or duplicated"
    function = s[s.index("void _raopcl_send_sync("):s.index("void *_rtp_timing_thread(")]
    # Exact #22 function except telemetry and the one documented seconds adapter.
    baseline = function.replace(CAPTURE, "").replace(SYNC_NEW, SYNC_OLD)
    assert hashlib.sha256(baseline.encode()).hexdigest() == "1f4bd1cda5a56774aad2313603637234b6caef84bcdffc0ba698f523e8a47d51", "unexpected SYNC behavior drift"
    assert s.count(WIRE_HELPER) == 1
    assert s.count(SYNC_NEW) == 1 and s.count(TIME_NEW) == 1
    assert "rsp.send_time = rsp.recv_time;" in s
    assert 'raopcld->diag_enabled && raopcld->rtsp_port == 52266' in s
    for marker in ('_Static_assert(sizeof(rtp_sync_pkt_t) == 20', 'memcpy(out->sync_first', 'memcpy(out->sync_periodic', 'happycast_first_sync_pending'):
        assert marker in s, marker
    for path, marker in (("native/raop-static/raop_bridge.h", "char sync_first[512], sync_periodic[512]"), ("native/raop-static/raop_bridge.c", "memcpy(out->sync_periodic"), ("crates/sairplay-msa-solo/src/windows_raop_session.rs", "MSA RAOP DIAG SYNC first=")):
        assert marker in (root / path).read_text(encoding="utf-8"), path
    print("PASS MiTV SYNC source/ABI gates")


def test_source(path):
    path.write_text('#include <stdint.h>\n#include <assert.h>\n#include <stdio.h>\n' + WIRE_HELPER + r'''
int main(void) {
    /* Hardware #23 initial/periodic samples; fractions stay untouched. */
    assert(mitv_wire_ntp_seconds(2761667517U, 52266) == 4002095805U);
    assert(mitv_wire_ntp_seconds(2761667526U, 52266) == 4002095814U);
    /* Zero and rollover: subtraction is modulo 2^32, including NTP era wrap. */
    assert(mitv_wire_ntp_seconds(3054539008U, 52266) == 0U);
    assert(mitv_wire_ntp_seconds(3054539007U, 52266) == UINT32_MAX);
    for (uint32_t port = 0; port <= 65535; ++port) {
        if (port == 52266) continue;
        assert(mitv_wire_ntp_seconds(2761667517U, (uint16_t)port) == 2761667517U);
        assert(mitv_wire_ntp_seconds(0U, (uint16_t)port) == 0U);
        assert(mitv_wire_ntp_seconds(UINT32_MAX, (uint16_t)port) == UINT32_MAX);
    }
    puts("PASS MiTV NTP wire epoch: hardware vectors, era wrap, all other ports unchanged");
    return 0;
}
''', encoding="utf-8")


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--repo", type=Path)
    p.add_argument("--libraop", type=Path)
    p.add_argument("--verify", action="store_true")
    p.add_argument("--test-source", type=Path)
    a = p.parse_args()
    if a.test_source:
        test_source(a.test_source)
    elif a.verify:
        verify(a.repo, a.libraop)
    elif a.repo and not a.libraop:
        patch_repo(a.repo)
    elif a.libraop and not a.repo:
        patch_libraop(a.libraop)
    else:
        p.error("choose --repo or --libraop, or both with --verify")
