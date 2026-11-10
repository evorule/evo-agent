# Skills System Reference

<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

Complete reference for the evo-agent skills system: the governed supply-chain model, the two provisioning paths, the three-state ledger with journal deepening, freshness stamps, the approval gates, and operations guidance. The bundled [skill-authoring guide](../agents/skills/skill-authoring/SKILL.md) is the agent-facing quick reference; this page is the maintainer-facing design and operations manual.

## Design position: a governed supply chain, not a skill folder

Generic agent "skills" are typically plain prompt files in a folder: drop a file, it loads, anyone (including the model) can edit it. evo-agent treats skills as **operational knowledge with a governed supply chain**, aligned with the project's three-part doctrine: *creation belongs to the LLM, deterministic enforcement belongs to the machine, final rulings belong to the human.* That produces several deliberate departures from generic systems — each listed in [§12](#12-deliberate-differences-from-generic-skill-systems) with its rationale. The recurring consequence: **capabilities in this system are gated and attributable; conveniences are not.**

Two provisioning paths implement this:

1. **Declared skills** (definition-level): a human lists skills in an agent definition (`agents/*.json`, `skills` array). Declaration is the approval act — these bypass the ledger entirely and cannot be overridden by runtime registrations. Path: `agents/skills/<name>/SKILL.md`, referenced from the definition file.
2. **Registered skills** (runtime-level): files discovered by directory scan, gated through the registration ledger (`discovered → active/revoked`) with hash pinning, leak screening, an approval gate, and a full audit trail.

## Locations and tiers

| Tier | Directory | Gating | Scope |
|------|-----------|--------|-------|
| Declared | `agents/skills/<name>/SKILL.md` (referenced from `agents/<type>.json`) | Human declaration (bypasses ledger, highest priority) | Ships with the agent definition |
| Project | `<serve workdir>/.evo/skills/<name>/SKILL.md` | Registration ledger | Per-project |
| User | `<serve workdir>/data/skills/<name>/SKILL.md` | Registration ledger | Per-install, cross-agent |

The registration ledger is `data/skills/registry.json` (materialized snapshot) plus `data/skills/registry.chain.jsonl` (append-only journal — the source of truth, see [§7](#7-journal-deepening-tamper-evidence-and-self-heal)). The human-gate audit trail is `data/human_gate_ledger.jsonl`.

Management endpoints all live on the **serve port**; `/api/services` lives on the **engine port**. `GET /api/meta` returns this face's route families; other faces are listed in `runtime.json`. Cross-port calls return 404.

## Three-state model and session assembly

```
SKILL.md placed in a skills directory
        │ session creation: directory scan (frontmatter parse, name parity)
        ▼
   discovered ── POST /api/skills/approve ──► active ──► manifest + read_skill
        │            (leak screen, FFFD gate, hash pin,        │
        │             env stamp, approval gate, ledger)         │ content changed
        ▼                                                       ▼
   revoked ◄── POST /api/skills/revoke        hash mismatch at session creation
                                              → downgraded to discovered (stop
                                                notice surfaced in-manifest)
```

- `discovered`: scanned but not approved. Not in the manifest; `read_skill` cannot load it. Listed by `GET /api/skills/pending` with the full body for review.
- `active`: approved, hash pinned. Enters the manifest; loadable via `read_skill`.
- `revoked`: explicit rejection record (kept in the ledger for audit). Not loaded. Restore = put the SKILL.md back and approve again (idempotent upsert).
- **Downgraded** (transient): an active skill whose on-disk hash no longer matches the pinned hash is demoted to discovered at session creation, and a stop notice is surfaced in the manifest segment — demotion is never silent.

Session assembly (once, at runner construction) merges three sources by priority: **definition declarations > project-level active > user-level active**, name conflicts resolved to the higher priority with a warning. The merged manifest produces:

- the persistent skills manifest segment in the system prompt (one line per skill: `name: description`, plus a note that mid-session changes apply next session), and
- the `read_skill` tool, bound to name→path (and, for registered actives, to the pinned hash — see [§6](#6-read-time-verification-and-management-face-drift)).

The manifest segment also carries two notice kinds: a **stop notice** (skills downgraded this session) and an **environment notice** (stale env-sensitive skills, see [§8](#8-freshness-stamps)). Freshness notices have a user switch; stop notices do not.

## Lifecycle operations

| Scenario | Path |
|----------|------|
| Create (first) | `PUT /api/files/write` the SKILL.md → `POST /api/skills/approve {name, level}` — or `POST /api/skills/import` (bundle) |
| Update content | Overwrite the SKILL.md → `POST /api/skills/approve` (idempotent upsert; **never import** for an existing name) |
| Revoke | `POST /api/skills/revoke {name, level}` |
| Restore after revoke | Put the SKILL.md back → approve again (idempotent re-activation) |
| Export for promotion | `POST /api/skills/export {name, level}` → returns an import-format bundle (hash re-verified before export); a human reviews and promotes it into `agents/skills/` + a definition |
| Inspect | `GET /api/skills` (all states + `content_matches` drift flag), `GET /api/skills/pending`, `GET /api/meta` (route families) |

Import refuses to overwrite any active **or revoked** row of the same name (approved content is never silently replaced). The rejection message for revoked rows names the restore path. Registration requires the frontmatter `name:` to equal the directory name — enforced on both the import and the scan/approve paths (anti-phishing parity).

## Approval gate: leak screening, encoding, privilege classes

`POST /api/skills/approve` runs, in order:

1. **Write-face gate** — authentication must be enabled (anonymous/`--no-auth` mode is read-only; the denial is itself ledgered).
2. **Privilege-class gate** — when the `[auth] human_tokens` list is non-empty, only tokens on that list may approve/revoke/import; other valid tokens are operator-class and receive a 403 with configuration guidance. With the list empty the gate is off (pure-web / manual deployments: opt-in). **The desktop shell turns the gate on by default** — it provisions two per-boot tokens: the UI token (injected into the workbench webview, privileged class) and a separate ops token (never injected into the webview, operator class), so the approval gate is enforced out of the box.
3. **Frontmatter + name-parity validation** — unparseable frontmatter or a declared name that mismatches the directory name is rejected (the latter is the anti-phishing check, parity with import).
4. **Encoding gate** — content containing U+FFFD replacement characters (encoding corruption) is rejected 422 before the hash can pin corrupted bytes.
5. **Leak screening, two tiers** — authorship-declaration phrases (hard-block 422) and LLM-technical terms (warning, passes). Exemptions: a phrase wrapped entirely in corner brackets `「…」` is quotation context and degrades the hard block to a warning; warning hits adjacent to path/identifier characters (e.g. the product's own install root) are skipped.
6. **Hash pin + ledger commit** — blake3 over the file, appended to the journal, snapshot updated.

Every approve/revoke/import is recorded to the human-gate ledger with two attribution fields: `identity` (`human`/`anonymous` — did the call pass authentication) and `class` (`privileged`/`operator` — which token class performed it). The two fields together are what make human rulings attributable. File-face mutations (`/api/files/*` writes) carry the same `class` field, so a tool-chain write is distinguishable from a UI save in the ledger.

## Ledger deepening: journal, drift self-heal, signing

The ledger is deliberately split:

- `registry.chain.jsonl` — **append-only journal, the source of truth**. One record per management action (`approve`/`revoke`/`downgrade`/`bootstrap`/`session_start`), each carrying `seq`, `prev_hash`, the action payload, and `hash = blake3(prev_hash ‖ canonical(record))`.
- `registry.json` — **materialized snapshot** (what all existing consumers read). Rewritten after journal appends.

Invariants and behaviors:

- **Write order**: journal append (fsync) first, snapshot write second. A crash can only leave the journal ahead; on next load the replayed state wins and the snapshot is rebuilt (an approved action is never dropped by a crash).
- **Replay premise**: every action is an idempotent upsert/state-set, so replaying any prefix converges to the same state.
- **Torn tail**: a parse failure on the last line is treated as a crash during append — truncated with a warning. Damage anywhere before the last line fails visible (tamper signature).
- **Snapshot drift**: if the snapshot diverges from journal replay, load flags `journal_drift()` and logs; the next writer-side save rewrites the snapshot from the journal — out-of-band edits to the snapshot are thereby **ineffective and leave evidence**. The file face (`/api/files/write|delete`) hard-403s `skills/registry.json` so the usual edit channel is closed entirely.
- **Mid-chain tamper** (payload edited without recomputing hashes): load fails visible.
- **Signing (boot-scoped, when provisioned)**: the desktop shell generates a one-shot ed25519 seed per boot and hands it to serve via env (`EVO_AGENT_JOURNAL__SIGNING_KEY`, scrubbed once locked into process memory; the private key never touches disk or the command line). The first journal write of the boot records a `session_start` anchor carrying the boot public-key fingerprint; every subsequent record must carry a valid signature over its record hash — unsigned or mis-signed records after the anchor fail visible. Without a provisioned key the journal behaves exactly as the unsigned design.

**Honest ceiling**: an attacker able to rewrite files as the same OS user can still wipe journal and snapshot consistently and fall back to unsigned mode — that case is caught by the v1 layer (snapshot drift vs the human-gate ledger and the manifest stop notices), not by signatures. Blocking it outright requires the signing key to live outside every same-user-reachable boundary, which is the shell process; wiping the journal defeats only the shell-memory key, and the next boot's anchor re-arms detection from that point forward.

## Read-time verification and management-face drift

Approval pins content. Two enforcement points make the pin real:

- `read_skill` verifies the pinned hash **at load time** for registered active skills; content that changed since approval is refused with a "changed since approval" error until re-approved. Declared (definition-level) skills are exempt — declaration is the human gate for those.
- `GET /api/skills` recomputes file hashes for active skills and exposes `content_matches: false` on drift — the management face shows tampering instead of a stale pin. The GET face stays read-only; state transitions happen only at session creation or via management actions.

Together: change-after-approve cannot be served as approved content, in any session, through any skill channel.

## Freshness stamps

Integrity (hash) and currency (environment) are deliberately orthogonal: the hash answers "is this the approved content" (enforced), freshness answers "does the approved content still describe reality" (advisory).

- At approve time the ledger stamps an **environment fingerprint** (hostname ‖ serve version ‖ engine version from `runtime.json`) and the skill's `env-sensitive: true` frontmatter declaration.
- At session creation, env-sensitive actives whose fingerprint mismatches the current environment are surfaced as a re-verification notice in the manifest segment. They are **not** downgraded or blocked — old-environment conclusions may still hold; re-verification is the duty.
- User switch: workbench setting `skills.freshnessNotices` (default on). Off = the notice channel is silent; declarations and stamps still apply.
- Refresh = re-approve the same content (idempotent) — the stamp updates with the hash, zero extra approval forms.
- Ledger entries without a stamp (pre-feature) never trigger notices; stamps appear after the first re-approval.

## Configuration reference

| Key | Where | Meaning | Default |
|-----|-------|---------|---------|
| `[auth] enabled` | config / `EVO_AGENT_AUTH__ENABLED` | authentication on/off (off = read-only face) | false |
| `[auth] tokens` | config / `EVO_AGENT_AUTH__TOKENS` | valid API tokens | empty |
| `[auth] human_tokens` | config / `EVO_AGENT_AUTH__HUMAN_TOKENS` | privileged tokens; non-empty turns on the approval class gate | empty (gate off; the desktop shell always sets it to the UI token — gate on) |
| `skills.freshnessNotices` | workbench settings | freshness notice channel switch | true |
| `EVO_AGENT_JOURNAL__SIGNING_KEY` | env, handed by the desktop shell | boot-scoped journal signing seed (hex 32B); scrubbed from env after locking into memory | absent (unsigned mode) |

## Troubleshooting

| Symptom | Cause | Resolution |
|---------|-------|------------|
| Skill invisible in the manifest | created mid-session | manifest is session-scoped; it appears next session |
| Skill vanished after an edit | content changed without re-approve; demoted at session creation (stop notice in manifest) | `approve` again; load was also refused by read-time verification |
| `GET /api/skills` shows `content_matches: false` | file edited after approval | re-approve (or restore content); the flag clears when hashes match |
| approve/revoke/import returns 403 "privileged token" | approval class gate is on and the caller used an operator token | use a token listed in `[auth] human_tokens` |
| approve returns 403 "read-only" | authentication disabled (anonymous face) | start with `--auth-token` / `[auth] enabled` |
| approve returns 422 "U+FFFD" | encoding corruption in the body | rewrite the file (check the writer's encoding), re-approve |
| approve 422 "authorship phrases" | leak screen hard block; quotation-context exemption only covers full `「…」` wrapping | paraphrase, as the bundled authoring guide does |
| import rejected "revoked — put its SKILL.md back" | revoked row protection | restore the file, approve again; do not delete the skill directory |
| file face 403 on `skills/registry.json` | by design: the ledger is managed exclusively by `/api/skills` | use the management endpoints |
| journal load error "chain broken / hash mismatch / missing signature" | tamper or corruption, fail visible | restore from the human-gate ledger + skill files; rebuild by re-approving |
| cross-port 404 | face split (serve / engine / llm_executor) | `GET /api/meta` + `runtime.json` name the ports |

## Deliberate differences from generic skill systems

| Observation (often measured against generic standards) | Status in this system | Rationale |
|---|---|---|
| "No self-update hook: the system never calls the model back to revise a skill" | Deliberate boundary | Skill activation is a supply-chain grant; model-driven self-update would make the grant self-referential. The update path exists (edit + re-approve) and is human-gated when the class gate is on |
| "Approve required for every change" | Deliberate gate, mechanically enforced since the class gate + gate ledger landed | Human rulings on operational knowledge are final in the project doctrine; the ledger now attributes them (`identity` + `class`) |
| "Same-content re-approval needed after edits" | Deliberate idempotent path | Approval pins content via hash; re-approve re-pins. This is also the freshness refresh path |
| "Import refuses existing names" | Deliberate protection | Approved content is never silently replaced; update goes through re-approve, promotion through export |
| "Directory name must equal frontmatter name" | Deliberate anti-phishing parity on both import and scan paths | The directory name is the registration key; the declared name is what `read_skill` serves — divergence is the phishing shape |
| "Skill file writes are sandboxed away from the skills directory; the ledger file is fully closed" | Deliberate boundary (two layers) | Writes to skills go through governed channels; the ledger trust root is management-face-only. Bypassing via arbitrary code execution is separately red-lined and approval-gated |
| "Manifest injected once per session" | Deliberate static segment | Keeps the system-prompt prefix stable across turns; mid-session changes surface next session, and stop/env notices still reach the current session through the same segment |
| "Leak screen on skill content" | Deliberate product governance | Skills are shipped/curated knowledge; authorship-declaration phrases are blocked (with quotation-context exemption), technical vocabulary only warns |
| "Content hash pinned and enforced at load" | Deliberate integrity enforcement | The approval would be meaningless if the pinned content could drift silently; enforcement covers read time and the management face, with journal-backed self-heal |
| "Skills don't auto-expire" | Deliberate: freshness is advisory, integrity is enforced | Environment drift surfaces as a re-verification notice; whether old conclusions still hold is a judgment, not a mechanical demotion |

## Known ceilings

- Same-OS-user file-system attackers can rewrite journal and snapshot consistently (detection then relies on the v1 layer: gate-ledger cross-reference and manifest notices). Boot-scoped signatures make partial tampering and post-anchor appends detectable; wiping the journal entirely degrades to unsigned mode.
- The approval class gate is only as strong as token custody: with `human_tokens` configured, privileged tokens must not be reachable by the agent runtime (the desktop hands the runtime an operator-class token; the workbench UI and any human REST calls use the privileged token).
- Token theft is a residual surface, not a closed one: a caller that can read the workbench webview's storage can obtain the privileged UI token and pass the gate. Governance approval promises are therefore scoped to the **desktop form** (the UI token never leaves the shell-controlled webview; pure-web deployments run with the gate off unless configured, and their confirmation surface is weaker — configure `human_tokens` and keep tokens out of shared storage when serving beyond localhost).
- `read_skill` verifies registered skills only; raw file reads of a SKILL.md are plain file reads (content, not activation) and remain sandboxed as usual.

## Test anchors

| Behavior | Guarding test |
|----------|---------------|
| Read-time hash refusal | `builtin_tools::skill_read::tests::test_read_refuses_content_changed_since_approval` |
| Management-face drift flag, read-only | `api::skill_api::tests::test_inventory_flags_hash_mismatch_without_downgrade` |
| Snapshot drift detection + heal | `agent::skill_store::tests::test_journal_detects_snapshot_drift_and_heals_on_save` |
| Mid-chain tamper fail-visible | `agent::skill_store::tests::test_journal_mid_chain_tamper_fails_visible` |
| Torn-tail recovery | `agent::skill_store::tests::test_journal_torn_tail_recovers` |
| Legacy snapshot migration | `agent::skill_store::tests::test_journal_bootstrap_migrates_legacy_snapshot` |
| Replay across reload | `agent::skill_store::tests::test_journal_replays_across_reload` |
| Signed mode roundtrip + tamper | `agent::skill_store::tests::test_journal_signed_mode_roundtrip_and_tamper` |
| Freshness stamp + mismatch list | `agent::skill_store::tests::test_merge_flags_env_stale_only_for_sensitive_and_mismatched`, `api::skill_api::tests::test_approve_records_env_stamp_for_sensitive_skills` |
| Privilege gate + ledger class | `api::skill_api::tests::test_privileged_gate_blocks_operator_self_approval`, `test_approve_ledger_records_class_without_gate`, `api::file_api::tests::test_ledger_records_class_for_file_writes` |
| Desktop dual-token provisioning | `desktop::orchestrator::tests::serve_spawn_carries_auth_token_via_env_not_command_line` |
| Registry file-face protection | `api::file_api::tests::test_skill_ledger_denied_on_file_face` |
| Anti-phishing scan parity | `agent::skill_store::tests::test_scan_rejects_frontmatter_name_mismatch` |
| Revoked-name restore guidance | `api::skill_api::tests::test_import_revoked_name_points_to_restore_path` |

## File map

| Path | Role |
|------|------|
| `src/agent/skill_store.rs` | Scan, three-state ledger, journal (actions, chain, replay/self-heal), freshness merge, leak screen |
| `src/api/skill_api.rs` | REST management face (list/pending/approve/revoke/import/export), session-assembly entry, approval gates |
| `src/builtin_tools/skill_read.rs` | `read_skill` tool (name-bound, read-time hash verification) |
| `src/api/serve_tools.rs` | `wire_skills` / `wire_skills_with_hashes` (tool + manifest wiring) |
| `src/agent/assembly.rs` | Manifest segment rendering, stop/env notices, S4b slot |
| `src/api/ws_handler.rs` | Session-creation assembly call site |
| `src/api/human_gate.rs` | Caller identity/class, gate helpers, human-gate ledger |
| `src/api/auth.rs` | Token classes (`human_tokens`), bearer/ticket middleware, env scrub |
| `src/api/file_api.rs` | File face (registry.json deny) |
| `src/agent/journal_signing.rs` | Boot-scoped signing (init from env, sign/verify helpers) |
| `desktop/src-tauri/src/orchestrator.rs` | Shell-side boot key provisioning |
| `agents/skills/skill-authoring/SKILL.md` | Bundled agent-facing authoring guide |
