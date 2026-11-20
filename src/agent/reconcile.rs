// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Cross-source reconciliation at the write-off moment (long-horizon mode).
//!
//! When a `task_done` submission reaches the acceptance gate, three
//! independent sources speak about the same work, and this module
//! cross-checks them with pure functions (zero IO, zero clock reads,
//! deterministic, fully unit-testable; the model supplies none of the
//! gate inputs — same decision-authority split as the write-off
//! conjunction):
//!
//! 1. **Assertion face** — what the model claims: the `task_done`
//!    `params.anchors` declarations (`path`, optionally with a claimed
//!    digest in the `path = hash` handover convention). These are
//!    free-form model claims, not machine-filtered values: inside
//!    long-horizon handover writes the fact fields are machine-drafted
//!    (the merge gate keeps machine truth), so the gate-reconcilable
//!    model assertion surface is exactly the anchors the model itself
//!    declares on the submission.
//! 2. **Machine ledger face** — what the journal window projects: file
//!    checkpoint anchors (latest hash per path, chain order, same
//!    convention as the checkpoint-block draft) and the newest verify
//!    run verdict in the window (chain-order tail, any verdict). A
//!    disabled or unreadable journal makes the face
//!    [`LedgerFace::Unestablished`] — assertions cannot be substantiated
//!    against it (unknown is never dressed up as a pass).
//! 3. **External criteria face** — the fresh acceptance-command run the
//!    gate just executed (the observable face; a single-source verdict
//!    no in-session claim can influence).
//!
//! Two comparisons are specified:
//!
//! - **Assertion ↔ ledger**: every declared anchor must exist in the
//!   ledger window and, when a digest is claimed, must match it. An
//!   anchor that references nothing the ledger recorded is an
//!   **off-ledger anchor** — a hard contradiction, listed with the exact
//!   path.
//! - **Ledger ↔ criteria**: the fresh criteria run and the window verify
//!   tail must agree on the pass/fail polarity. A green criterion over a
//!   red verify window is single-source noise: counted faithfully,
//!   never silently repaired, and never treated as a pass (the faces
//!   disagree, so the true state is unknowable from here). A window with
//!   no verify run at all is *not* a red verify window — the comparison
//!   is vacuous there, not a pass; and an unconfigured criterion face is
//!   vacuous too (the conjunction reports that piece separately).
//!
//! The report lists every discrepancy item-by-item (one entry per
//! disagreement, never folded into a summary, never silently dropped) —
//! the row count *is* the errata count, reported as-is for downstream
//! drift accounting. Zero new journal events: on a consistent report the
//! structured record rides the admitted `task_done` params under a
//! runner-owned key, and on an inconsistent one the itemized rows ride
//! the refusal feedback — both surfaces are already journaled by the
//! existing call/result events.
//!
//! On the ledger↔criteria face neither side is model-supplied (both are
//! machine faces); the mandated report field names are kept verbatim,
//! with `llm_claim` carrying the in-window ledger value and
//! `machine_fact` the fresh external run on that face.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::agent::journal::{JournalEvent, JournalLine};
use crate::agent::writeoff;

/// Face label of an assertion↔ledger discrepancy (a hard contradiction:
/// the claim contradicts the recorded ledger).
pub const FACE_ASSERTION_VS_LEDGER: &str = "assertion_vs_ledger";
/// Face label of a ledger↔criteria discrepancy (a cross-face polarity
/// disagreement: single-source noise, counted faithfully).
pub const FACE_LEDGER_VS_CRITERIA: &str = "ledger_vs_criteria";

/// Cap for claim/fact strings rendered into discrepancy rows (the gate's
/// own detail payloads are bounded already; this keeps rows bounded even
/// for adversarially long claims).
const ROW_TEXT_CAP: usize = 400;

/// One anchor claim on the assertion face.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorClaim {
    /// Workspace-relative path as claimed (normalized: backslashes folded
    /// to forward slashes, trimmed).
    pub path: String,
    /// Claimed digest, when the declaration carried one (`path = hash`
    /// form or a path→hash map entry). `None` = bare path claim; only
    /// ledger existence is checked for it.
    pub hash: Option<String>,
}

/// The assertion face: the model's own claims on the `task_done`
/// submission. Extracted by a pure reader of the submission args.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AssertionFace {
    /// Anchor declarations, in declaration order (duplicates preserved:
    /// two contradictory claims about one path are two rows, not one —
    /// nothing is folded).
    pub anchors: Vec<AnchorClaim>,
}

/// The verify-track tail inside the window (newest run in chain order,
/// any verdict).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyTail {
    /// Verify command name.
    pub name: String,
    /// Verdict as recorded (`pass` | `fail` | `unknown`).
    pub verdict: String,
}

/// The journal-window projection of the machine ledger face.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectedLedger {
    /// File checkpoint anchors: latest recorded hash per path inside the
    /// window (chain order, latest wins — the checkpoint-block draft
    /// convention).
    pub anchors: BTreeMap<String, String>,
    /// Newest verify run in the window, when the window holds one.
    pub verify_tail: Option<VerifyTail>,
}

/// The machine ledger face.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerFace {
    /// Projected from a readable session journal window.
    Projected(ProjectedLedger),
    /// The journal is disabled or unreadable: the ledger face cannot be
    /// established. Assertions cannot be substantiated against it — the
    /// comparison is honestly unresolvable there, never a silent pass.
    Unestablished,
}

/// The external criteria face: the fresh acceptance-command run result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CriteriaFace {
    /// The gate executed the criterion command in this admission.
    FreshRun {
        /// Exit-0 success of the fresh run.
        passed: bool,
        /// Machine-side detail (exit/stdout tails as composed by the gate;
        /// already bounded).
        detail: String,
    },
    /// No acceptance command is configured: the external face cannot be
    /// established in this admission (the conjunction reports that piece
    /// separately; the ledger↔criteria comparison is vacuous here).
    Unconfigured,
}

/// One cross-face disagreement, itemized. On the assertion↔ledger face
/// `llm_claim` is genuinely the model's claim; on the ledger↔criteria
/// face it carries the in-window ledger value (neither side is
/// model-supplied there — field names follow the mandated report
/// contract; see the module docs).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Discrepancy {
    /// Which cross-face comparison disagreed
    /// ([`FACE_ASSERTION_VS_LEDGER`] or [`FACE_LEDGER_VS_CRITERIA`]).
    pub face: String,
    /// The disputed field (`anchors`, `verify_verdict`, `ledger`).
    pub field: String,
    /// The claimed/asserted side value (bounded).
    pub llm_claim: String,
    /// The machine/external observable value (bounded).
    pub machine_fact: String,
}

/// The reconciliation report. `consistent` is true only when every
/// comparable face agreed; every disagreement appears in `discrepancies`
/// item-by-item — the row count is the errata count, reported as-is
/// (never folded, never silently repaired).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReconcileReport {
    /// True only when zero discrepancies were found.
    pub consistent: bool,
    /// Every disagreement, one entry each, in detection order.
    pub discrepancies: Vec<Discrepancy>,
}

/// Normalize a path candidate for anchor matching (backslashes folded to
/// forward slashes, trimmed) — mirrors the touch-surface matching
/// convention so Windows-side references match the journal's
/// workspace-relative paths.
fn normalize_path(p: &str) -> String {
    p.trim().replace('\\', "/")
}

/// Make a claim/fact string safe for the refusal feedback consumers:
/// quotes, backslashes and control characters are escaped in the
/// JSON-string convention so the rendered row stays a valid JSON string
/// value even when interpolated verbatim by streaming-path feedback
/// composition (rows are model/journal-influenced text; never widen the
/// interpolation surface).
fn json_safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Bound a claim/fact string for row rendering (JSON-safe escaping plus
/// an explicit truncation marker when capped — never a silent cut).
fn bound_text(s: &str) -> String {
    let safe = json_safe(s);
    if safe.chars().count() <= ROW_TEXT_CAP {
        safe
    } else {
        let cut: String = safe.chars().take(ROW_TEXT_CAP).collect();
        format!("{cut}…[truncated]")
    }
}

/// Build one discrepancy row with both text faces bounded and made
/// consumer-safe (single choke point — no raw claim/fact text escapes).
fn row(face: &str, field: &str, llm_claim: String, machine_fact: String) -> Discrepancy {
    Discrepancy {
        face: face.to_string(),
        field: field.to_string(),
        llm_claim: bound_text(&llm_claim),
        machine_fact: bound_text(&machine_fact),
    }
}

/// Extract the assertion face from `task_done` args (pure, best-effort):
/// reads `params.anchors` only. Recognized shapes — a bare path string, a
/// `path = hash` string (the handover anchors convention), an object with
/// a `path` key (plus an optional `hash` key), or a path→hash map.
/// Anything else is skipped (an unparseable declaration is not a claim
/// this comparison can carry; the coverage statistic reads the same
/// field for its own projection).
pub fn extract_assertion_face(args: &serde_json::Value) -> AssertionFace {
    let mut anchors = Vec::new();
    let Some(list) = args.get("params").and_then(|p| p.get("anchors")) else {
        return AssertionFace { anchors };
    };
    let mut push = |path: &str, hash: Option<&str>| {
        let p = normalize_path(path);
        if p.is_empty() {
            return;
        }
        anchors.push(AnchorClaim {
            path: p,
            hash: hash.map(str::to_string),
        });
    };
    match list {
        serde_json::Value::Array(items) => {
            for item in items {
                match item {
                    serde_json::Value::String(s) => {
                        let t = s.trim();
                        if let Some((p, h)) = t.split_once(" = ") {
                            push(p, Some(h.trim()));
                        } else {
                            push(t, None);
                        }
                    }
                    serde_json::Value::Object(map) => {
                        if let Some(p) = map.get("path").and_then(|v| v.as_str()) {
                            push(p, map.get("hash").and_then(|v| v.as_str()));
                        }
                    }
                    _ => {}
                }
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                push(k, v.as_str());
            }
        }
        _ => {}
    }
    AssertionFace { anchors }
}

/// Project the machine ledger face from a session journal read (pure,
/// zero IO, zero clock reads). `None` (journal disabled or unreadable)
/// maps to [`LedgerFace::Unestablished`] — an unknowable ledger never
/// masquerades as an agreeing one. The window starts at the newest turn
/// boundary (the write-off window discipline; a journal without any
/// turn boundary projects the whole session).
pub fn ledger_face_from_journal(lines: Option<&[JournalLine]>) -> LedgerFace {
    let Some(lines) = lines else {
        return LedgerFace::Unestablished;
    };
    let start = writeoff::window_start_seq(lines);
    let mut anchors: BTreeMap<String, String> = BTreeMap::new();
    let mut verify_tail: Option<VerifyTail> = None;
    for line in lines.iter().filter(|l| l.seq >= start) {
        match &line.event {
            JournalEvent::FileCheckpointTaken { path, hash, .. } => {
                anchors.insert(normalize_path(path), hash.clone());
            }
            JournalEvent::VerifyRan { name, verdict, .. } => {
                verify_tail = Some(VerifyTail {
                    name: name.clone(),
                    verdict: verdict.clone(),
                });
            }
            _ => {}
        }
    }
    LedgerFace::Projected(ProjectedLedger {
        anchors,
        verify_tail,
    })
}

/// Whether the report holds at least one hard contradiction (an
/// assertion↔ledger row). The caller uses this to name the refusal class:
/// a contradiction is red; a report whose rows are all ledger↔criteria
/// polarity noise is unresolvable (unknown is not pass).
pub fn is_contradiction(report: &ReconcileReport) -> bool {
    report
        .discrepancies
        .iter()
        .any(|d| d.face == FACE_ASSERTION_VS_LEDGER)
}

/// Render one assertion↔ledger row pair for a claim.
fn assertion_rows(
    claim: &AnchorClaim,
    ledger: Option<&String>,
    ledger_established: bool,
) -> Vec<Discrepancy> {
    let claimed = match &claim.hash {
        Some(h) => format!("'{}' (claimed hash: {})", claim.path, h),
        None => format!("'{}' (no hash claimed)", claim.path),
    };
    match (ledger_established, ledger) {
        (false, _) => vec![row(
            FACE_ASSERTION_VS_LEDGER,
            "anchors",
            claimed,
            "ledger face cannot be established (journal disabled or unreadable)".to_string(),
        )],
        (true, None) => vec![row(
            FACE_ASSERTION_VS_LEDGER,
            "anchors",
            claimed,
            format!(
                "no ledger anchor for this path (off-ledger anchor): '{}'",
                claim.path
            ),
        )],
        (true, Some(recorded)) if claim.hash.as_deref().is_some_and(|h| h != recorded) => {
            vec![row(
                FACE_ASSERTION_VS_LEDGER,
                "anchors",
                claimed,
                format!("ledger anchor hash for '{}': {}", claim.path, recorded),
            )]
        }
        (true, Some(_)) => Vec::new(),
    }
}

/// Render the ledger↔criteria row when the two faces disagree on the
/// pass/fail polarity. Vacuous (no row) when either face is missing: an
/// unconfigured criterion and a window with no verify run are honest
/// gaps, not disagreements.
fn polarity_row(ledger: &ProjectedLedger, criteria: &CriteriaFace) -> Vec<Discrepancy> {
    let CriteriaFace::FreshRun { passed, detail } = criteria else {
        return Vec::new();
    };
    let Some(tail) = &ledger.verify_tail else {
        return Vec::new();
    };
    let ledger_green = tail.verdict == "pass";
    if ledger_green == *passed {
        return Vec::new();
    }
    let llm_claim = format!("window verify tail '{}': {}", tail.name, tail.verdict);
    let machine_fact = if *passed {
        "fresh acceptance run passed".to_string()
    } else {
        format!("fresh acceptance run failed ({})", bound_text(detail))
    };
    vec![row(
        FACE_LEDGER_VS_CRITERIA,
        "verify_verdict",
        llm_claim,
        machine_fact,
    )]
}

/// Cross-check the three sources (pure). Every disagreement lands in the
/// report item-by-item, in detection order: assertion↔ledger rows first
/// (declaration order, one row per unresolvable or contradicted claim),
/// then the ledger↔criteria polarity row when the faces disagree. Rows
/// are never folded, never dropped, and nothing is silently repaired.
pub fn reconcile_sources(
    assertion: &AssertionFace,
    ledger: &LedgerFace,
    criteria: &CriteriaFace,
) -> ReconcileReport {
    let mut discrepancies = Vec::new();
    let (ledger_anchors, established) = match ledger {
        LedgerFace::Projected(p) => (Some(&p.anchors), true),
        LedgerFace::Unestablished => (None, false),
    };
    for claim in &assertion.anchors {
        let recorded = ledger_anchors.and_then(|m| m.get(&claim.path));
        discrepancies.extend(assertion_rows(claim, recorded, established));
    }
    if established {
        if let LedgerFace::Projected(p) = ledger {
            discrepancies.extend(polarity_row(p, criteria));
        }
    }
    ReconcileReport {
        consistent: discrepancies.is_empty(),
        discrepancies,
    }
}

/// Render the refusal detail for an inconsistent report: the errata count
/// plus every discrepancy itemized as a numbered segment on one line
/// (single-line by design: the detail rides refusal feedback consumed as
/// interpolated JSON on the streaming path, so no raw newlines; the
/// numbering keeps every row separately addressable — nothing folded,
/// nothing silently repaired).
pub fn rejection_line(report: &ReconcileReport) -> String {
    let mut out = format!(
        "{} discrepanc{}, itemized:",
        report.discrepancies.len(),
        if report.discrepancies.len() == 1 {
            "y"
        } else {
            "ies"
        }
    );
    for (i, d) in report.discrepancies.iter().enumerate() {
        out.push_str(&format!(
            " [{}] face={} field={}: claim vs fact: {} | {}",
            i + 1,
            d.face,
            d.field,
            d.llm_claim,
            d.machine_fact
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn claim(path: &str, hash: Option<&str>) -> AnchorClaim {
        AnchorClaim {
            path: path.to_string(),
            hash: hash.map(str::to_string),
        }
    }

    fn ledger_with(anchors: &[(&str, &str)], verify: Option<(&str, &str)>) -> LedgerFace {
        LedgerFace::Projected(ProjectedLedger {
            anchors: anchors
                .iter()
                .map(|(p, h)| (p.to_string(), h.to_string()))
                .collect(),
            verify_tail: verify.map(|(n, v)| VerifyTail {
                name: n.to_string(),
                verdict: v.to_string(),
            }),
        })
    }

    fn fresh(passed: bool) -> CriteriaFace {
        CriteriaFace::FreshRun {
            passed,
            detail: if passed {
                String::new()
            } else {
                "exit=Some(1)".to_string()
            },
        }
    }

    // ===== ① Three-source consistent → consistent report (zero rows) =====

    #[test]
    fn test_three_source_consistent_yields_zero_rows() {
        let assertion = AssertionFace {
            anchors: vec![
                claim("src/a.txt", Some("blake3:aaa")),
                claim("docs/b.md", None),
            ],
        };
        let ledger = ledger_with(
            &[("src/a.txt", "blake3:aaa"), ("docs/b.md", "blake3:bbb")],
            Some(("check", "pass")),
        );
        let report = reconcile_sources(&assertion, &ledger, &fresh(true));
        assert!(
            report.consistent,
            "matching anchors + green/green polarity = consistent"
        );
        assert!(
            report.discrepancies.is_empty(),
            "consistent report carries no rows"
        );
        // Deterministic: same input twice → identical report.
        let again = reconcile_sources(&assertion, &ledger, &fresh(true));
        assert_eq!(report, again);
    }

    // ===== ② Assertion vs ledger differences → itemized rows =====

    #[test]
    fn test_assertion_ledger_mismatch_itemized_per_claim() {
        // Two wrong claims → two rows (one per claim; never folded).
        let assertion = AssertionFace {
            anchors: vec![
                claim("src/a.txt", Some("blake3:stale")),
                claim("src/missing.txt", None),
            ],
        };
        let ledger = ledger_with(&[("src/a.txt", "blake3:real")], None);
        let report = reconcile_sources(&assertion, &ledger, &fresh(true));
        assert!(!report.consistent);
        assert_eq!(report.discrepancies.len(), 2, "one row per claim");
        let d0 = &report.discrepancies[0];
        assert_eq!(d0.face, FACE_ASSERTION_VS_LEDGER);
        assert_eq!(d0.field, "anchors");
        assert!(
            d0.llm_claim.contains("blake3:stale"),
            "the claim is carried verbatim (no silent repair): {d0:?}"
        );
        assert!(
            d0.machine_fact.contains("blake3:real"),
            "the ledger fact names the recorded hash: {d0:?}"
        );
        let d1 = &report.discrepancies[1];
        assert!(
            d1.machine_fact.contains("src/missing.txt"),
            "the off-ledger row names the exact path: {d1:?}"
        );
    }

    // ===== ③ Off-ledger anchor → red, path named =====

    #[test]
    fn test_off_ledger_anchor_is_red_with_path_named() {
        let assertion = AssertionFace {
            anchors: vec![claim("ghost/path.txt", Some("blake3:zzz"))],
        };
        let ledger = ledger_with(&[("src/a.txt", "blake3:aaa")], None);
        let report = reconcile_sources(&assertion, &ledger, &fresh(true));
        assert!(!report.consistent);
        assert!(
            is_contradiction(&report),
            "off-ledger anchor = contradiction"
        );
        let d = &report.discrepancies[0];
        assert_eq!(d.face, FACE_ASSERTION_VS_LEDGER);
        assert!(
            d.machine_fact.contains("off-ledger anchor")
                && d.machine_fact.contains("ghost/path.txt"),
            "the row is red and names the anchor path: {d:?}"
        );
        let line = rejection_line(&report);
        assert!(line.contains("1 discrepancy"));
        assert!(line.contains("ghost/path.txt"));
    }

    // ===== ③ (§criteria-green / verify-red) noise counted, not folded =====

    #[test]
    fn test_criteria_green_verify_red_counted_as_noise() {
        let assertion = AssertionFace::default();
        let ledger = ledger_with(&[], Some(("check", "fail")));
        let report = reconcile_sources(&assertion, &ledger, &fresh(true));
        assert!(!report.consistent);
        assert_eq!(report.discrepancies.len(), 1, "errata count = 1, as-is");
        assert!(
            !is_contradiction(&report),
            "no assertion row: this is cross-face noise, not a contradiction"
        );
        let d = &report.discrepancies[0];
        assert_eq!(d.face, FACE_LEDGER_VS_CRITERIA);
        assert_eq!(d.field, "verify_verdict");
        assert!(d.llm_claim.contains("fail"), "window side carried: {d:?}");
        assert!(
            d.machine_fact.contains("fresh acceptance run passed"),
            "external side carried: {d:?}"
        );
    }

    #[test]
    fn test_criteria_red_verify_green_mirror_row() {
        let assertion = AssertionFace::default();
        let ledger = ledger_with(&[], Some(("check", "pass")));
        let report = reconcile_sources(&assertion, &ledger, &fresh(false));
        assert_eq!(
            report.discrepancies.len(),
            1,
            "mirror polarity also counted"
        );
        assert_eq!(report.discrepancies[0].face, FACE_LEDGER_VS_CRITERIA);
    }

    #[test]
    fn test_polarity_vacuous_when_faces_absent() {
        // No verify tail in the window: not a red window — vacuous, no row.
        let assertion = AssertionFace::default();
        let ledger = ledger_with(&[("a", "h")], None);
        let report = reconcile_sources(&assertion, &ledger, &fresh(true));
        assert!(
            report.consistent,
            "absent verify track is a gap, not a disagreement"
        );
        // Unconfigured criteria: vacuous on this face too.
        let ledger = ledger_with(&[], Some(("check", "fail")));
        let report = reconcile_sources(&assertion, &ledger, &CriteriaFace::Unconfigured);
        assert!(
            report.consistent,
            "unconfigured external face is vacuous here"
        );
    }

    // ===== ④ Errata count faithful: rows never folded, never repaired =====

    #[test]
    fn test_errata_count_itemized_not_folded() {
        // Three distinct problems → exactly three rows, each preserved
        // verbatim; the rendered line numbers every row.
        let assertion = AssertionFace {
            anchors: vec![
                claim("p1", Some("h-wrong")),
                claim("p2", None),
                claim("p3", Some("h-also-wrong")),
            ],
        };
        let ledger = ledger_with(&[("p1", "h-real1"), ("p3", "h-real3")], None);
        let report = reconcile_sources(&assertion, &ledger, &fresh(true));
        assert_eq!(
            report.discrepancies.len(),
            3,
            "2 hash mismatches + 1 off-ledger"
        );
        let line = rejection_line(&report);
        assert!(line.contains("3 discrepancies"));
        for n in 1..=3 {
            assert!(line.contains(&format!("[{n}]")), "row {n} present: {line}");
        }
        assert!(line.contains("h-wrong") && line.contains("h-also-wrong"));
        assert!(line.contains("p2"), "off-ledger path p2 named");
    }

    #[test]
    fn test_unestablished_ledger_substantiates_nothing() {
        // Journal unreadable + claims present → every claim unsubstantiated
        // (unknown is not pass); zero claims → vacuously consistent (the
        // conjunction's Unknown pieces refuse separately).
        let assertion = AssertionFace {
            anchors: vec![claim("a.txt", None), claim("b.txt", Some("h"))],
        };
        let report = reconcile_sources(&assertion, &LedgerFace::Unestablished, &fresh(true));
        assert_eq!(
            report.discrepancies.len(),
            2,
            "one unsubstantiated row per claim"
        );
        assert!(is_contradiction(&report));
        assert!(
            report.discrepancies[0]
                .machine_fact
                .contains("cannot be established"),
            "the row says the ledger face is unknowable: {report:?}"
        );
        let empty = AssertionFace::default();
        let report = reconcile_sources(&empty, &LedgerFace::Unestablished, &fresh(true));
        assert!(report.consistent, "nothing claimed = nothing to reconcile");
    }

    // ===== Assertion-face extraction shapes =====

    #[test]
    fn test_extract_assertion_face_shapes() {
        let args = serde_json::json!({
            "instruction_type": "task_done",
            "params": {
                "anchors": [
                    "src/a.txt",
                    "docs/b.md = blake3:abcd",
                    {"path": "lib/c.rs"},
                    {"path": "lib/d.rs", "hash": "blake3:ddd"},
                    "",
                    "  src/a.txt  "
                ]
            }
        });
        let face = extract_assertion_face(&args);
        assert_eq!(face.anchors.len(), 5, "empty dropped, rest kept in order");
        assert_eq!(face.anchors[0], claim("src/a.txt", None));
        assert_eq!(face.anchors[1], claim("docs/b.md", Some("blake3:abcd")));
        assert_eq!(face.anchors[2], claim("lib/c.rs", None));
        assert_eq!(face.anchors[3], claim("lib/d.rs", Some("blake3:ddd")));
        assert_eq!(
            face.anchors[4],
            claim("src/a.txt", None),
            "duplicate preserved (never folded at extraction)"
        );
        // Map form: path→hash.
        let map = serde_json::json!({
            "instruction_type": "task_done",
            "params": {"anchors": {"m.txt": "blake3:mmm"}}
        });
        let face = extract_assertion_face(&map);
        assert_eq!(face.anchors, vec![claim("m.txt", Some("blake3:mmm"))]);
        // Backslash normalization matches the ledger-side convention.
        let win = serde_json::json!({
            "instruction_type": "task_done",
            "params": {"anchors": ["src\\win.txt"]}
        });
        let face = extract_assertion_face(&win);
        assert_eq!(face.anchors[0].path, "src/win.txt");
        // No anchors field at all → empty face.
        let bare = serde_json::json!({"instruction_type": "task_done", "params": {}});
        assert!(extract_assertion_face(&bare).anchors.is_empty());
    }

    // ===== Ledger-face projection: pure, window-scoped, deterministic =====

    #[test]
    fn test_ledger_face_projection_window_scoped_deterministic() {
        let jl = |seq: u64, event: JournalEvent| JournalLine {
            seq,
            ts: 1,
            event,
            hashes: None,
        };
        let checkpoint = |seq: u64, path: &str, hash: &str| {
            jl(
                seq,
                JournalEvent::FileCheckpointTaken {
                    path: path.to_string(),
                    kind: "content".to_string(),
                    hash: hash.to_string(),
                    bytes: 1,
                    tool: "file_write".to_string(),
                },
            )
        };
        let verify = |seq: u64, verdict: &str| {
            jl(
                seq,
                JournalEvent::VerifyRan {
                    name: "check".to_string(),
                    command: "true".to_string(),
                    exit_code: Some(0),
                    error_count: 0,
                    diags_digest: "d".to_string(),
                    verdict: verdict.to_string(),
                    output_digest: "o".to_string(),
                },
            )
        };
        let turn = jl(
            3,
            JournalEvent::TurnStarted {
                turn_seq: 1,
                goal: "g".to_string(),
            },
        );
        // Events before the newest turn boundary are out of the window.
        let lines = vec![
            checkpoint(1, "old.txt", "h-old"),
            verify(2, "fail"),
            turn,
            checkpoint(4, "a.txt", "h1"),
            checkpoint(5, "a.txt", "h2"), // latest wins
            verify(6, "pass"),
            verify(7, "fail"), // chain-order tail wins (any verdict)
        ];
        let face = ledger_face_from_journal(Some(&lines));
        let LedgerFace::Projected(p) = &face else {
            panic!("readable journal must project");
        };
        assert_eq!(p.anchors.len(), 1, "pre-window anchor excluded");
        assert_eq!(p.anchors.get("a.txt").map(String::as_str), Some("h2"));
        assert_eq!(
            p.verify_tail.as_ref().map(|t| t.verdict.as_str()),
            Some("fail"),
            "newest verify run wins regardless of verdict"
        );
        // Deterministic: same input twice → identical face.
        assert_eq!(face, ledger_face_from_journal(Some(&lines)));
        // Journal absent → unestablished (unknown, not a fake-agreeing face).
        assert_eq!(ledger_face_from_journal(None), LedgerFace::Unestablished);
    }

    #[test]
    fn test_row_text_bounded_with_marker() {
        let long = "x".repeat(ROW_TEXT_CAP + 50);
        let assertion = AssertionFace {
            anchors: vec![claim(&long, None)],
        };
        let report = reconcile_sources(&assertion, &LedgerFace::Unestablished, &fresh(true));
        let d = &report.discrepancies[0];
        assert!(d.llm_claim.len() < ROW_TEXT_CAP + 60, "row text bounded");
        assert!(
            d.llm_claim.contains("[truncated]"),
            "cut is explicit, not silent"
        );
    }

    #[test]
    fn test_row_text_json_safe_for_feedback_interpolation() {
        // Model-controlled claim text with quotes/newlines must not widen
        // the refusal-feedback interpolation surface: the streaming path
        // composes the refusal JSON by string interpolation, so the true
        // contract is "row text embedded as a JSON string value parses".
        let assertion = AssertionFace {
            anchors: vec![claim("we\"ird\npath\\x", Some("h\"q"))],
        };
        let report = reconcile_sources(&assertion, &LedgerFace::Unestablished, &fresh(true));
        let d = &report.discrepancies[0];
        assert!(
            !d.llm_claim.contains('\n'),
            "raw control chars escaped: {d:?}"
        );
        let embedded = format!("\"{}\"", d.llm_claim);
        assert!(
            serde_json::from_str::<serde_json::Value>(&embedded).is_ok(),
            "row text must interpolate as a valid JSON string: {embedded}"
        );
        // The whole rendered rejection line stays single-line and equally
        // interpolation-safe.
        let line = rejection_line(&report);
        assert!(!line.contains('\n'), "single-line by design");
        let embedded = format!("\"{}\"", line);
        assert!(
            serde_json::from_str::<serde_json::Value>(&embedded).is_ok(),
            "the whole rendered line must interpolate as a JSON string"
        );
    }
}
