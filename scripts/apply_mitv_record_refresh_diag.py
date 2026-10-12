from __future__ import annotations

from pathlib import Path
import argparse


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, got {count}: {old[:160]!r}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


def patch_repo(root: Path) -> None:
    bridge_c = root / "native" / "raop-static" / "raop_bridge.c"
    bridge_h = root / "native" / "raop-static" / "raop_bridge.h"
    session_rs = root / "crates" / "sairplay-msa-solo" / "src" / "windows_raop_session.rs"
    active_feedback_py = root / "scripts" / "apply_mitv_active_feedback.py"
    for path in (bridge_c, bridge_h, session_rs, active_feedback_py):
        if not path.is_file():
            raise SystemExit(f"missing repository source: {path}")

    # Retain the exact existing HappyCast RECORD-refresh behavior, but make its
    # attempt/result and requested RTP timestamp visible in the existing
    # 10-second wire diagnostic.
    replace_once(
        bridge_c,
        '''    uint64_t feedback_ok;\n    uint64_t feedback_fail;\n    CRITICAL_SECTION lock;\n''',
        '''    uint64_t feedback_ok;\n    uint64_t feedback_fail;\n    uint64_t refresh_record_attempted;\n    uint64_t refresh_record_ok;\n    uint64_t refresh_record_fail;\n    uint32_t refresh_record_ts;\n    CRITICAL_SECTION lock;\n''',
    )
    replace_once(
        bridge_c,
        '''    if (ok && handle->happycast_feedback && state == RAOP_FLUSHED) {\n        ok = raopcl_refresh_record(handle->client, stream_start) ? 1 : 0;\n    }\n''',
        '''    if (ok && handle->happycast_feedback && state == RAOP_FLUSHED) {\n        handle->refresh_record_attempted++;\n        handle->refresh_record_ts = (uint32_t)NTP2TS(\n            stream_start, raopcl_sample_rate(handle->client));\n        ok = raopcl_refresh_record(handle->client, stream_start) ? 1 : 0;\n        if (ok) handle->refresh_record_ok++;\n        else handle->refresh_record_fail++;\n    }\n''',
    )
    replace_once(
        bridge_c,
        '''        out->feedback_active = handle->happycast_feedback && handle->feedback_supported;\n        out->feedback_ok = handle->feedback_ok;\n        out->feedback_fail = handle->feedback_fail;\n        ok = 1;\n''',
        '''        out->feedback_active = handle->happycast_feedback && handle->feedback_supported;\n        out->feedback_ok = handle->feedback_ok;\n        out->feedback_fail = handle->feedback_fail;\n        out->refresh_record_attempted = handle->refresh_record_attempted;\n        out->refresh_record_ok = handle->refresh_record_ok;\n        out->refresh_record_fail = handle->refresh_record_fail;\n        out->refresh_record_ts = handle->refresh_record_ts;\n        ok = 1;\n''',
    )
    replace_once(
        bridge_h,
        '''    uint32_t feedback_active;\n    uint64_t feedback_ok, feedback_fail;\n} sr_raop_wire_diag;\n''',
        '''    uint32_t feedback_active;\n    uint64_t feedback_ok, feedback_fail;\n    uint64_t refresh_record_attempted, refresh_record_ok, refresh_record_fail;\n    uint32_t refresh_record_ts;\n} sr_raop_wire_diag;\n''',
    )
    replace_once(
        session_rs,
        '''    feedback_active: u32,\n    feedback_ok: u64, feedback_fail: u64,\n}\n''',
        '''    feedback_active: u32,\n    feedback_ok: u64, feedback_fail: u64,\n    refresh_record_attempted: u64, refresh_record_ok: u64, refresh_record_fail: u64,\n    refresh_record_ts: u32,\n}\n''',
    )
    replace_once(
        session_rs,
        '''feedback_active={} feedback_ok={} feedback_fail={} HANDSHAKE''',
        '''feedback_active={} feedback_ok={} feedback_fail={} refresh_record_attempted={} refresh_record_ok={} refresh_record_fail={} refresh_record_ts={} HANDSHAKE''',
    )
    replace_once(
        session_rs,
        '''            diag.feedback_active, diag.feedback_ok, diag.feedback_fail,\n            cstr(&diag.handshake_et),''',
        '''            diag.feedback_active, diag.feedback_ok, diag.feedback_fail,\n            diag.refresh_record_attempted, diag.refresh_record_ok,\n            diag.refresh_record_fail, diag.refresh_record_ts,\n            cstr(&diag.handshake_et),''',
    )

    # Stage one additional vendor-only A/B in the already isolated MiTV patch
    # pipeline. This mutates only the workflow worktree copy of the patch driver;
    # the actual pinned libraop source is still cloned fresh later in the job.
    replace_once(
        active_feedback_py,
        '''        patch_libraop(root)\n        handshake_diag.patch_libraop(root)\n''',
        '''        patch_libraop(root)\n        handshake_diag.patch_libraop(root)\n        import apply_mitv_user_agent_ab as user_agent_ab\n        user_agent_ab.patch_libraop(root)\n''',
    )

    print(f"patched MiTV RECORD refresh telemetry + staged User-Agent A/B under {root}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", required=True, type=Path)
    args = parser.parse_args()
    patch_repo(args.repo.resolve())


if __name__ == "__main__":
    main()
