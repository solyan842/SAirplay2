from __future__ import annotations

from pathlib import Path
import argparse

HAPPYCAST_PORT = 52266


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, got {count}: {old[:180]!r}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


def patch_libraop(root: Path) -> None:
    raop_c = root / "src" / "raop_client.c"
    if not raop_c.is_file():
        raise SystemExit(f"missing pinned libraop source: {raop_c}")

    # HappyCast-only A/B. Preserve all generic libraop behavior. For port 52266
    # only, defer the initial SYNC until immediately after the first audio RTP
    # packet has actually been sent. Periodic SYNC and resume behavior otherwise
    # stay untouched.
    replace_once(
        raop_c,
        '''\tbool encrypt;\n\tbool first_pkt;\n\tuint64_t head_ts, pause_ts, start_ts, first_ts;\n''',
        '''\tbool encrypt;\n\tbool first_pkt;\n\tbool happycast_first_sync_pending;\n\tuint64_t head_ts, pause_ts, start_ts, first_ts;\n''',
    )

    replace_once(
        raop_c,
        '''\t\tif (!p->pause_ts) {\n\t\t\tp->head_ts = p->first_ts = p->start_ts ? p->start_ts : now_ts;\n\t\t\tif (first_pkt) _raopcl_send_sync(p, true);\n\t\t\tLOG_INFO("[%p]: restarting w/o pause n:%u.%u, hts:%" PRIu64 "", p, RAOP_SECNTP(now), p->head_ts);\n''',
        f'''\t\tif (!p->pause_ts) {{\n\t\t\tp->head_ts = p->first_ts = p->start_ts ? p->start_ts : now_ts;\n\t\t\tif (first_pkt) {{\n\t\t\t\tif (p->rtsp_port == {HAPPYCAST_PORT}) p->happycast_first_sync_pending = true;\n\t\t\t\telse _raopcl_send_sync(p, true);\n\t\t\t}}\n\t\t\tLOG_INFO("[%p]: restarting w/o pause n:%u.%u, hts:%" PRIu64 "", p, RAOP_SECNTP(now), p->head_ts);\n''',
    )

    replace_once(
        raop_c,
        '''\tif (p->state == RAOP_FLUSHED) {\n\t\tp->first_pkt = true;\n\t\tLOG_INFO("[%p]: begining to stream (LATE) hts:%" PRIu64 " n:%u.%u", p, p->head_ts, RAOP_SECNTP(now));\n\t\tp->state = RAOP_STREAMING;\n\t\t_raopcl_send_sync(p, true);\n\t}\n''',
        f'''\tif (p->state == RAOP_FLUSHED) {{\n\t\tp->first_pkt = true;\n\t\tLOG_INFO("[%p]: begining to stream (LATE) hts:%" PRIu64 " n:%u.%u", p, p->head_ts, RAOP_SECNTP(now));\n\t\tp->state = RAOP_STREAMING;\n\t\tif (p->rtsp_port == {HAPPYCAST_PORT}) p->happycast_first_sync_pending = true;\n\t\telse _raopcl_send_sync(p, true);\n\t}}\n''',
    )

    replace_once(
        raop_c,
        '''\tp->head_ts += p->chunk_len;\n\n\t_raopcl_send_audio(p, packet, sizeof(rtp_audio_pkt_t) + size);\n\n\tpthread_mutex_unlock(&p->mutex);\n''',
        '''\tp->head_ts += p->chunk_len;\n\n\tif (_raopcl_send_audio(p, packet, sizeof(rtp_audio_pkt_t) + size) &&\n\t\tp->happycast_first_sync_pending) {\n\t\tp->happycast_first_sync_pending = false;\n\t\t_raopcl_send_sync(p, true);\n\t}\n\n\tpthread_mutex_unlock(&p->mutex);\n''',
    )

    print(f"patched HappyCast-only first-SYNC-after-first-audio A/B on port {HAPPYCAST_PORT}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--libraop", required=True, type=Path)
    args = parser.parse_args()
    patch_libraop(args.libraop.resolve())


if __name__ == "__main__":
    main()
