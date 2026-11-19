// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Acceptance evidence for the long-horizon `task_done` admission gate
//! (acceptance hardening batch).
//!
//! Four machine faces are assembled by the runner before a `task_done`
//! submission is evaluated, and the model never supplies any of them (same
//! decision-authority split as the runner-filled `acceptance_passed` flag):
//!
//! 1. **Touch surface projection** — the write-face checkpoint manifest
//!    (`.evo-checkpoint/manifest.jsonl`, the on-disk SSOT of the checkpoint
//!    store) is projected to a last-state-per-path touch list and injected
//!    into the `task_done` params under runner-owned keys. Any model-supplied
//!    value under those keys is overwritten, so the touch surface cannot be
//!    forged. The store itself is consumed strictly read-only; this module
//!    mirrors the documented manifest line contract and never writes it.
//! 2. **Anchor coverage** — pure, deterministic statistics crossing the
//!    journal event anchors (file checkpoint events) with the artifact
//!    anchors (touch surface): how much of what was actually touched is
//!    accounted for by the audit trail. The difference between what the
//!    acceptance claims reference and the touched list is the report
//!    credibility signal.
//! 3. **Evidence digest recomputation** — digests recorded by evidence
//!    events are recomputed against the *current* value of their extant
//!    artifact (checkpoint blob bytes), never trusted from the ledger's
//!    self-report. A blob tampered with after capture recomputes to a
//!    different digest and the submission is refused.
//!    Declared design behavior: verify-run output digests have no extant
//!    artifact by design (verify output is ephemeral — only the digest is
//!    journaled), so they are simply not part of the recomputable sample
//!    rather than silently counted either way; a live manifest entry whose
//!    blob is gone means the checkpoint store was altered externally and is
//!    refused (fail-closed). An empty touch surface (verify-only evidence,
//!    nothing written this session) leaves the sample vacuously empty —
//!    there is no artifact to tamper-check, and this is not a pass or a
//!    fail of anything.
//! 4. **Dual-run determinism and the assembly self-check** — in long-horizon
//!    mode the acceptance command runs twice and both stdout bytes and exit
//!    code must be bit-identical (a divergence means the criterion is not
//!    repeatable and the submission is honestly refused — discipline, not
//!    defect). Before the mode admits its first submission, three negative
//!    probes prove the gate actually works: a forged pass flag cannot grant
//!    admission, a known-bad criterion does go red, and a non-deterministic
//!    criterion is captured by the dual run. Any probe failing keeps the
//!    mode closed for the session (fail-closed — an idle gate that cannot
//!    go red must not pretend to protect anything).
//!
//! Non-long-horizon runners never build any of these faces; the acceptance
//! gate keeps its legacy criteria-only behavior byte-identically (default
//! invariant).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::acceptance::apply_acceptance_gate;
use crate::agent::acceptance::GateOutcome;
use crate::agent::journal::JournalEvent;
use crate::agent::journal::JournalLine;
use crate::agent::workspace_checkpoint::KIND_TOMBSTONE;

/// Checkpoint store directory under the workspace root (store layout
/// contract; consumed read-only).
const CHECKPOINT_DIR: &str = ".evo-checkpoint";
/// Checkpoint manifest file name (on-disk SSOT of the snapshot stack).
const MANIFEST_FILE: &str = "manifest.jsonl";
/// Checkpoint blob directory name (content-addressed snapshot bytes).
const BLOBS_DIR: &str = "blobs";
/// Digest algorithm prefix shared with the journal/checkpoint digest
/// convention (`blake3:<hex>`).
const DIGEST_PREFIX: &str = "blake3:";
/// Cap for diagnostic lists rendered into reports (orphan anchors);
/// counts always report the full number.
const DIAG_LIST_CAP: usize = 20;

/// Default number of newest touch-surface entries spot-checked at
/// `task_done` time (evidence recomputation sample size).
pub const DEFAULT_SPOT_CHECK_LIMIT: usize = 3;

/// One touch-surface entry: a mirror of the checkpoint manifest line
/// (field contract documented in the checkpoint store module). Parsed
/// read-only from `manifest.jsonl`; the store keeps its own private
/// representation and is never modified from here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TouchSurfaceEntry {
    /// Stack sequence (monotone over the store lifetime)
    pub seq: u64,
    /// Ledger timestamp (unix ms)
    pub ts: u64,
    /// Write-face tool that triggered the snapshot
    pub tool: String,
    /// Workspace-relative path
    pub path: String,
    /// Snapshot kind (content | tombstone)
    pub kind: String,
    /// Snapshot digest (blake3:<hex>; tombstone = digest of empty bytes)
    pub hash: String,
    /// Snapshot byte count (tombstone = 0)
    pub bytes: u64,
    /// Associated journal line seq (None when the journal face is absent)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_seq: Option<u64>,
}

/// Read the checkpoint manifest from the workspace root. Fail-soft: a
/// missing file or a corrupt line is skipped with a warning (same
/// degradation discipline as the store's own manifest reader — the
/// snapshot face is not the governance authority, the journal is).
pub fn read_manifest_entries(workdir: &Path) -> Vec<TouchSurfaceEntry> {
    let root = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    let path = root.join(CHECKPOINT_DIR).join(MANIFEST_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| match serde_json::from_str::<TouchSurfaceEntry>(l) {
            Ok(e) => Some(e),
            Err(e) => {
                tracing::warn!("checkpoint manifest line skipped by acceptance projection: {e}");
                None
            }
        })
        .collect()
}

/// Project the touch surface: the last manifest entry per path wins
/// (stack order), emitted sorted by path for deterministic output. Same
/// input always yields the same list byte-for-byte (pure function).
pub fn project_touch_surface(entries: &[TouchSurfaceEntry]) -> Vec<TouchSurfaceEntry> {
    let mut latest: BTreeMap<String, TouchSurfaceEntry> = BTreeMap::new();
    for e in entries {
        latest.insert(e.path.clone(), e.clone());
    }
    latest.into_values().collect()
}

/// Normalize a path candidate for touch matching (backslashes folded to
/// forward slashes, trimmed) so Windows-side references match the
/// workspace-relative manifest convention.
fn normalize_path(p: &str) -> String {
    p.trim().replace('\\', "/")
}

/// Collect one path candidate from an anchors string. Recognized shapes:
/// a bare path, or the association form `path = hash` used by the
/// handover anchors convention. Anything else is taken verbatim
/// (best-effort extraction — the coverage figure is a credibility
/// signal, not an admission gate).
fn collect_path_candidate(s: &str, out: &mut BTreeSet<String>) {
    let t = s.trim();
    if t.is_empty() {
        return;
    }
    if let Some((p, _rest)) = t.split_once(" = ") {
        out.insert(normalize_path(p));
        return;
    }
    out.insert(normalize_path(t));
}

/// Extract the model-referenced paths from `task_done` args. Best-effort
/// and pure: reads `params.anchors` only (array of strings / objects with
/// a `path` key / path→hash map). The model cannot inflate this figure —
/// the touched side of the ratio is projected from the manifest, not from
/// the model's claims.
pub fn extract_referenced_paths(args: &serde_json::Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(anchors) = args.get("params").and_then(|p| p.get("anchors")) else {
        return out;
    };
    match anchors {
        serde_json::Value::Array(items) => {
            for item in items {
                match item {
                    serde_json::Value::String(s) => collect_path_candidate(s, &mut out),
                    serde_json::Value::Object(map) => {
                        if let Some(p) = map.get("path").and_then(|v| v.as_str()) {
                            out.insert(normalize_path(p));
                        }
                    }
                    _ => {}
                }
            }
        }
        serde_json::Value::Object(map) => {
            for k in map.keys() {
                out.insert(normalize_path(k));
            }
        }
        _ => {}
    }
    out
}

/// Touch coverage of the model's references against the projected touch
/// surface (report credibility signal; pure, deterministic).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TouchCoverage {
    /// Touched paths (projected surface size)
    pub touched: usize,
    /// Distinct path candidates the model referenced
    pub referenced_candidates: usize,
    /// Touched paths the model actually referenced
    pub covered: usize,
    /// Coverage percentage (vacuously 100 when nothing was touched)
    pub ratio_pct: u64,
    /// Touched paths missing from the model's references (sorted)
    pub unreferenced: Vec<String>,
}

/// Compute the touch coverage (pure). Zero touched paths is vacuous:
/// there is nothing to reference, so the ratio reports 100 with zero
/// covered paths rather than pretending a deficit.
pub fn compute_touch_coverage(
    surface: &[TouchSurfaceEntry],
    referenced: &BTreeSet<String>,
) -> TouchCoverage {
    let total = surface.len();
    let covered = surface
        .iter()
        .filter(|e| referenced.contains(&e.path))
        .count();
    // Vacuous on an empty surface: nothing to reference reports 100
    // (checked division keeps the zero-total face explicit).
    let ratio_pct = (covered * 100).checked_div(total).unwrap_or(100) as u64;
    let unreferenced = surface
        .iter()
        .filter(|e| !referenced.contains(&e.path))
        .map(|e| e.path.clone())
        .collect();
    TouchCoverage {
        touched: total,
        referenced_candidates: referenced.len(),
        covered,
        ratio_pct,
        unreferenced,
    }
}

/// Journal event anchors: `(path, hash)` pairs from file checkpoint
/// events, in chain order. Pure projection over journal lines.
pub fn journal_anchors(lines: &[JournalLine]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in lines {
        if let JournalEvent::FileCheckpointTaken { path, hash, .. } = &line.event {
            out.push((path.clone(), hash.clone()));
        }
    }
    out
}

/// Journal-anchor coverage against the artifact anchors (touch surface):
/// how much of what was actually touched carries a journal anchor, and
/// whether the anchored digests agree with the artifact's recorded
/// digest. Pure, deterministic, machine-computable.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AnchorCoverage {
    /// Journal anchor events seen (chain order occurrences)
    pub journal_anchor_count: usize,
    /// Artifact anchors (projected touch surface size)
    pub artifact_count: usize,
    /// Artifact paths carrying at least one journal anchor
    pub covered: usize,
    /// Covered paths whose latest journal anchor digest equals the
    /// artifact's recorded digest
    pub hash_agreed: usize,
    /// Coverage percentage (vacuously 100 when nothing was touched)
    pub ratio_pct: u64,
    /// Artifact paths with no journal anchor (sorted)
    pub uncovered_artifacts: Vec<String>,
    /// Journal-anchored paths absent from the artifact surface (sorted;
    /// capped diagnostic list — e.g. LRU-evicted checkpoint entries leave
    /// journal anchors without a live manifest row, which is by design)
    pub orphan_journal_anchors: Vec<String>,
    /// Full count of orphan journal paths (the list above is capped)
    pub orphan_journal_anchor_count: usize,
}

/// Compute the journal-anchor coverage (pure). The latest journal anchor
/// per path wins (chain order, same convention as the write-off evidence
/// projection).
pub fn compute_anchor_coverage(
    journal: &[(String, String)],
    artifacts: &[TouchSurfaceEntry],
) -> AnchorCoverage {
    let mut latest: BTreeMap<String, String> = BTreeMap::new();
    for (p, h) in journal {
        latest.insert(p.clone(), h.clone());
    }
    let total = artifacts.len();
    let mut covered = 0usize;
    let mut agreed = 0usize;
    let mut uncovered_artifacts = Vec::new();
    for e in artifacts {
        match latest.get(&e.path) {
            Some(h) => {
                covered += 1;
                if *h == e.hash {
                    agreed += 1;
                }
            }
            None => uncovered_artifacts.push(e.path.clone()),
        }
    }
    let orphans: Vec<String> = latest
        .keys()
        .filter(|p| !artifacts.iter().any(|e| &e.path == *p))
        .cloned()
        .collect();
    let orphan_count = orphans.len();
    let ratio_pct = (covered * 100).checked_div(total).unwrap_or(100) as u64;
    AnchorCoverage {
        journal_anchor_count: journal.len(),
        artifact_count: total,
        covered,
        hash_agreed: agreed,
        ratio_pct,
        uncovered_artifacts,
        orphan_journal_anchors: orphans.into_iter().take(DIAG_LIST_CAP).collect(),
        orphan_journal_anchor_count: orphan_count,
    }
}

/// Face of one digest recomputation against its extant artifact.
#[derive(Debug, Clone, PartialEq)]
pub enum DigestFace {
    /// Recomputed digest equals the recorded digest
    Match,
    /// The extant artifact recomputes to a different digest (tampered or
    /// corrupted after capture)
    Mismatch {
        /// Digest as recorded by the ledger
        recorded: String,
        /// Digest recomputed over the current artifact bytes
        recomputed: String,
    },
    /// No extant artifact to recompute over (verify-run output is
    /// ephemeral by design; a live manifest entry with a missing blob
    /// means external alteration and is refused by the caller)
    ArtifactMissing,
}

/// Recompute a recorded digest against the current artifact bytes (pure
/// over the given bytes). `None` = no extant artifact. The digest
/// convention is `blake3:<hex>` over the exact artifact bytes — the same
/// recomputation discipline as the checkpoint store's undo path (never
/// trust the recorded value, recompute).
pub fn verify_digest_against_artifact(recorded: &str, actual: Option<&[u8]>) -> DigestFace {
    let Some(bytes) = actual else {
        return DigestFace::ArtifactMissing;
    };
    let recomputed = format!("{}{}", DIGEST_PREFIX, blake3::hash(bytes).to_hex());
    if recomputed == recorded {
        DigestFace::Match
    } else {
        DigestFace::Mismatch {
            recorded: recorded.to_string(),
            recomputed,
        }
    }
}

/// Blob file path for a recorded digest (content-addressed: the hex
/// segment of the digest under the checkpoint blob directory). `None`
/// when the digest carries no recognizable hex segment.
fn blob_path_for(workdir: &Path, hash: &str) -> Option<PathBuf> {
    let hex = hash.strip_prefix(DIGEST_PREFIX)?;
    if hex.is_empty() {
        return None;
    }
    let root = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    Some(root.join(CHECKPOINT_DIR).join(BLOBS_DIR).join(hex))
}

/// Evidence integrity verdict for the spot-check sample.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum IntegrityVerdict {
    /// Every sampled face matched (an empty touch surface is vacuously
    /// ok — there is no artifact to tamper-check; not a pass of anything)
    Ok {
        /// Faces successfully recomputed
        checked: usize,
        /// True when the touch surface was empty (vacuous sample)
        empty_sample: bool,
    },
    /// At least one sampled face mismatched or its artifact is gone —
    /// the ledger's recorded evidence no longer matches the disk
    Reject {
        /// Itemized failing faces (one line each)
        faces: Vec<String>,
    },
}

/// Spot-check the touch surface: sample up to `limit` newest entries and
/// recompute each recorded digest against the extant artifact. Content
/// entries read their blob from the checkpoint store; tombstone entries
/// recompute over empty bytes (a tombstone never has a blob file — the
/// pre-capture of a previously-absent path stores no content). Any
/// mismatch or missing blob refuses (fail-closed): the caller treats
/// [`IntegrityVerdict::Reject`] as a `task_done` refusal.
pub fn spot_check_evidence(
    workdir: &Path,
    surface: &[TouchSurfaceEntry],
    limit: usize,
) -> IntegrityVerdict {
    if surface.is_empty() {
        return IntegrityVerdict::Ok {
            checked: 0,
            empty_sample: true,
        };
    }
    let mut sample: Vec<&TouchSurfaceEntry> = surface.iter().collect();
    sample.sort_by_key(|e| std::cmp::Reverse(e.seq));
    sample.truncate(limit);
    let mut faces = Vec::new();
    let mut checked = 0usize;
    for e in &sample {
        let actual = if e.kind == KIND_TOMBSTONE {
            Some(Vec::new())
        } else {
            blob_path_for(workdir, &e.hash).and_then(|p| std::fs::read(&p).ok())
        };
        match verify_digest_against_artifact(&e.hash, actual.as_deref()) {
            DigestFace::Match => checked += 1,
            DigestFace::Mismatch {
                recorded,
                recomputed,
            } => faces.push(format!(
                "spot-check mismatch for '{}' (seq {}): recorded {recorded} vs recomputed {recomputed}",
                e.path, e.seq
            )),
            DigestFace::ArtifactMissing => faces.push(format!(
                "spot-check artifact missing for '{}' (seq {}): the checkpoint store was altered externally or is corrupt",
                e.path, e.seq
            )),
        }
    }
    if faces.is_empty() {
        IntegrityVerdict::Ok {
            checked,
            empty_sample: false,
        }
    } else {
        IntegrityVerdict::Reject { faces }
    }
}

/// Full runner-computed acceptance evidence report handed to the gate.
/// Every field is projected from machine sources (checkpoint manifest
/// SSOT, session journal, checkpoint blobs) — the model supplies none of
/// it, and runner-owned `task_done` params keys are overwritten on
/// injection so the model cannot forge the surface.
#[derive(Debug, Clone, Serialize)]
pub struct TouchReport {
    /// Projected touch surface (last manifest state per path, sorted)
    pub surface: Vec<TouchSurfaceEntry>,
    /// Model-reference coverage against the surface (credibility signal)
    pub coverage: TouchCoverage,
    /// Journal-anchor coverage against the surface (`None` = journal
    /// absent, the statistic is unknowable and reported as such)
    pub journal_anchor_coverage: Option<AnchorCoverage>,
    /// Evidence spot-check verdict (digest recomputation over blobs)
    pub integrity: IntegrityVerdict,
}

/// Inject the report into `task_done` params under runner-owned keys.
/// Overwrite semantics: any model-supplied value under these keys is
/// replaced — the touch surface is runner-projected and cannot be forged
/// (same decision-authority discipline as the runner-filled
/// `acceptance_passed` flag).
pub fn inject_touch_report(args: &mut serde_json::Value, report: &TouchReport) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    let params = obj.entry("params").or_insert_with(|| serde_json::json!({}));
    let Some(p) = params.as_object_mut() else {
        return;
    };
    p.insert(
        "touch_surface".to_string(),
        serde_json::to_value(&report.surface).unwrap_or_else(|_| serde_json::json!([])),
    );
    p.insert(
        "touch_coverage".to_string(),
        serde_json::to_value(&report.coverage).unwrap_or_else(|_| serde_json::json!({})),
    );
    p.insert(
        "journal_anchor_coverage".to_string(),
        match &report.journal_anchor_coverage {
            Some(c) => serde_json::to_value(c).unwrap_or(serde_json::json!(null)),
            None => serde_json::json!(null),
        },
    );
    p.insert(
        "evidence_integrity".to_string(),
        serde_json::to_value(&report.integrity).unwrap_or_else(|_| serde_json::json!({})),
    );
}

/// Rejection suffix listing the unreferenced touch paths (report
/// credibility signal rendered on every `task_done` refusal). Empty
/// string when nothing was touched or coverage is complete — the list
/// only appears when there is a deficit to name.
pub fn coverage_rejection_line(report: &TouchReport) -> String {
    if report.coverage.touched == 0 || report.coverage.unreferenced.is_empty() {
        return String::new();
    }
    format!(
        " Touch coverage {}/{} ({}%); unreferenced touch paths: [{}]",
        report.coverage.covered,
        report.coverage.touched,
        report.coverage.ratio_pct,
        report.coverage.unreferenced.join(", ")
    )
}

/// Whether two acceptance-command runs are bit-identical on the faces
/// the dual-run discipline checks (exit code + stdout bytes). Pure.
pub fn dual_run_bit_identical(
    first_exit: Option<i32>,
    first_stdout: &[u8],
    second_exit: Option<i32>,
    second_stdout: &[u8],
) -> bool {
    first_exit == second_exit && first_stdout == second_stdout
}

/// Result of the assembly self-check: three negative probes that must all
/// pass before the long-horizon mode opens (fail-closed).
#[derive(Debug, Clone)]
pub struct AssemblySelfProof {
    /// Probe 1: a model-forged `acceptance_passed=true` could not grant
    /// admission against a failing criterion
    pub forged_flag_rejected: bool,
    /// Probe 2: a known-bad criterion (deliberate non-zero exit) actually
    /// went red — the gate can refuse
    pub known_bad_criteria_red: bool,
    /// Probe 3: a non-deterministic criterion (two runs, diverging
    /// output) was captured by the dual-run discipline
    pub nondeterminism_captured: bool,
    /// Itemized failure lines for every probe that did not behave as
    /// designed (empty when all three passed)
    pub failures: Vec<String>,
}

impl AssemblySelfProof {
    /// All three probes passed and the mode may open. Any failure keeps
    /// the mode closed for the session (fail-closed).
    pub fn all_pass(&self) -> bool {
        self.forged_flag_rejected
            && self.known_bad_criteria_red
            && self.nondeterminism_captured
            && self.failures.is_empty()
    }
}

/// Non-deterministic probe command: output genuinely varies between two
/// process spawns, so a captured divergence proves the dual run executed
/// the command twice for real. Windows uses a random GUID (a time-based
/// `%RANDOM%` collides when both runs spawn within the same system tick —
/// measured on this repo's Windows test host); Unix uses the shell PID.
#[cfg(windows)]
pub const NONDETERMINISTIC_PROBE_CMD: &str =
    "powershell -NoProfile -Command [guid]::NewGuid().ToString()";
#[cfg(not(windows))]
pub const NONDETERMINISTIC_PROBE_CMD: &str = "echo $$-$$-$$";

/// Run the three assembly self-check probes through the real gate
/// machinery. Every probe spawns actual processes — a probe can only
/// pass if the gate genuinely executed the criterion command and judged
/// its exit/output (an idle gate that never ran anything cannot diverge
/// two runs or go red on `exit 1`).
pub async fn run_assembly_selfproof() -> AssemblySelfProof {
    let mut sp = AssemblySelfProof {
        forged_flag_rejected: false,
        known_bad_criteria_red: false,
        nondeterminism_captured: false,
        failures: Vec::new(),
    };
    // Satisfied journal pieces so the probes exercise the long-horizon
    // admission path (conjunction + dual run), not the legacy shortcut.
    let pieces = crate::agent::writeoff::JournalPieces {
        evidence: crate::agent::writeoff::EvidenceState::Present {
            digest: "assembly-self-probe".to_string(),
        },
        freshness: crate::agent::writeoff::FreshnessState::Fresh,
    };
    // Probe 1: forged pass flag + failing criterion — the runner-filled
    // truth must win, the forged flag must be ignored.
    let forged = serde_json::json!({
        "instruction_type": "task_done",
        "params": { "acceptance_passed": true }
    });
    match apply_acceptance_gate(Some("exit 7"), &forged, Some(&pieces), None).await {
        GateOutcome::Reject(_) => sp.forged_flag_rejected = true,
        GateOutcome::Allow(_) => sp.failures.push(
            "probe 1 (forged pass flag): the gate allowed a task_done whose criterion exits non-zero — runner-filled truth was not enforced"
                .to_string(),
        ),
    }
    // Probe 2: a deliberately broken criterion must actually go red.
    let clean = serde_json::json!({ "instruction_type": "task_done", "params": {} });
    match apply_acceptance_gate(Some("exit 1"), &clean, Some(&pieces), None).await {
        GateOutcome::Reject(d) if d.contains("acceptance criteria not met") => {
            sp.known_bad_criteria_red = true;
        }
        GateOutcome::Reject(d) => sp.failures.push(format!(
            "probe 2 (known-bad criterion): refused but without the criteria face: {d}"
        )),
        GateOutcome::Allow(_) => sp.failures.push(
            "probe 2 (known-bad criterion): the gate allowed a deliberately broken criterion — it cannot go red"
                .to_string(),
        ),
    }
    // Probe 3: a non-deterministic criterion must be captured by the dual
    // run and honestly refused.
    match apply_acceptance_gate(Some(NONDETERMINISTIC_PROBE_CMD), &clean, Some(&pieces), None).await
    {
        GateOutcome::Reject(d) if d.contains("non-deterministic") => {
            sp.nondeterminism_captured = true;
        }
        GateOutcome::Reject(d) => sp.failures.push(format!(
            "probe 3 (dual-run divergence): refused but the divergence was not attributed to non-determinism: {d}"
        )),
        GateOutcome::Allow(_) => sp.failures.push(
            "probe 3 (dual-run divergence): a non-deterministic criterion was allowed — the dual run did not capture the divergence"
                .to_string(),
        ),
    }
    sp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64, path: &str, kind: &str, hash: &str, bytes: u64) -> TouchSurfaceEntry {
        TouchSurfaceEntry {
            seq,
            ts: 1_000 + seq,
            tool: "file_write".to_string(),
            path: path.to_string(),
            kind: kind.to_string(),
            hash: hash.to_string(),
            bytes,
            journal_seq: None,
        }
    }

    fn digest_of(bytes: &[u8]) -> String {
        format!("{}{}", DIGEST_PREFIX, blake3::hash(bytes).to_hex())
    }

    fn fixture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("d4-accept-ev-{}-{name}", std::process::id(),));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(CHECKPOINT_DIR).join(BLOBS_DIR)).unwrap();
        dir
    }

    fn write_manifest(dir: &Path, lines: &[String]) {
        std::fs::write(
            dir.join(CHECKPOINT_DIR).join(MANIFEST_FILE),
            lines.join("\n") + "\n",
        )
        .unwrap();
    }

    fn manifest_line(seq: u64, path: &str, kind: &str, hash: &str, bytes: u64) -> String {
        format!(
            "{{\"seq\":{seq},\"ts\":{},\"tool\":\"file_write\",\"path\":\"{path}\",\"kind\":\"{kind}\",\"hash\":\"{hash}\",\"bytes\":{bytes}}}",
            1_000 + seq
        )
    }

    // ===== Touch surface projection: manifest direct read (no model input) =====

    #[test]
    fn test_read_manifest_entries_direct_read_and_fail_soft() {
        let dir = fixture_dir("manifest");
        let h = digest_of(b"hello");
        // Manifest line contract mirrored read-only; corrupt line skipped.
        write_manifest(
            &dir,
            &[
                manifest_line(1, "a.txt", "content", &h, 5),
                "{not json".to_string(),
                manifest_line(2, "b.txt", "tombstone", &digest_of(&[]), 0),
            ],
        );
        let entries = read_manifest_entries(&dir);
        assert_eq!(entries.len(), 2, "corrupt line skipped, good lines kept");
        assert_eq!(entries[0].path, "a.txt");
        assert_eq!(entries[0].hash, h);
        assert_eq!(entries[1].kind, "tombstone");
        // Missing file = empty face (fail-soft).
        let empty = read_manifest_entries(Path::new("/definitely/not/a/real/workspace-root"));
        assert!(empty.is_empty());
    }

    #[test]
    fn test_project_touch_surface_last_per_path_deterministic() {
        let h1 = digest_of(b"v1");
        let h2 = digest_of(b"v2");
        let entries = vec![
            entry(1, "b.txt", "content", &h1, 2),
            entry(2, "a.txt", "content", &h1, 2),
            entry(3, "a.txt", "content", &h2, 2),
        ];
        let surface = project_touch_surface(&entries);
        // Last entry per path wins; output sorted by path.
        assert_eq!(surface.len(), 2);
        assert_eq!(surface[0].path, "a.txt");
        assert_eq!(surface[0].hash, h2, "latest state wins");
        assert_eq!(surface[1].path, "b.txt");
        // Deterministic: same input twice → byte-stable output.
        let again = project_touch_surface(&entries);
        assert_eq!(surface, again);
    }

    // ===== Coverage pure functions =====

    #[test]
    fn test_touch_coverage_five_touched_three_referenced() {
        // Design fixture: 5 touched / 3 referenced = 60%, unreferenced listed.
        let h = digest_of(b"x");
        let surface: Vec<TouchSurfaceEntry> = ["p1", "p2", "p3", "p4", "p5"]
            .iter()
            .enumerate()
            .map(|(i, p)| entry(i as u64 + 1, p, "content", &h, 1))
            .collect();
        let referenced: BTreeSet<String> =
            ["p1", "p2", "p3"].iter().map(|s| s.to_string()).collect();
        let cov = compute_touch_coverage(&surface, &referenced);
        assert_eq!(cov.touched, 5);
        assert_eq!(cov.covered, 3);
        assert_eq!(cov.ratio_pct, 60);
        assert_eq!(cov.unreferenced, vec!["p4".to_string(), "p5".to_string()]);
    }

    #[test]
    fn test_touch_coverage_vacuous_and_extraction_shapes() {
        let cov = compute_touch_coverage(&[], &BTreeSet::new());
        assert_eq!(cov.ratio_pct, 100, "vacuous: nothing to reference");
        assert_eq!(cov.covered, 0);
        // Extraction shapes: bare path / "path = hash" / object / map /
        // backslash normalization — and forged keys elsewhere are ignored.
        let args = serde_json::json!({
            "instruction_type": "task_done",
            "params": {
                "anchors": [
                    "src/a.txt",
                    "docs/b.md = blake3:abcd",
                    {"path": "lib/c.rs"},
                    "",
                    "  src/a.txt  "
                ]
            }
        });
        let refs = extract_referenced_paths(&args);
        assert!(refs.contains("src/a.txt"));
        assert!(refs.contains("docs/b.md"));
        assert!(refs.contains("lib/c.rs"));
        assert_eq!(refs.len(), 3, "dedup + empty dropped");
        // No anchors field at all → empty reference set.
        let bare = serde_json::json!({"instruction_type": "task_done", "params": {}});
        assert!(extract_referenced_paths(&bare).is_empty());
    }

    #[test]
    fn test_anchor_coverage_machine_computable() {
        let ha = digest_of(b"a");
        let hb = digest_of(b"b");
        let journal = vec![
            ("src/a.txt".to_string(), ha.clone()),
            ("src/gone.txt".to_string(), hb.clone()),
            ("src/a.txt".to_string(), hb.clone()), // latest wins below
        ];
        let artifacts = vec![
            entry(1, "src/a.txt", "content", &hb, 1), // agrees with latest
            entry(2, "src/b.txt", "content", &hb, 1), // no journal anchor
        ];
        let cov = compute_anchor_coverage(&journal, &artifacts);
        assert_eq!(cov.journal_anchor_count, 3);
        assert_eq!(cov.artifact_count, 2);
        assert_eq!(cov.covered, 1);
        assert_eq!(cov.hash_agreed, 1, "latest journal anchor digest agrees");
        assert_eq!(cov.ratio_pct, 50);
        assert_eq!(cov.uncovered_artifacts, vec!["src/b.txt".to_string()]);
        assert_eq!(cov.orphan_journal_anchor_count, 1);
        assert_eq!(cov.orphan_journal_anchors, vec!["src/gone.txt".to_string()]);
    }

    #[test]
    fn test_journal_anchors_projection_pure() {
        let line = |seq: u64, path: &str, hash: &str| JournalLine {
            seq,
            ts: 1,
            event: JournalEvent::FileCheckpointTaken {
                path: path.to_string(),
                kind: "content".to_string(),
                hash: hash.to_string(),
                bytes: 1,
                tool: "file_write".to_string(),
            },
            hashes: None,
        };
        let other = JournalLine {
            seq: 9,
            ts: 1,
            event: JournalEvent::VerifyRan {
                name: "n".to_string(),
                command: "c".to_string(),
                exit_code: Some(0),
                error_count: 0,
                diags_digest: "d".to_string(),
                verdict: "pass".to_string(),
                output_digest: "o".to_string(),
            },
            hashes: None,
        };
        let lines = vec![line(1, "a", "h1"), other, line(2, "a", "h2")];
        let anchors = journal_anchors(&lines);
        assert_eq!(
            anchors,
            vec![
                ("a".to_string(), "h1".to_string()),
                ("a".to_string(), "h2".to_string())
            ],
            "verify events are not anchors; checkpoint events project in chain order"
        );
    }

    // ===== Evidence digest recomputation (tamper detection) =====

    #[test]
    fn test_verify_digest_faces() {
        let bytes = b"hello";
        let good = digest_of(bytes);
        assert_eq!(
            verify_digest_against_artifact(&good, Some(bytes)),
            DigestFace::Match
        );
        assert!(matches!(
            verify_digest_against_artifact(&good, None),
            DigestFace::ArtifactMissing
        ));
        let mutated = b"hellO";
        assert!(matches!(
            verify_digest_against_artifact(&good, Some(&mutated[..])),
            DigestFace::Mismatch { .. }
        ));
    }

    #[test]
    fn test_spot_check_honest_fixture_and_empty_surface() {
        let dir = fixture_dir("honest");
        let h = digest_of(b"hello");
        let ht = digest_of(&[]);
        write_manifest(
            &dir,
            &[
                manifest_line(1, "a.txt", "content", &h, 5),
                manifest_line(2, "b.txt", "tombstone", &ht, 0),
            ],
        );
        std::fs::write(
            dir.join(CHECKPOINT_DIR)
                .join(BLOBS_DIR)
                .join(h.strip_prefix(DIGEST_PREFIX).unwrap()),
            b"hello",
        )
        .unwrap();
        let entries = read_manifest_entries(&dir);
        let surface = project_touch_surface(&entries);
        let verdict = spot_check_evidence(&dir, &surface, DEFAULT_SPOT_CHECK_LIMIT);
        assert_eq!(
            verdict,
            IntegrityVerdict::Ok {
                checked: 2,
                empty_sample: false
            },
            "content blob matches and tombstone recomputes over empty bytes"
        );
        // Empty surface = vacuous sample (verify-only evidence session).
        assert_eq!(
            spot_check_evidence(&dir, &[], DEFAULT_SPOT_CHECK_LIMIT),
            IntegrityVerdict::Ok {
                checked: 0,
                empty_sample: true
            }
        );
    }

    #[test]
    fn test_spot_check_tampered_blob_detected() {
        // Negative: after-capture blob tampering must be detected by
        // recomputation — the recorded digest is never trusted.
        let dir = fixture_dir("tamper");
        let h = digest_of(b"hello");
        write_manifest(&dir, &[manifest_line(1, "a.txt", "content", &h, 5)]);
        let blob = dir
            .join(CHECKPOINT_DIR)
            .join(BLOBS_DIR)
            .join(h.strip_prefix(DIGEST_PREFIX).unwrap());
        std::fs::write(&blob, b"hello").unwrap();
        let entries = read_manifest_entries(&dir);
        let surface = project_touch_surface(&entries);
        assert!(matches!(
            spot_check_evidence(&dir, &surface, 3),
            IntegrityVerdict::Ok { .. }
        ));
        // Tamper one byte on disk.
        std::fs::write(&blob, b"hellO").unwrap();
        match spot_check_evidence(&dir, &surface, 3) {
            IntegrityVerdict::Reject { faces } => {
                assert_eq!(faces.len(), 1);
                assert!(faces[0].contains("a.txt"), "{}", faces[0]);
                assert!(faces[0].contains("mismatch"), "{}", faces[0]);
            }
            other => panic!("tampered blob must reject, got {other:?}"),
        }
    }

    #[test]
    fn test_spot_check_missing_blob_and_limit() {
        // Negative: a live manifest entry whose blob is gone = external
        // alteration → refuse (fail-closed).
        let dir = fixture_dir("missing");
        let h1 = digest_of(b"one");
        let h2 = digest_of(b"two");
        write_manifest(
            &dir,
            &[
                manifest_line(1, "old.txt", "content", &h1, 3),
                manifest_line(2, "new.txt", "content", &h2, 3),
            ],
        );
        std::fs::write(
            dir.join(CHECKPOINT_DIR)
                .join(BLOBS_DIR)
                .join(h1.strip_prefix(DIGEST_PREFIX).unwrap()),
            b"one",
        )
        .unwrap();
        // Blob for seq 2 (newest) deliberately NOT written.
        let entries = read_manifest_entries(&dir);
        let surface = project_touch_surface(&entries);
        match spot_check_evidence(&dir, &surface, DEFAULT_SPOT_CHECK_LIMIT) {
            IntegrityVerdict::Reject { faces } => {
                assert!(faces[0].contains("new.txt"), "{}", faces[0]);
                assert!(faces[0].contains("missing"), "{}", faces[0]);
            }
            other => panic!("missing blob must reject, got {other:?}"),
        }
        // Sample-size limit: newest-first; limit 1 checks only seq 2
        // (which is missing) — limit 1 against a repaired store checks
        // exactly one face.
        std::fs::write(
            dir.join(CHECKPOINT_DIR)
                .join(BLOBS_DIR)
                .join(h2.strip_prefix(DIGEST_PREFIX).unwrap()),
            b"two",
        )
        .unwrap();
        assert_eq!(
            spot_check_evidence(&dir, &surface, 1),
            IntegrityVerdict::Ok {
                checked: 1,
                empty_sample: false
            },
            "limit 1 samples only the newest entry"
        );
    }

    // ===== Dual-run comparator =====

    #[test]
    fn test_dual_run_bit_identical_comparator() {
        assert!(dual_run_bit_identical(Some(0), b"ok", Some(0), b"ok"));
        assert!(!dual_run_bit_identical(Some(0), b"ok", Some(1), b"ok"));
        assert!(!dual_run_bit_identical(Some(0), b"ok", Some(0), b"ok2"));
        assert!(
            dual_run_bit_identical(None, b"", None, b""),
            "spawn-failure faces compare like any other face"
        );
    }

    // ===== Assembly self-check (three negative probes, real runs) =====

    #[tokio::test]
    async fn test_assembly_selfproof_three_negatives_all_pass() {
        // Integration: the three probes run real gate evaluations with
        // real processes. Probe 3 can only pass if the criterion command
        // genuinely executed twice with diverging output, so a green
        // result here is itself the proof that the probes ran (fake-green
        // guard); the timing assertion adds the elapsed-evidence guard.
        let t0 = std::time::Instant::now();
        let sp = run_assembly_selfproof().await;
        assert!(
            sp.all_pass(),
            "all three probes must pass on a working gate: {:?}",
            sp.failures
        );
        assert!(sp.forged_flag_rejected && sp.known_bad_criteria_red && sp.nondeterminism_captured);
        assert!(
            t0.elapsed() >= std::time::Duration::from_millis(1),
            "probes must spawn real processes (elapsed {:?})",
            t0.elapsed()
        );
    }

    #[test]
    fn test_assembly_selfproof_fail_closed_decision() {
        // A probe that did not pass keeps the mode closed (fail-closed).
        let mut sp = AssemblySelfProof {
            forged_flag_rejected: true,
            known_bad_criteria_red: true,
            nondeterminism_captured: true,
            failures: Vec::new(),
        };
        assert!(sp.all_pass());
        sp.nondeterminism_captured = false;
        assert!(!sp.all_pass(), "any failed probe keeps the mode shut");
        sp.failures.push("synthetic".to_string());
        assert!(!sp.all_pass());
    }

    // ===== Report injection: runner-owned keys, overwrite semantics =====

    #[test]
    fn test_inject_touch_report_overwrites_forged_values() {
        let h = digest_of(b"x");
        let surface = vec![entry(1, "a.txt", "content", &h, 1)];
        let referenced: BTreeSet<String> = ["a.txt"].iter().map(|s| s.to_string()).collect();
        let report = TouchReport {
            surface: surface.clone(),
            coverage: compute_touch_coverage(&surface, &referenced),
            journal_anchor_coverage: None,
            integrity: IntegrityVerdict::Ok {
                checked: 1,
                empty_sample: false,
            },
        };
        let mut args = serde_json::json!({
            "instruction_type": "task_done",
            "params": {
                "touch_surface": "FORGED-BY-MODEL",
                "touch_coverage": {"ratio_pct": 100}
            }
        });
        inject_touch_report(&mut args, &report);
        assert_eq!(
            args["params"]["touch_surface"][0]["path"],
            serde_json::json!("a.txt"),
            "model-supplied surface replaced by the manifest projection"
        );
        assert_eq!(
            args["params"]["touch_coverage"]["ratio_pct"],
            serde_json::json!(100)
        );
        assert_eq!(
            args["params"]["touch_coverage"]["touched"],
            serde_json::json!(1)
        );
        assert_eq!(
            args["params"]["journal_anchor_coverage"],
            serde_json::json!(null),
            "journal absent → statistic reported as unknowable, not fabricated"
        );
        assert_eq!(
            args["params"]["evidence_integrity"]["verdict"],
            serde_json::json!("ok")
        );
    }

    #[test]
    fn test_coverage_rejection_line_lists_deficit_only() {
        let h = digest_of(b"x");
        let surface: Vec<TouchSurfaceEntry> = ["p1", "p2", "p3", "p4", "p5"]
            .iter()
            .enumerate()
            .map(|(i, p)| entry(i as u64 + 1, p, "content", &h, 1))
            .collect();
        let referenced: BTreeSet<String> =
            ["p1", "p2", "p3"].iter().map(|s| s.to_string()).collect();
        let report = TouchReport {
            surface: surface.clone(),
            coverage: compute_touch_coverage(&surface, &referenced),
            journal_anchor_coverage: None,
            integrity: IntegrityVerdict::Ok {
                checked: 0,
                empty_sample: false,
            },
        };
        let line = coverage_rejection_line(&report);
        assert!(line.contains("3/5"), "{line}");
        assert!(line.contains("60%"), "{line}");
        assert!(line.contains("p4") && line.contains("p5"), "{line}");
        // Full coverage → no deficit line.
        let all: BTreeSet<String> = ["p1", "p2", "p3", "p4", "p5"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let full = TouchReport {
            coverage: compute_touch_coverage(&surface, &all),
            ..report
        };
        assert!(coverage_rejection_line(&full).is_empty());
        // Empty surface → no deficit line (vacuous).
        let empty = TouchReport {
            surface: vec![],
            coverage: compute_touch_coverage(&[], &BTreeSet::new()),
            journal_anchor_coverage: None,
            integrity: IntegrityVerdict::Ok {
                checked: 0,
                empty_sample: true,
            },
        };
        assert!(coverage_rejection_line(&empty).is_empty());
    }
}
