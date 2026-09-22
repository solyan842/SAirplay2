# Legacy RAOP transport provenance

SAirplay2 keeps its native AirPlay 2 engine in Rust. For the legacy RAOP and
AirPlay 2 RAOP-compatible routes, the Windows package currently uses the
upstream Windows cliraop.exe helper from Philippe44/libraop at the exact
current master commit adopted on 2026-09-22:

dadcfcaa26d988cdd3e3501ddf8286c224f1b494

The helper is bundled with the matching OpenSSL runtime files from that same
commit. PCM is captured once by SAirplay2 as 16-bit / 44.1 kHz / stereo and
fanned out to helper processes over stdin. The helper remains responsible for
the source RAOP ANNOUNCE / SETUP / RECORD, auth-setup behavior, ALAC
packetization, NTP timing, resend handling and RTSP lifecycle.

This bridge is pinned to the exact adopted upstream SHA for reproducible builds rather than floating silently. The
next parity work is the unified persistent command lifecycle and mixed
native+RAOP group timeline.
