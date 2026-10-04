# SAirplay2 — WORK RULES

1. CURRENT STATE
Always verify the current branch, HEAD, Action run and job directly from GitHub.
Never use chat memory, handoff files, summaries, old Actions or PROJECT-STATE as current state.

2. SCOPE
Never touch stable.
Work only on the current development branch.
Do exactly one task at a time.
Do not change unrelated files or subsystems.

3. FAILED ACTION
For a failed Action:
run ID
-> fetch_workflow_run_jobs
-> job ID
-> fetch_workflow_job_logs
-> read result.content directly

Resource uri is OPTIONAL FALLBACK ONLY.
Never search for or require Resource uri when result.content already contains the log.

This rule supersedes all older handoff files, summaries, chat instructions, and documentation.

4. PUSH / PASS
After a push, if an Action starts or is pending, STOP.
On PASS, report Action number, SHA, exact changed files, important untouched areas,
and one next step. Then STOP.

5. ENGINEERING
Follow pinned MSA behavior for AirPlay transport/audio semantics.
Do not guess, speculate, refactor, clean up, or invent protocol behavior without evidence.

6. TOOL USE
Execute the required tool directly.
Do not repeatedly rediscover or repeat the same tool call without progress.
If a tool path fails, verify the exact blocker before trying a valid alternative.
Never turn a tool failure into a guessed code change.


