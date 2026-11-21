# Headless CI integration (`run --json`)

This document is the machine-facing contract for running `evo-agent run` in
headless environments (CI pipelines, schedulers, automation harnesses). It
describes the stdout event stream, the exit-code contract, and a minimal
GitHub Actions recipe.

## Event stream (`--json`)

```bash
evo-agent run --json "fix the failing test in src/lib.rs"
```

- With `--json`, every line on **stdout** is a single JSON object (NDJSON,
  one event per line, no blank lines) isomorphic to the WebSocket frames the
  `serve` API emits. Consumers must tolerate unknown event types and unknown
  fields (additive evolution).
- All human-oriented output (progress logs, the final summary, warnings) goes
  to **stderr**; stdout carries only the event stream.
- `--json-pretty` emits the same stream indented, for human inspection only;
  machine consumers should use `--json`.
- Machine-parse contract: every stdout line parses as JSON independently
  (`ConvertFrom-Json` / `jq -c` / `JSON.parse` line by line).

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Run completed and the acceptance check passed. |
| `1` | Agent execution failure: LLM error, tool-chain failure, or step budget exhausted. |
| `2` | Usage / configuration error: rejected CLI arguments, invalid workspace settings (including an invalid `session.profile` value), unsupported unattended approval modes, or shell sandbox configuration failures. |
| `3` | Reserved for output-schema validation failures (planned `--output-schema` feature; not currently emitted). |

Legacy exit codes are unchanged; `3` is additive and only future schema
validation will ever produce it.

## Coding profile selection

By default every run starts with the coding profile active: the system prompt
carries a rendered workspace map (bounded snapshot), a background code index
warm-up is scheduled (fail-soft), and the persistent service-catalog slot is
replaced by the workspace map.

Two opt-out / override channels exist:

| Channel | Effect |
|---------|--------|
| `run --profile coding` | Force the coding profile on, overriding workspace settings. |
| `run --profile manual` | Force the coding profile off — behavior is identical to runs before the coding-entry default. |
| `session.profile` in `{workdir}/.evo/settings.json` | `"coding"` = on (default when absent/null), `"manual"` = off. Any other value is a configuration error: the run aborts with exit code `2`. |

Priority: CLI flag > workspace settings > default (on).

## Minimal GitHub Actions recipe

```yaml
jobs:
  agent-task:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - name: Install evo-agent
        run: cargo install --path .
      - name: Run agent (headless)
        id: agent
        env:
          # LLM credentials reach the agent via provider environment
          # variables only (MINIMAX_API_KEY / DEEPSEEK_API_KEY / OPENAI_API_KEY
          # family, resolved env-first over config files; never committed).
          MINIMAX_API_KEY: ${{ secrets.MINIMAX_API_KEY }}
        run: |
          set -o pipefail
          evo-agent run --json --profile coding "${{ inputs.goal }}" \
            < /dev/null > events.ndjson 2> agent.log
      - name: Upload artifacts
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: agent-run
          path: |
            events.ndjson
            agent.log
```

Notes for CI authors:

- Unattended approval: the CLI approval path reads y/n from stdin, and stdin
  EOF is a denial — redirect `< /dev/null` so an interactive prompt can never
  hang the job. Unattended runs therefore require the agent definition to use
  an unattended approval mode (`auto_policy`); attended modes with
  `< /dev/null` simply deny every write-capable tool call.
- `set -o pipefail` keeps a mid-pipeline failure from being masked when you
  post-process `events.ndjson` on the fly.
- Branch on the exit code: `0` to proceed, `1` for agent failures (retry or
  alert), `2` for configuration mistakes (fail the workflow, never retry).
- Constrain the working tree before unattended runs: the run path honors the
  workspace approval configuration, so review `.evo/` in the repository when
  automating write-capable agents.
