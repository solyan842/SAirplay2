# Legacy RAOP transport provenance

SAirplay2 keeps its native AirPlay 2 engine in Rust. Legacy RAOP and AirPlay 2
RAOP-compatible routes use a source-built Philippe44/libraop helper pinned to:

```
dadcfcaa26d988cdd3e3501ddf8286c224f1b494
```

The pin is intentional for reproducible Windows artifacts; it must not silently
float with upstream master.

The helper is bundled with the matching OpenSSL runtime files from that pin.
SAirplay2 captures one 16-bit / 44.1 kHz / stereo PCM source and fans it out to
legacy helper processes over stdin. Libraop remains responsible for the legacy
RAOP ANNOUNCE / SETUP / RECORD, auth setup, ALAC packetization, NTP timing,
resend handling and RTSP lifecycle.

SAirplay2's checked-in helper overlay adds runtime volume control. CI rebuilds
the helper from the pinned source, verifies the runtime-volume option, and
packages that exact built binary.

Legacy RAOP remains a separate transport class. Native AirPlay 2 PTP/RTP or
buffered-media logic must not be patched into the legacy helper path.
