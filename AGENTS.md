# SAirplay2 — MANDATORY AGENT ENTRY GATE

Status: **REQUIRED / REPO SOURCE OF TRUTH**

This file exists to prevent future ChatGPT/session/agent work from repeating already-solved steps, relying on incomplete conversation memory, or diagnosing GitHub Actions without real logs.

## 0. STOP GATE — READ BEFORE DOING ANYTHING

Before any diagnosis, code edit, build/test change, GitHub Actions investigation, protocol change, device-specific workaround, or handoff continuation, the agent MUST:

1. verify the current repository, branch/ref, and relevant commit;
2. read this `AGENTS.md` from the branch/ref being worked on;
3. read `docs/PROJECT-STATE.md` if it exists on that branch;
4. read `docs/GITHUB-ACTIONS-LOGGING-POLICY.md` if it exists on that branch;
5. read every task-specific lock/source document referenced by those files;
6. only then inspect Actions, logs, code, or make changes.

Conversation history, memory, handoff summaries, previous assistant statements, and prior tool output are **secondary context only**. They MUST NOT be treated as authoritative when the repository can be checked directly.

If repository state conflicts with conversation/memory/handoff, **repository state wins**.

## 1. NO REPEATED WORK / NO STATE DRIFT

Before proposing or repeating a fix, first verify whether that work is already present in the current branch/ref.

Forbidden behavior:
- proposing to add durable logging when it already exists;
- proposing to recreate a policy/file already committed;
- repeating a raw-log retrieval loop after the fallback condition has been reached;
- claiming a fix or checkpoint exists without verifying the file/commit;
- reverting to an older workflow simply because the current tool response is incomplete;
- treating an incomplete conversation summary as proof that repository work is missing.

When a completed checkpoint is verified in the repo, treat it as locked state until new repository evidence shows otherwise.

## 2. GITHUB ACTIONS DIAGNOSTIC GATE

For every failed GitHub Action:

1. identify the exact workflow run ID;
2. identify the exact failed job ID;
3. fetch the raw job log **once**;
4. if real raw log text is returned, read the actual failure and identify the first meaningful/root error;
5. if raw log is `Skipped`, empty, inaccessible, suppressed, truncated before the failure, or otherwise not consumable, **do not retry the same path**;
6. immediately fetch the durable log artifact required by `docs/GITHUB-ACTIONS-LOGGING-POLICY.md`;
7. inspect the actual artifact log content;
8. only after real error text is visible may code or workflow logic be changed.

Never infer a compiler/build root cause from run metadata, job status, step name, memory, handoff, or guesswork.

If both raw log and durable artifact cannot be consumed because of connector/tool limitations, report the exact blocked boundary and STOP. Do not loop back to redesign logging unless repository evidence proves logging itself is missing or broken.

## 3. DURABLE LOGGING IS A BUILD REQUIREMENT

Every workflow that builds, tests, compiles, links, packages, or runs diagnostics must:
- persist diagnostically important stdout + stderr to a workspace log file;
- preserve the real native exit code;
- upload the durable log artifact with `if: always()` so PASS and FAIL both retain evidence;
- cover each independent failure boundary, including native C/C++, CMake/MSBuild/nmake, Rust Cargo, generators/patch scripts where relevant, packaging/signing where relevant.

A new or materially modified workflow is incomplete until this fallback exists.

## 4. SAIRPLAY2 PROJECT LOCKS

Protected stable checkpoints must never be modified, rebased, force-updated, or used as an experimental scratch area.

MSA SOLO / AirPlay 2 work must remain source-aligned. Before changing transport/timing/buffering/pacing/START/FLUSH/PCM/ALAC/volume/recovery behavior, verify how the pinned MSA source does it first. Windows-specific code adapts Windows to the MSA contract; it must not silently redefine that contract.

Do not patch symptoms speculatively. Protocol/audio changes require source evidence or hardware/log evidence identifying the affected layer.

Do not mix device-specific experiments into unrelated SOtM, MSA Core, stable, Pair, or MultiRoom behavior without explicit evidence and scope.

## 5. BRANCH / SCOPE GATE

Before every write:
- state and verify the target branch;
- confirm the requested scope;
- do not modify stable/protected checkpoints;
- do not widen a MiTV-only change into SOtM/MSA Core/general transport unless evidence requires it;
- do not silently move work to another branch.

## 6. VERIFICATION BEFORE REPORTING SUCCESS

After a write/change, re-fetch the changed file/commit/workflow and verify the intended content is actually present.

Never report “done”, “locked”, “written”, “fixed”, or “saved” based only on an attempted write call.

For CI changes, verify the workflow/file contents and, when applicable, the resulting Action run/artifact behavior.

## 7. HANDOFF / NEW CHAT RULE

A new chat/session must not continue SAirplay2 work from conversation memory alone.

Required restart sequence:
`repo/ref -> AGENTS.md -> PROJECT-STATE.md (if present) -> logging policy (if present) -> task-specific locks -> current Action/run/artifact/code`

This sequence is mandatory even when a handoff summary exists.

The purpose is to make repository state, not assistant memory, the persistent source of truth.
