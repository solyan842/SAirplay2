from __future__ import annotations

from pathlib import Path
import argparse

HAPPYCAST_PORT = 52266
HAPPYCAST_USER_AGENT = "AirPlay/999.0.0"


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, got {count}: {old[:180]!r}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


def patch_libraop(root: Path) -> None:
    rtsp_h = root / "src" / "rtsp_client.h"
    rtsp_c = root / "src" / "rtsp_client.c"
    raop_c = root / "src" / "raop_client.c"
    for path in (rtsp_h, rtsp_c, raop_c):
        if not path.is_file():
            raise SystemExit(f"missing pinned libraop source: {path}")

    replace_once(
        rtsp_h,
        "struct rtspcl_s *rtspcl_create(char* user_name);\nbool   \t\t\trtspcl_destroy(struct rtspcl_s *p);\n",
        "struct rtspcl_s *rtspcl_create(char* user_name);\nbool   \t\t\trtspcl_destroy(struct rtspcl_s *p);\nbool            rtspcl_set_useragent(struct rtspcl_s *p, const char *useragent);\n",
    )
    replace_once(
        rtsp_c,
        '''struct rtspcl_s *rtspcl_create(char *useragent) {\n\trtspcl_t* rtspcld = malloc(sizeof(rtspcl_t));\n\tmemset(rtspcld, 0, sizeof(rtspcl_t));\n\trtspcld->useragent = useragent;\n\trtspcld->fd = -1;\n\treturn rtspcld;\n}\n''',
        '''struct rtspcl_s *rtspcl_create(char *useragent) {\n\trtspcl_t* rtspcld = malloc(sizeof(rtspcl_t));\n\tmemset(rtspcld, 0, sizeof(rtspcl_t));\n\trtspcld->useragent = useragent;\n\trtspcld->fd = -1;\n\treturn rtspcld;\n}\n\n/* MiTV/HappyCast A/B only. The supplied value is a static string literal. */\nbool rtspcl_set_useragent(struct rtspcl_s *p, const char *useragent) {\n\tif (!p || !useragent || !*useragent) return false;\n\tp->useragent = useragent;\n\treturn true;\n}\n''',
    )
    replace_once(
        raop_c,
        '''\tif (peer.s_addr != INADDR_ANY) p->peer_addr.s_addr = peer.s_addr;\n\tif (destport != 0) p->rtsp_port = destport;\n\n\tRAND_bytes((uint8_t*) &p->ssrc, sizeof(p->ssrc));\n''',
        f'''\tif (peer.s_addr != INADDR_ANY) p->peer_addr.s_addr = peer.s_addr;\n\tif (destport != 0) p->rtsp_port = destport;\n\tif (p->rtsp_port == {HAPPYCAST_PORT}) {{\n\t\trtspcl_set_useragent(p->rtspcl, "{HAPPYCAST_USER_AGENT}");\n\t}}\n\n\tRAND_bytes((uint8_t*) &p->ssrc, sizeof(p->ssrc));\n''',
    )

    print(
        f"patched HappyCast-only RTSP User-Agent A/B: port={HAPPYCAST_PORT} ua={HAPPYCAST_USER_AGENT}"
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--libraop", required=True, type=Path)
    args = parser.parse_args()
    patch_libraop(args.libraop.resolve())


if __name__ == "__main__":
    main()
