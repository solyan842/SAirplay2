# Legacy RAOP transport provenance

SAirplay2 keeps its native AirPlay 2 engine in Rust. For the legacy RAOP and
AirPlay 2 RAOP-compatible routes, the Windows package currently uses the
upstream Windows cliraop.exe helper from Philippe44/libraop at the immutable
commit:

81c2182649da8645ac2a58b78e9f370c79a4165b

The helper is bundled with the matching OpenSSL runtime files from that same
commit. PCM is captured once by SAirplay2 as 16-bit / 44.1 kHz / stereo and
fanned out to helper processes over stdin. The helper remains responsible for
the source RAOP ANNOUNCE / SETUP / RECORD, auth-setup behavior, ALAC
packetization, NTP timing, resend handling and RTSP lifecycle.

This bridge is deliberately pinned rather than reimplemented from memory. The
next parity work is the unified persistent command lifecycle and mixed
native+RAOP group timeline.
