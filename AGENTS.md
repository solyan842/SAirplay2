# SAirplay2 — Mandatory Agent Workflow

This file is a HARD GATE for any agent working on this repository.
Read this file BEFORE inspecting, editing, committing, or proposing any change.

## 1. Protected scope

- NEVER modify, move, rebase, force-update, or otherwise touch protected stable checkpoints/branches.
- Active work must stay on the explicitly selected development branch.
- Do not modify unrelated files.
- Do not refactor, clean up, optimize, or "improve" anything outside the single current failure/task.
- MSA SOLO / pinned MSA source remains the source of truth for the active architecture.

## 2. One task at a time

The required work rhythm is:

`one failure/task -> one body of evidence -> one minimal change -> push -> Action starts -> STOP`

No bundling. No parallel fixes. No speculative follow-up changes.

If GitHub Actions starts or is running after a push, STOP immediately. Do not continue editing or investigating another issue until that Action has completed and its result has been evaluated.

## 3. Mandatory FULL GitHub Actions log gate

For EVERY failed GitHub Action, the agent MUST read the FULL job log before concluding the cause or modifying code.

Required sequence:

1. Resolve the workflow run ID.
2. Resolve the failed job ID.
3. Fetch the decoded workflow job log.
4. Capture the returned `Resource uri`.
5. Read and/or search that Resource until the relevant failure is located in the FULL log.
6. Identify the FIRST meaningful failure with exact evidence (file, line when available, compiler/runtime error, and surrounding context).
7. Only then propose or make ONE minimal fix for that failure.

### Forbidden shortcuts

- Do NOT conclude from the visible connector preview.
- Do NOT conclude from only the last lines of the job.
- Do NOT use a screenshot as a replacement for the full log while the Resource-uri path is available.
- Do NOT claim "the log cannot be read" until the required run -> job -> decoded job log -> Resource-uri path has actually been attempted and has failed.
- Do NOT use `/actions/runs/{id}/logs` through a generic text fetch as proof that decoded logs are unavailable; that route may be ZIP/binary/redirect based.
- Do NOT guess the cause from a familiar-looking error.

If the FULL log cannot be retrieved after the required path is exhausted:

**DO NOT MODIFY CODE.**

Report only the exact blocker that prevents full-log retrieval.

## 4. Failure handling

For a FAIL:

- report the first meaningful failure only;
- cite the exact evidence from the full log;
- propose exactly one minimal fix;
- do not touch unrelated layers;
- after the fix is pushed and Action starts, STOP.

Rule:

`one error = one evidence = one minimal change`

## 5. PASS handling

For a PASS:

- report the exact Action number;
- report the exact commit SHA;
- state PASS explicitly;
- state the exact files changed;
- state important areas that were NOT touched;
- name only the next single step;
- then wait for explicit user continuation.

CI PASS proves build/tests only. Hardware/audible PASS is separate evidence and must not be invented.

## 6. Source and architecture discipline

Before changing transport/protocol/audio behavior, answer first:

**How does the pinned MSA source do this?**

Do not invent timing, buffering, pacing, START/FLUSH, route selection, PCM handoff, ALAC framing, volume semantics, recovery, or protocol exceptions when pinned MSA already defines the behavior.

Windows-specific code may adapt Windows to the MSA contract; it must not redefine the contract.

Do not alter AP2, Rust, libraop source, routing, timing, or another layer unless the current full-log/hardware evidence points to that layer.

## 7. No speculative debugging

If something is not verified, say it is not verified.
If evidence is missing, stop.
If a connector/tool is blocked, report the blocker immediately instead of repeatedly probing unrelated paths.

Never patch symptoms speculatively.

## 8. Startup checklist for every future SAirplay2 session

Before doing any work:

1. Read `/AGENTS.md` completely.
2. Read `docs/PROJECT-STATE.md` for current branch/state and architecture locks.
3. Read the current handoff/state file when one exists.
4. Confirm the single current task.
5. Apply all gates above before touching the repository.

These rules are mandatory, not advisory.