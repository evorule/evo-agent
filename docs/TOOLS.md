# Tool System Reference

<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

Full reference for the evo-agent tool system: the 3-tier security model, the built-in tools, the rule-management toolkit, source-level switches and adjudication tiers, and the per-agent tool whitelists. The [README](../README.md) keeps a summary; this page carries the complete set.

## Tool totals

- **Spec count**: 59 tools (locked by tests).
- **Serve surface (default)**: 62 tools = 17 built-in + 45 rule tools.
- **sandbox/dataset/publish/production** account for 14 tools that are defined but not exposed on the serve surface (advanced tools).
- **Rule-management toolkit total: 52** (workspace 2 + rule 12 + translate 3 + audit 4 + sandbox 5 + dataset 2 + publish 5 + production 2 + bundles 5 + knowledge 3 + hit-stats 2 + skill assembly 1 + meta 1 + evolution 2 + why/order 3) — the breakdown table below reflects this corrected total.

## 3-tier security model

| Tier | Behavior | Examples |
|------|----------|----------|
| **ACTIVE** | Executes directly, no approval | file_read/file_list/file_write/file_create/file_move/file_delete, search_files/grep_files, git_status/git_diff/git_log/git_stage/git_commit, shell_exec (8-command whitelist), http_get (6-host whitelist) |
| **CANDIDATE** | LLM wants it → returns a proposal → user approves → executes | rm, mv, sed, chmod and 20 shell commands in total; any public HTTP host |
| **BLOCKED** | Never approved | sudo, python, bash, curl, kill and 35 escape-class commands in total; SSRF blacklisted IP ranges |

## Built-in tools (15)

| Tool | Description |
|------|-------------|
| `file_read` / `file_list` / `file_write` / `file_create` / `file_move` / `file_delete` | File I/O (working-directory sandbox; rejects `..` / absolute paths / symlink escapes) |
| `search_files` / `grep_files` | Search files by content |
| `shell_exec` | Run shell commands (3-tier: 8 active + 20 candidate + 35 blocked) |
| `http_get` | HTTP GET requests (6-host whitelist + SSRF protection) |
| `git_status` / `git_diff` / `git_log` / `git_stage` / `git_commit` | git read-only + safe writes (stage/commit; dangerous flags rejected) |

Full-tree search skips large directories by default (`target` / `node_modules` / `.git` / `.evo-trash` / `data`); `search_files` accepts `exclude` and `grep_files` accepts `excludeGlobs` to add more (e.g. `["vendor", "dist"]`).

## Rule-management toolkit (52)

Assembled via `rule_management_toolkit` / `full_rule_toolkit`, used by the `rule-copilot` agent:

| Category | Count | Tools |
|----------|-------|-------|
| workspace | 2 | ws_list, ws_create |
| rule | 12 | rule_list, rule_get, rule_create, rule_update, rule_versions, rule_version_get, rule_submit, rule_activate, rule_block, rule_archive, rule_fork, rule_reload |
| translate | 3 | rule_to_transform, rule_to_conditional, rule_validate |
| audit | 4 | audit_get, audit_verify, session_rewind, audit_export |
| sandbox | 5 | sandbox orchestration (fork + synthetic data + test report) |
| dataset | 2 | dataset management |
| publish | 5 | publish queue + three-tier permissions |
| production | 2 | production environment management |
| bundles | 5 | rule-bundle import / list / rollback |
| knowledge | 3 | knowledge retrieval |
| hit-stats | 2 | hit_stats (rule hit-statistics listing) + hit_stats_series (per-rule cross-version slices), both read-only |
| skill assembly | 1 | skill_pack_to_bundle (skill rule shell → execution-domain snapshot pack bridge; crate-side hashing + structure pre-check; always emits an unverified-fail pack first; a passing re-pack goes through the governance domain channel) |
| meta | 1 | meta_summary (constraint-list summary) |
| evolution | 2 | evolution_signals (evolution-signal pull) + rule_promote (constraint-layer promotion nomination) |
| why/order | 3 | explain_denial (denial-reason explanation + reconciliation against the rule original; the io_guard retained-value denial closes the loop on itself via the fact's hit record) + causal_order (causal ordering of two facts, chained-hash order) + lineage_of (rule lineage = version chain + promotion ledger), all read-only |

## Governance write switch (agentTools.governanceWrite)

Governance write access defaults to **fully on** (`true`, backward compatible — the precondition for LLM self-run). Sensitive-domain deployments (government / defense / finance / healthcare) can turn it off with one switch in the workbench settings:

| Setting | Effect |
|---------|--------|
| `agentTools.governanceWrite = true` (default) | All 46 rule tools; zero change from the status quo |
| `agentTools.governanceWrite = false` | **21 governance-write tools go offline as a group** (8 rule writes + rule_promote + ws_create + sandbox_start/close + dataset_create + publish ×5 + bundle_export/bundle_import/bundle_import_dry_run) — they don't even appear in the LLM's tool spec (the capability surface ceases to exist, not "calls rejected"); 25 read-only tools remain (query / audit / knowledge / skill assembly / meta / evolution_signals / translate / validate, etc.) |

Notes: the key is `application`-scoped and **cannot be overridden at workspace level** (preventing project-level configs from privately expanding the agent tool surface). After turning it off, the LLM can still assist "human drafting" (rule_to_transform / rule_to_conditional / rule_validate are pure computations — no network, no disk writes); submission / promotion / publish authority returns to humans (the console-cloud approval flow). Truly confidential enterprises usually run in-house intranet models where data never leaves the intranet; the default does not interfere with their fully-on shape.

## Dynamic-source tool switches (agentTools.mcp / agentTools.serviceProxy)

MCP remote tools and service-proxy tools are registered dynamically at runtime (not a static tool surface); each binds a source-level master switch, default **fully on** (backward compatible, zero change):

| Setting | Effect |
|---------|--------|
| `agentTools.mcp = true` (default) | All MCP remote tools registered via `mcp.servers` handshake are available |
| `agentTools.mcp = false` | **MCP dynamic tools go offline as a group** — absent from the LLM's tool spec |
| `agentTools.serviceProxy = true` (default) | All service-proxy tools registered via governance service reconciliation are available |
| `agentTools.serviceProxy = false` | **Service-proxy tools go offline as a group** — absent from the LLM's tool spec |

Same scoping semantics as the governance-write switch (`application`; workspace cannot override). For a tool to enter a given agent's session it must still be declared by name in that agent's `def.tools` — the switch is a source-level master gate; `def.tools` is session-level focus.

## Dynamic-source adjudication tiers (agentTools.mcpAdjudication / agentTools.serviceProxyAdjudication)

The adjudication tier of dynamic-source tools is independent of the switches (switches govern **presence**; tiers govern **adjudication** — the two keys are orthogonal). Default is **`sensitive`**: every call submits an intent beforehand, is adjudicated by governance rules, and leaves an account entry (intent is always reported). After human tiering it can be lowered to `standard` (executes directly without intent adjudication; executability unchanged):

| Setting | Effect |
|---------|--------|
| `agentTools.mcpAdjudication = "sensitive"` (default) | Every MCP dynamic-tool call goes through pre-call intent adjudication with an account entry |
| `agentTools.mcpAdjudication = "standard"` | MCP dynamic tools execute directly without intent adjudication |
| `agentTools.serviceProxyAdjudication = "sensitive"` (default) | Every service-proxy call goes through pre-call intent adjudication with an account entry |
| `agentTools.serviceProxyAdjudication = "standard"` | Service-proxy tools execute directly without intent adjudication |

Same `application` scope; tier keys apply only to dynamic-source entries — static tools' governance tiering follows the static table and is unaffected.

## MCP tool integration

Connect to the external tool ecosystem through the MCP client:

```toml
# evo-agent.toml
[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
```

MCP tools are registered automatically with the `mcp_{server}_{tool}` prefix and managed uniformly via the ToolHandler.

### Spawn governance (allowlist gate)

Every stdio MCP server launch passes a source-authentication gate before the process
is spawned. The trusted list can come from two sources, in priority order:

1. **Governance list (recommended)**: the approved entries of the governed dataset
   `org.evorule.mcp.servers` on an EvoRule governance service. Approval goes through
   the dataset lifecycle (human approve action fills the attribution and promotes the
   entry); the executor pulls the effective list as a snapshot bundle and caches it
   in-process with a TTL. Unlisted servers observed at the gate are automatically
   proposed into the governance dataset as Draft entries (fire-and-forget; a proposal
   failure never affects the spawn decision).
2. **Local allowlist file**: `~/.evorule/mcp-allowlist.json` (override with
   `EVORULE_MCP_ALLOWLIST`) - the authority when governance is not configured, and
   the explicit fallback when it is unreachable or stale (every downgrade is logged
   with its reason and duration; a missing file means deny-all under `enforce`).

Policy: `EVORULE_MCP_SPAWN_POLICY` = `enforce` (default, unlisted = reject) or
`warn` (explicit migration mode, allow + log). Matching is command-exact plus
per-segment prefix wildcard; package runners (npx/uvx/node/python/...) must carry a
non-empty `args_pattern`.

Governance-source environment variables (all optional; the governance list requires
URL and pull key, the proposal channel additionally requires the propose key):

| Variable | Default | Purpose |
| --- | --- | --- |
| `EVORULE_MCP_GOV_URL` | empty | Governance service base URL, e.g. `http://127.0.0.1:18081` |
| `EVORULE_MCP_GOV_API_KEY` | empty | `X-Api-Key` with `pull` scope for the effective-list snapshot |
| `EVORULE_MCP_GOV_PROPOSE_KEY` | empty | `X-Api-Key` with `entries:propose` scope for the auto-proposal channel |
| `EVORULE_MCP_GOV_DATASET` | `org.evorule.mcp.servers` | Governed dataset id |
| `EVORULE_MCP_GOV_TTL_SECS` | `300` (clamped 5..=3600) | Freshness window of the cached list |
| `EVORULE_MCP_SPAWN_POLICY` | `enforce` | Unlisted-server disposition |
| `EVORULE_MCP_ALLOWLIST` | `~/.evorule/mcp-allowlist.json` | Local allowlist file path |

## Built-in agents and tool whitelists

Agent profiles load from `agents/{type}.json`:

```json
{
  "agent_type": "researcher",
  "version": "0.1.0",
  "description": "Research agent",
  "system_prompt": "You are a careful research assistant.",
  "model": "MiniMax-M2.5",
  "temperature": 0.3,
  "max_steps": 20,
  "step_timeout_secs": 60,
  "tools": ["file_read", "search_files", "file_list"],
  "memory": {
    "type": "persistent",
    "namespace": "researcher",
    "message_persist": { "mode": "every_message" },
    "max_session_summaries": 3,
    "max_injected_events": 5,
    "enable_event_extraction": true
  },
  "context_window_tokens": 32768,
  "parallel_tools": 1
}
```

| Agent | Description | Tool whitelist |
|-------|-------------|----------------|
| `general` | General agent — files + shell + web + rule consumption and drafting (whitelist of 28 tools, incl. read-only `evolution_signals`) | file_* (6), search_files, grep_files, git_status/diff/log, shell_exec, http_get, rule_list/get/versions/version_get/validate/create/update/submit, knowledge_datasets/search/entry_get, audit_get, meta_summary, evolution_signals |
| `researcher` | Research agent — read-only search | file_read, search_files, file_list |
| `rule-copilot` | Rule-collaboration agent — 23 rule-management tools (whitelist, measured) | ws_list, ws_create, rule_list/get/create/update/versions/version_get/submit/activate/block/archive/fork/reload, rule_to_transform, rule_to_conditional, rule_validate, audit_get, audit_verify, session_rewind, meta_summary, evolution_signals, rule_promote |

### Usage tips for large workspaces

- **Run real tasks in a dedicated workdir**: don't point the agent at the source repo itself (which contains `target/` and other build artifacts); prefer `evo-agent run --workdir <your-project-dir>` on a clean project directory.
- **Prefer `file_read` for reading and `file_list` for listing**; use `search_files` / `grep_files` only when the file location is uncertain.
- **Workspace directories persist across sessions**: the agent-writable `workspace/` directory is not cleaned automatically after a session ends. Before automation scripts, batch drivers, or test harnesses reuse the same directory, clear it explicitly (delete the previous round's artifacts) or switch to a dedicated workdir — leftover files silently change the preconditions of later runs ("file already exists", "reading last round's content") and distort assertions and experiment conclusions.
