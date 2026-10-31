# CLI Reference

<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

Full command-line reference for the `evo-agent` binary. The [README](../README.md) keeps a curated selection; this page carries the complete set.

## Command overview

```text
evo-agent run <goal>                    # run an agent (goal + optional agent type)
evo-agent list                          # list all agents in agents/
evo-agent tools list                    # list the 15 built-in tools (3-tier security model)
evo-agent tools show <name>             # show one tool's active/candidate/blocked detail
evo-agent validate <agent>              # validate an agent.json definition
evo-agent config                        # show the merged configuration
evo-agent serve --port 8081             # start the HTTP server (preloads and validates every
                                        #   agent profile at startup; bad profiles fail fast)
evo-agent patrol --session <id> --workspace <ws_id>  # evolution patrol (one-shot task, see below)
evo-agent workflow <workflow_id>        # run a multi-agent DAG workflow (--plan-execute for the
                                        #   full plan-execute pipeline)
evo-agent repl                          # interactive REPL mode (reuses the same session)
evo-agent replay --session <id>         # replay a session's memory-event chain
```

## run

```bash
# run a task with the researcher agent
MINIMAX_API_KEY=sk-... \
  evo-agent run "summarize the README" -a researcher

# streaming output (token-by-token)
evo-agent run "analyze the code structure" -a researcher --stream

# auto-approve candidate tools
evo-agent run "run the build script" -a general --auto-approve-candidates
```

## repl

```bash
evo-agent repl -a general
# > show me the current directory structure
# > /session        # show the current session ID
# > /rewind 3       # rewind to version 3
# > /exit
```

## replay

```bash
# replay all events (timeline order)
evo-agent replay --session 123

# trace backward along the causal chain from event E005
evo-agent replay --session 123 --event E005

# replay all events of one entity
evo-agent replay --session 123 --entity pet_doudou

# LLM natural-language narration (temperature=0; the facts stay unchanged)
evo-agent replay --session 123 --narrate
```

## patrol (evolution patrol task mode)

A one-shot self-evolution patrol: **signal probing → agent drafting → gate-one evidence assembly → governance-chain nomination → structured JSON patrol report**. The trigger lives in this process (cron/ops scripts invoke it on demand); there is zero autonomous loop on the server side — each invocation executes exactly once.

```bash
# patrol evolution signals of one session; draft and nominate automatically when
# signals exist (default rule-copilot profile)
evo-agent patrol --session 42 --workspace 01ABC...

# append the patrol report to a file (schedulers archive it as a time series)
evo-agent patrol --session 42 --workspace 01ABC... --out patrol-report.jsonl
```

Behavior semantics:

- **No signals**: zero-action silent exit (exit 0); the report is `status=no_signal` (one JSON line on stdout, for scheduler consumption).
- **Signals present**: round A — the agent pulls signal details and drafts a constraint-layer draft (`rule_create` → `rule_submit` → `rule_versions`), while the process side assembles gate-one sandbox evidence (dataset → sandbox → close); round B — the agent carries the evidence and calls `rule_promote` to nominate into the human-review queue; the report is `status=nominated` with a `queue_id`. The draft must be an **enforce blocking type** (`transform` entry with `type=enforce`; `params` must contain `domain` and a non-empty `reason`; the blocking condition lives inside `params.domain`; entry-level `condition` or any unknown key is strictly forbidden). After extraction the draft first passes commit-stage structural validation (same standard as the server schema gate); on failure it fails fast into the report (the `error` field carries the validation error, `draft_json` keeps the raw draft) and never enters round B or the submission chain in a bad state.
- **Duplicate nomination**: when a nomination for the same target rule already exists (pending review or published), the server-side two-state dedup gate rejects it (409) and the report is `status=duplicate_rejected` — nomination rights stay inside the human-review loop; the agent surface has no approval channel.

## workflow

```json
// rules/workflows/research_and_write.json
{
  "workflow_id": "research_and_write",
  "nodes": [
    { "id": "research", "agent_type": "researcher", "task": "survey the Rust async ecosystem", "depends_on": [] },
    { "id": "write", "agent_type": "general", "task_template": "Write a report based on the research:\n{research}", "depends_on": ["research"] }
  ],
  "output_node": "write"
}
```

```bash
evo-agent workflow research_and_write

# formula-computation scenario (v1.3): LLM nodes only collect parameters; the sum /
# duration / daily average / clamping are all computed deterministically by compute
# pure functions (see rules/workflows/budget_review.json)
evo-agent workflow budget_review

# tax-statement scenario: compute handles simple closed-form calculations (taxable
# income determination); tax-bracket cascading goes through the UDF macro
# (call_service); document generation goes through template_render — the selection
# boundaries are in evorule-server docs/WASM_UDF_GUIDE.md §5.1 (see
# rules/workflows/tax_statement.json)
```

The workflow DSL supports workflow_dag v1.0/v1.1/v1.2/v1.3 coexisting (dispatched automatically by document shape):

- **v1.1** — node-level conditional branching `run_when` (skipped when it evaluates false; the skip cascades).
- **v1.2** — bounded loops `loops` (statically expanded at load time into a chain of linear copies; still a pure DAG after expansion) and pure-function nodes `compute` (closed directory `strcmp`/`numeric_cmp`/`regex_match`; no LLM, no IO, no side effects; typical use is loop convergence gating: exit early when the result matches the previous round).
- **v1.3** — the compute closed directory expands to 27 functions (arithmetic / string / date / logic families); same input always yields the same output; everything fails fast (division by zero / overflow / out-of-range / out-of-domain dates / out-of-vocabulary all fail the node). Typical use is formula computation: LLM nodes only collect parameters, and all arithmetic is done deterministically by compute nodes (example `rules/workflows/budget_review.json`).

### Replan and plan-execute

On execution failure or budget exhaustion, the outer driver loop (`src/agent/driver.rs`) decides via a replan predicate whether to trigger replanning: **discard-style replanning** — the failure summary plus the plan structure are fed to the planner to produce the next PlanFact, which is materialized and executed fresh (already-executed results are not injected). The replan hard cap defaults to 3 (`--max-replan` / `--max-wall-ms` / `--max-tokens` are tunable; the last one is a cumulative token budget — unlimited when unset). The full plan-execute pipeline (planner produces a plan first, then executes) is enabled with `--plan-execute`:

```bash
# PlanExecute mode: load the workflow as a planning probe (single planner node);
# its output is parsed into PlanFact v1 → materialized → executed
evo-agent workflow research_plan --plan-execute

# DSL mode: a hand-written workflow is executed directly as a v1 plan; when a
# failure triggers replan, the planner produces PlanFact v2 to fix and re-run
# (see rules/workflows/replan_drill.json)
evo-agent workflow replan_drill

# enforce drill: a node model outside the allowlist → the TCB gate raises a
# Violation → the driver classifies it, terminates the whole loop, and does NOT
# replan (constitution violations are one-vote-veto)
evo-agent workflow enforce_drill

# replan hard-cap debounce drill: a compliant node followed by a guaranteed-fail
# node; --max-replan 0 makes step 1 of the decision sequence (hard cap) terminate
# immediately with the error propagated explicitly
evo-agent workflow ok_then_fail_drill --max-replan 0

# planner-retry drill (real-LLM E2E scenario E entry): the planner's first answer
# is invalid JSON → the error is fed back for one retry → the feedback branch
# yields a valid PlanFact → v1 materialization and execution succeed
evo-agent workflow retry_drill --plan-execute
```

On success the driver prints a statistics line (with cost instrumentation since Phase 2):

```text
=== workflow 'research_plan' done (plan_versions=1 replans=0 nodes_executed=3
wall_ms=81234 repeated_nodes=0 tokens_used=18745 replan_tokens=0) ===
```

- `repeated_nodes`: idempotent duplicates in replan output versus already-executed nodes (same id and agent_type) — warned and allowed (discard-style accepts the duplicate cost); nodes in the PlanFact that declare write-class tools (`file_write`/`shell_exec`) are rejected outright at submission (defense-in-depth second gate).
- `tokens_used`: total LLM token consumption (aggregated from provider `usage`; observation only).
- `replan_tokens`: consumption of v2+ re-executions plus the replan planner calls themselves (upper bound of repeat-execution cost; accumulates data for incremental replan policies).

If the planner's PlanFact cannot be parsed as JSON, the driver retries once with the extraction error as feedback (hard cap of 2 planner calls total); if that also fails, the whole run terminates — no guessing, no silence.

### Workflow task marks (collaboration bookkeeping)

During workflow execution the driver creates a marker session (`initial_content={"kind":"workflow_run","workflow_id":...}`) and, after each node succeeds, submits one neutral completion signal `set meta_signal.node_done=<node_id>` to that session (a failed signal submission terminates the workflow — bookkeeping is a hard obligation). The signal itself carries no task semantics; the "node done → task mark" adjudication lives entirely in the rule layer: governance rules (e.g. `rules/governance/collab_task_marks.json`, a business-rule branch shell + set, effective once deployed to the server's rule directory) listen for the signal and write marks such as `meta_task.due_diligence_done` / `meta_task.implemented` / `meta_task.closed` into session state. The mechanism layer produces signals; the rule layer adjudicates marks — the two are separated, so adjusting collaboration discipline only requires changing rules, zero releases. The sample workflow `collab_dd_impl_close` (due-diligence → implementation → acceptance, three nodes) runs under exactly this discipline.

### Collaborative acceptance rules (enforced collaboration discipline)

Constraint-layer disposition decisions are likewise lifted into the rule layer: `rules/governance/00_constraint_collab_acceptance.json` (a constitution-level constraint, `tier="constraint"`, promoted and published through the governance chain) contains three enforce sentinels —

- **R1 boundary enforcement**: tool intent `meta_tool.pending_target_scope=out_of_sandbox` → Halted;
- **R2 implementation precondition**: `meta_workflow.phase=implementation` requires `meta_task.due_diligence_done` to be set (no implementation without due diligence);
- **R3 acceptance precondition**: `phase=closure` requires `meta_task.implemented` to be set (no acceptance without implementation).

Violating instructions are dropped by the engine and a Violation is emitted (attributed with rule_index + reason).

Mechanism-layer cooperation: the runner parses the target landing point of every file-tool call into the canonical field `meta_tool.pending_target_scope` (versus the capability-boundary sandbox root) and sends it along with the instruction into the chain for R1 adjudication; the handler's inline sandbox check remains as the fallback line of defense. For every LLM node, the workflow engine submits a phase signal `set meta_workflow.phase=<node_id>` to the marker session before delegating (the precondition R2/R3 adjudicate on) and submits the completion signal immediately after the node succeeds (so the next node's gate can see the precondition marks) — gate and marking are strictly paired. A block surfaces as an `enforce violation:` error that terminates the run (no replan); when the rule is not deployed, intent sets are still recorded and the inline fallback rejects — the two layers each do their part.
