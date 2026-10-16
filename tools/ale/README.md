# evo-agent × ALE deployer

`tools/ale/` hosts the [Agents' Last Exam](https://github.com/rdi-berkeley/agents-last-exam)
(ALE) integration for evo-agent: a `BaseAgentDeployer` implementation that
lets the official `ale_run` runner drive evo-agent as the harness under test.

## Shape

`local`-executor agent — the deployer runs inside the framework's Python
process on the host and, per run unit:

1. **install** — stages `<executor work_dir>/agent/` as a dedicated serve
   workdir: installs the official **vm / cua MCP bridges** (vendored in
   `ale_run.agents._assets`, `CUA_SERVER_URL` = `executor.cua_bridge_url()`),
   writes a per-run `evo-agent.toml` project config wiring both bridges
   (`[[mcp.servers]]`, `always_allow` per tool so unattended runs don't stall
   on approval), then starts `evo-agent --workdir ... serve` and waits for
   `/health`.
2. **launch** — POSTs the task prompt to `/agents/<type>/run` (request-level
   `execution_profile` / `tools` allowlist / `model` / `max_steps` come from
   the preset config), then snapshots the session's ATIF trajectory and
   message transcript into the work_dir and stops serve.
3. **parse_artifacts** — maps the ATIF-v1.8 trajectory onto the ALE-v1.0
   `TrajectoryBuilder` (agent steps split into `agent` + `environment`
   steps, tokens → `StepMetrics`/`override_final_metrics`), re-attaching
   screenshots from the content-addressed `data/agent-images/` store as
   inline base64 image parts (the framework relocates them to
   `screenshots/NNNN.png` at persistence time).

## Install

```bash
pip install -e tools/ale          # makes `evo_agent_ale` importable
```

Build the serve binary first (`cargo build --release`); point
`serve_binary` in the preset at the executable.

## Run

Copy `configs/agents/evo-agent.yaml` into the ALE checkout's
`configs/agents/` (or add this directory to the experiment yaml's
`agents:` paths), then:

```bash
ale run experiment.yaml
```

`configs/experiment.example.yaml` shows the shell; `environment` / `tasks`
follow the official quick start.

## Config knobs (`EvoAgentConfig`, preset `config:` block)

| knob | default | meaning |
|---|---|---|
| `serve_binary` | `"evo-agent"` | executable path |
| `serve_host` / `serve_port` | `127.0.0.1` / `18091` | per-run serve bind; unique per concurrent unit |
| `agent_type` | `general` | agent definition in the serve workdir |
| `model` | `None` | request-level model override |
| `execution_profile` | `None` | e.g. `sandbox-trusted` (benchmark line) |
| `tools` | `None` | request-level allowlist (intersect-only, `*` wildcards) |
| `max_steps` / `workspace` | `None` | request-level overrides |
| `install_bridges` | `true` | wire the official vm/cua MCP bridges |
| `always_allow_bridges` | `true` | pre-authorize bridge tools (see note) |
| `extra_config_toml` | `None` | verbatim TOML appended to the generated project config |
| `start_timeout_s` / `request_timeout_s` | `120` / `None` | health wait / run POST timeout (wall budget is orchestration-owned) |

**Approval note**: bridge tools default into `always_allow` (config-level
explicit authorization recorded in the generated `evo-agent.toml` and the
audit trail) so benchmark runs can proceed unattended. Set
`always_allow_bridges: false` to restore the default human-approval flow.

## Files

- `evo_agent_ale/deployer.py` — `EvoAgentDeployer` (install / launch / parse_artifacts)
- `evo_agent_ale/config.py` — `EvoAgentConfig` dataclass
- `evo_agent_ale/transcript_to_trajectory.py` — ATIF → ALE-v1.0 mapping + screenshot re-attachment
- `configs/agents/evo-agent.yaml` — agent preset
- `configs/experiment.example.yaml` — experiment shell
