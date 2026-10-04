# SAirplay2 — WORK RULES

## 1. CURRENT STATE

Always verify the current branch, HEAD, latest relevant Action run, and job directly from GitHub before making decisions.

GitHub is the source of truth for the current repository state.

Chat memory, handoff files, summaries, old Actions, and PROJECT-STATE may be used only as historical context.  
Never treat them as the current repository state unless they are verified against GitHub.

The user's latest explicit instruction in the current conversation takes priority over older chat instructions, handoff files, summaries, and project documentation.

---

## 2. SCOPE

Never touch stable unless the user explicitly instructs otherwise.

Work only on the verified current development branch.

Do exactly one task at a time.

Do not change unrelated files, components, subsystems, behavior, formatting, dependencies, or architecture.

Do not perform cleanup, refactoring, modernization, or opportunistic fixes outside the current task.

---

## 3. FAILED ACTION

For a failed GitHub Action, inspect the actual failed run before changing code:

run ID  
-> fetch workflow run jobs  
-> identify failed job ID  
-> fetch workflow job logs  
-> read the actual log content directly  
-> identify the first relevant failure and its root cause

Do not infer the failure from an Action title, commit message, previous run, memory, or handoff.

Resource URI is an optional fallback only.

Never search for or require a Resource URI when the returned result already contains the required log content.

Do not modify code until the actual failure has been identified from evidence.

---

## 4. PUSH / ACTION / PASS

After a push:

- If the relevant Action is running, queued, or pending: STOP.
- Do not make another code change while waiting for that Action unless the user explicitly requests a separate analysis-only task.
- If the Action fails: inspect the exact failed run and fix only the verified cause.
- If the Action passes: report the Action number, commit SHA, exact changed files, important untouched areas, and exactly one proposed next step.

Then STOP and wait for the user's instruction.

---

## 5. ENGINEERING

Follow the pinned MSA behavior as the source of truth for AirPlay transport and audio semantics.

Preserve verified MSA behavior unless there is direct evidence that SAirplay2 requires an adapter-specific difference.

Do not guess protocol behavior.

Do not invent behavior.

Do not replace verified upstream behavior with custom logic without evidence.

Do not refactor working code merely to make it cleaner.

Prefer the smallest evidence-based change that fixes the verified problem.

When behavior differs from MSA, first determine whether the difference comes from:

1. the SAirplay2 adapter,
2. platform-specific behavior,
3. build/runtime integration,
4. device capability,
5. or an intentional documented difference.

Do not assume the MSA core itself is wrong without evidence.

---

## 6. TOOL USE

Execute the required tool directly.

Do not repeatedly rediscover tools, repository state, branches, Actions, jobs, or files when the required information has already been retrieved and is still current.

If a tool path fails, identify the exact blocker before trying a valid alternative.

Do not turn a tool failure into a guessed code change.

Do not substitute memory or assumptions for information that can be verified directly.

Use the shortest valid tool path that provides authoritative evidence.

---

## 7. CHANGE DISCIPLINE

Before editing, identify:

- the exact current task,
- the exact affected file or subsystem,
- the evidence supporting the change.

After editing, verify:

- only intended files changed,
- unrelated behavior was not modified,
- the change still follows the verified current task and authoritative project rules.

---

## 8. DOCUMENTATION AUTHORITY

For `dev/msa-core-architecture`, current authoritative documentation is limited to:

1. `AGENTS.md` — work rules.
2. `docs/MSA-CORE-SOURCE-LOCK.md` — current architecture/state/source lock.
3. `docs/MSA-CORE-ARCHITECTURE.md` — Core migration architecture.

Inherited `MSA-SOLO-*`, `PROJECT-STATE.md`, `SOURCE-AUDIT.md`,
`ARCHITECTURE.md`, `DEVELOPMENT-RULES.md` and other older documents are
historical/reference material only when they conflict with the current Core lock.

Current branch, HEAD and GitHub Actions state must always be verified directly
from GitHub and must not be inferred from documentation.
