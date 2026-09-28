# MSA-First Rebuild Audit Baseline

Branch: `dev/msa-first-rebuild`
Comparison baseline: `stable-3` at `e0457111c13b129ed57eaf2a2b2a03cd1448187e`
Rebuild ancestry starts from Stable1 only to retain repository/build context.

## Purpose

Stable3 is preserved as the hardware-proven SAirplay2 implementation.
This branch is not a continuation of Stable2/Phase A/Phase B architecture.
It is an independent MSA-first rebuild used to compare architectures fairly.

## Pinned primary sources

- music-assistant/airplay-cli: `431c5c582eef9307c4e39c50a0ea65e970bc1128`
- music-assistant/server: `f09136859e240fc7859160e186c2e2186e917715`
- libraop pinned by SAirplay2: `dadcfcaa26d988cdd3e3501ddf8286c224f1b494`

## Verified MSA architecture

### Session ownership

MSA has one persistent session owner around the PCM input.
The session owns:
- one input reader;
- one bounded ring;
- explicit session state;
- START/FLUSH/STANDBY/END lifecycle;
- epoch changes on START;
- reader parking during FLUSH;
- source readiness based on complete packet availability.

The reader/session and transport are separate responsibilities.

### Explicit state, not inferred audio state

The MSA session state machine explicitly distinguishes:
- IDLE
- PLAYING
- STANDBY
- ENDED

Digital silence, zero PCM and a temporary empty read are not playback-state
signals. START/FLUSH/STANDBY commands own state transitions.

### Cold start

The server:
1. starts/connects every member;
2. starts feeding the session input;
3. waits until every member reports audio present;
4. waits for receiver clock readiness where applicable;
5. computes one shared audible anchor;
6. commands START to every member concurrently;
7. uses each member's true acknowledged instant for convergence.

No guessed setup delay substitutes for connection/audio/clock readiness.

### FLUSH / warm replacement

A warm replacement:
1. stops the old source writer;
2. quiesces transport sends;
3. parks the session input reader;
4. flushes receiver transport state;
5. resets the session ring;
6. drains old input bytes to EAGAIN;
7. re-arms audio-present state;
8. resumes input buffering while still IDLE;
9. starts the new source;
10. waits for all FLUSH acknowledgements;
11. computes one warm anchor;
12. STARTs the group again.

### Group source ownership

The server owns the source session. Members do not independently advance the
source timeline. A source chunk belongs to one session timeline and member
failure/removal is handled at session/group level.

### Transport-specific pacing remains transport-owned

Shared session ownership does not mean flattening transport behavior.

- Native AirPlay 2 keeps AP2 pacing, PTP/NTP timeline, RTP/RTX and splice rules.
- RAOP keeps libraop `accept_frames`, latency and START/FLUSH state rules.
- The group/session layer owns source progression and audible-anchor decisions.

### RAOP rules

Pinned RAOP session behavior includes:
- commanded START against a feasibility floor;
- first/replacement commit may stop+flush as required;
- warm START after FLUSH does not flush again;
- START on a live unflushed stream fails;
- standby keeps the same connected client reusable;
- receiver latency is subtracted from the audible START instant.

## Confirmed SAirplay2 architectural deviations to re-audit

These are not automatically bugs, but they cannot be assumed MSA-equivalent.

1. Stable1 inferred idle/content state from WASAPI/PCM observations and emitted
   synthetic keepalive behavior instead of using an explicit session state
   machine.

2. Stable2 and Phase A accumulated source-aligned behaviors incrementally across
   `windows_audio_worker`, `windows_multiroom_worker`, `native_session`,
   `media_sender`, and legacy helper code rather than having one authoritative
   session lifecycle.

3. Phase A added a persistent Windows PCM ring, but the ring was not yet the
   complete MSA session state machine: START/FLUSH/STANDBY/source replacement
   ownership remained split across higher-level workers.

4. Native-only, native group and RAOP paths evolved separately. Similar timing
   constants or logs are not proof that they share the same MSA lifecycle.

5. Previous Phase B attempted to add common coordination on top of those
   existing lifecycles. That created overlapping ownership instead of replacing
   the old ownership model cleanly.

## Rebuild rule

Do not port an SAirplay2 implementation merely because it worked in Stable3.

For each layer:
1. derive the required behavior from pinned MSA/libraop source;
2. write the invariant/test first;
3. implement the Windows adaptation;
4. keep transport-specific mechanics only where MSA itself keeps them transport-specific;
5. hardware-test the resulting MSA-first path;
6. compare it against untouched Stable3.

## Fair comparison target

Only after the MSA-first implementation independently passes:
- native solo;
- Stereo Pair;
- native MultiRoom;
- RAOP solo/group;
- mixed AP2 + RAOP;
- warm FLUSH/replace;
- late join/rejoin;
- member loss/failure isolation;
- 16/44.1 baseline;
- then 24/48 where capability permits;

may Stable3 vs MSA-first be compared for stability, latency, sync, recovery and
feature completeness.
