# SAirplay2 source parity audit

Primary reference pinned for this audit:

- music-assistant/airplay-cli
- commit `431c5c582eef9307c4e39c50a0ea65e970bc1128`
- primary files: `src/ap2_client.c`, `src/ap2_ptp.c`, `src/ap2_hap.c`, `src/ap2_io.c`, `DESIGN.md`

Rule: when the primary source already defines native AirPlay 2 behavior, SAirplay2 ports that behavior to Rust/Windows without inventing a different protocol design. Platform substitutions are allowed only below the wire/lifecycle contract.


## Verification snapshot

- **Primary source:** music-assistant/airplay-cli @ `431c5c582eef9307c4e39c50a0ea65e970bc1128`.
- **Code head verified:** `24aff961702661502bfe89e178f870adc6dabcd6`.
- **Windows CI:** run #328 / `35530517923` — Check PASS, invariant tests PASS, GUI build PASS, artifact upload PASS.
- **Artifact digest:** `sha256:6b2775cf2e333359821c5b955118bd80b956306147f08411d216756dff98111d`.
- **Hardware status:** pending fresh HomePod mini / AirPort Express test on this parity build. CI-PROVEN never means hardware-proven.

## Audit matrix

| Area | Primary source | SAirplay2 at audit | Status / action |
|---|---|---|---|
| Discovery feature bits | AP2 bits 38/40/41/46/48, flags, model | Parsed and preserved | MATCH |
| Route selection | Conservative native/compat split | Conservative resolver | MATCH for current alpha |
| GET /info connection | Same TCP continues into HAP | Same TCP retained | MATCH |
| GET /info CSeq | Main RTSP counter starts at 0 | CSeq 0 | MATCH |
| RTSP User-Agent | `AirPlay/670.6.2` | Same | MATCH |
| RTSP base headers | CSeq, User-Agent, DACP-ID, Active-Remote | Native requests emit the same base set; Client-Instance removed | **MATCH · CI-PROVEN** |
| /info capability parse | audioStream/bufferStream masks/extended tables | Implemented | MATCH for 16/44.1 realtime |
| HAP transient pairing | transient M1/M3, fixed PIN unless password, same TCP | Implemented | MATCH current transient path |
| Stored pair-verify | Supported by source | Not implemented | FEATURE GAP; not required for current transient HomePod test path |
| Audio key | first 32 bytes of transient SRP session key | Same | MATCH |
| Timing choice | feature bit 41 -> PTP; fallback NTP only if PTP unavailable | Same high-level decision | MATCH |
| PTP bind | UDP 319/320, multicast membership/interface + unicast peer delivery | Binds 319/320, joins 224.0.1.129, selects RTSP-local multicast egress interface | **MATCH · CI-PROVEN** |
| PTP pre-SETPEERS delivery | peer list empty -> multicast fallback | Empty peer list uses 224.0.1.129; populated peers use source-style unicast delivery | **MATCH · CI-PROVEN** |
| PTP Announce/Sync/Follow_Up | source-shaped gPTP majorSdoId=1 | Source-shaped gPTP majorSdoId=1, Announce 1s, Sync/FUP 125ms, Apple/802.1AS TLVs | **MATCH base sender · CI-PROVEN** |
| PTP Delay/Pdelay replies | required | Implemented | MATCH basic wire path |
| PTP Signaling grants | REQUEST_UNICAST -> GRANT | Implemented | MATCH basic wire path |
| PTP BMCA / peer Announce processing | implemented; sender holds GM normally | Sender hold-GM behavior is preserved for the current one-receiver path; full generic BMCA diagnostics/election are not ported | **PARTIAL · not blocking current single-receiver realtime path** |
| HomePod OS27 follow-clock | conditional AudioAccessory standalone follow mode | Exact model/igl/pgid/tsid/osvers/ov/srcvers/vs predicate; tracks receiver Announce + Sync/FUP, offset and dynamic ClockID/timebase | **MATCH source rule/path · CI-PROVEN; HARDWARE-PENDING** |
| PTP settle | source observes peer/offset up to 400ms | Polls follow-clock decision/offset up to 400ms instead of blind sleep | **MATCH current follow path · CI-PROVEN** |
| PTP Session SETUP plist | PTP protocol, IDs, group, timingPeerInfo/List | implemented | MATCH for GM path |
| PTP identity | deviceID/macAddress/ClockID derived from DACP ID | same | MATCH |
| NTP Session SETUP | deviceID, UUID, timingPort, timingProtocol=NTP | implemented | MATCH |
| Event channel | separate TCP to eventPort, event keys, keep open | implemented and kept open | MATCH for no-MRP audio path |
| RECORD order | RECORD before stream SETUP | same | MATCH |
| Realtime Stream SETUP | type 96, ALAC, ports, latency fields, shk, spf=352 | same request fields | MATCH request |
| Stream response ports | parse dataPort/controlPort by key | same | MATCH |
| Stream response latency | parse latencyMin/latencyMax, clamp lead; arrival latency info | Parses both fields and clamps effective lead immediately after SETUP | **MATCH · CI-PROVEN** |
| SETPEERS | after stream SETUP for PTP; bare plist [receiver, us] | Implemented immediately after Stream SETUP as bare plist `[receiver, us]` | **MATCH · CI-PROVEN** |
| PTP peer list after SETPEERS | hand same peers to PTP engine and kick | Same `[receiver, us]` peers handed to engine; kick forces immediate timing emission | **MATCH · CI-PROVEN** |
| Main RTSP CSeq | /info=0, setup=1, RECORD=2, stream=3, SETPEERS=4, then feedback=5... | PTP follows 0/1/2/3/4 then shared feedback/teardown counter; NTP omits SETPEERS and continues at 4 | **MATCH · CI-PROVEN** |
| RTP SSRC | PTP=0, NTP=session_id | same | MATCH |
| RTP header | PT 96, marker first packet | same | MATCH |
| Realtime audio crypto | ChaCha20-Poly1305, seq nonce, AAD timestamp+SSRC, nonce suffix | same | MATCH |
| NTP sync | 20-byte D4 | implemented | MATCH |
| PTP realtime anchor | 28-byte D7, PTP ns + ClockID + frame geometry | Frozen start line uses dynamic PTP master time/ClockID, including receiver-follow mode | **MATCH · CI-PROVEN; HARDWARE-PENDING** |
| ALAC 16/44.1/352 | fixed realtime encoder | implemented | MATCH target format |
| Delivery pacing window | latencyMax-250ms or default 1.75s; splice depth rules | Receiver window applied with source 250ms margin and shallow 600ms realtime splice depth | **MATCH current realtime target · CI-PROVEN** |
| Initial fill spacing | >=1ms packet release spacing | 1ms minimum release spacing in the Windows producer | **MATCH · CI-PROVEN** |
| Retransmit history | 512 exact wire packets | 512-slot ring stores the exact encrypted wire RTP only after successful local send | **MATCH · CI-PROVEN** |
| Retransmit request | read control UDP type 0x55 | Dedicated control worker drains type 0x55 requests | **MATCH · CI-PROVEN** |
| Retransmit response | type 0x56/D6 + original wire RTP | D6 wrapper echoes request sequence and returns original retained wire RTP | **MATCH · CI-PROVEN** |
| /feedback keepalive | POST /feedback every ~2s | 2s cadence on shared encrypted RTSP channel | **MATCH · CI-PROVEN** |
| Feedback timeout | 2s total budget including serialization lock | One 2s deadline covers lock acquisition + exchange; busy-lock expiry skips tick without consuming CSeq | **MATCH · CI-PROVEN** |
| Feedback miss budget | 3 consecutive misses | added | MATCH basic policy |
| RTSP serializer | one shared channel + lock + global CSeq | added in latest refactor | MATCH architecture |
| Late feedback response carry | preserve stream/HAP nonce sequencing | channel preserves encrypted carry/pending stale CSeq | MATCH basic mechanism |
| MRP/event servicing | source services MRP when MRP exists | MRP not implemented | FEATURE GAP, not required for base audio |
| Native volume | SET_PARAMETER text/parameters on shared RTSP | Exact libraop 0–100 mapping; initial volume is sent before audio start when explicitly configured; live Apply uses shared RTSP/CSeq on a non-UI worker | **MATCH · CI-PROVEN; HARDWARE-PENDING** |
| Metadata | source supports native metadata/MRP | absent | FEATURE GAP; not base HomePod transport prerequisite |
| Buffered type 103 | opt-in only in source | not implemented | OK for current realtime-only target |
| Warm splice/flush lifecycle | source has persistent timeline behavior | Timeline invariants are implemented; Windows realtime producer now keeps the same wire alive with encoded silence through temporary source starvation. Explicit pause/seek command API is still not wired into GUI/native session | **PARTIAL · starvation/source-switch base path CI-PROVEN; explicit command path pending** |
| TEARDOWN | source sends clean native teardown | Stops producer/workers, closes event channel, then serialized TEARDOWN with shared CSeq/deadline; read-timeout farewell is write-only 250ms | **MATCH · CI-PROVEN** |
| Feedback/retransmit shutdown ordering | workers stop before sockets/resources close | Audio -> retransmit -> feedback -> event -> TEARDOWN; timing/control resources remain live through teardown | **MATCH current base lifecycle · CI-PROVEN** |

## Correction order

Do not add unrelated features while parity corrections are open.

1. Remove non-source `Client-Instance` from native RTSP requests.
2. Port PTP `SETPEERS` exactly after realtime Stream SETUP; advance shared CSeq correctly.
3. Port PTP peer-list semantics and multicast fallback/member handling from `ap2_ptp.c`.
4. Parse stream response `latencyMin/latencyMax` and clamp effective lead exactly like source.
5. Port delivery pacing window + 1ms initial-fill spacing.
6. Port realtime retransmit ring and 0x55/0x56 responder.
7. Port clean TEARDOWN on stop/drop using the same shared RTSP lock/CSeq.
8. Pass the full relevant TXT context into native session and port the source HomePod OS27 follow-clock predicate/engine behavior before claiming OS27 parity.
9. Only after the above, integrate warm splice/flush and source-change lifecycle needed by the stable acceptance tests.

## Items intentionally not treated as current protocol defects

- Buffered type 103: source makes it opt-in (`buffered_requested`), so realtime type 96 is valid.
- MRP/type-130/now-playing: not required to establish the base realtime audio path.
- Pair-verify credentials: source supports it, but current alpha is intentionally exercising transient pairing; add when credential UI/storage is introduced.

## Stable gate

No build is called stable until the audit's wire-critical deviations are closed and hardware passes:
cold start, >60s continuous audio, silence 15/30/60s then resume, source changes, repeated next, format transitions, HomePod AP2, AirPort AP2/RAOP as applicable, and 30-minute run.
