# Phase B Clean Rebuild

Stable3 baseline: `e0457111c13b129ed57eaf2a2b2a03cd1448187e`.

This branch rebuilds Phase B beside the hardware-proven Phase A runtime.
Native-only Windows audio behavior from Stable3 is protected and must not be
replaced by mixed-transport plumbing.

First retained Phase B slice:
- pure cross-transport timeline arithmetic;
- transport-neutral concurrent START convergence;
- transport-neutral FLUSH/warm-anchor normalization.

No native worker, WASAPI ownership, persistent PCM cadence, pacing or
starvation behavior is changed by this slice.

Old experimental Phase B remains on `dev/msa-cross-transport-foundation` as
reference only.
