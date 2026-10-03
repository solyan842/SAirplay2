# MSA Core source consolidation status

Branch: `dev/msa-core-architecture`

Validated Buffered Type-103 Windows-adapter behavior has been promoted from the CI runtime patch layer into committed Rust source.

Promotion commit:

`f0450fee062fd2b9b4e52b150043a640c451d9ef` — `refactor: promote validated buffered resume into source`

Current source facts:

- `crates/sairplay-msa-solo/src/windows_audio_worker.rs` now directly owns the field-proven capture-idle lifecycle: STANDBY/FLUSHBUFFERED -> local PCM flush acknowledgement -> fresh PCM -> deferred START -> receiver-derived effective lead.
- `scripts/apply-validated-branch-fixes.ps1` no longer patches audio/worker behavior; it currently contains GUI-only runtime adjustments.
- `scripts/verify-buffered-resume-invariants.ps1` remains the regression guard against returning to the failed inferred in-place rate-0/rate-1 path.

This promotion is intended to be zero wire/behavior change relative to the hardware-tested artifact. Full Windows CI and the dedicated Buffered Resume invariant must pass on the committed source before consolidation is considered CI-validated. A repeat Naim 16/44.1 Buffered hardware test remains required before declaring the consolidation hardware-validated.
