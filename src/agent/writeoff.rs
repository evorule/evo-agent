// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Write-off conjunction gate for `task_done` submissions (long-horizon mode).
//!
//! For long-horizon agents, admission of a `task_done` submission is a
//! three-piece conjunction instead of the criteria piece alone. Every piece
//! is computed by the runner from machine sources; the model never supplies
//! gate inputs (same decision-authority split as the acceptance gate's
//! runner-filled `acceptance_passed` flag):
//!
//! 1. acceptance criteria — the configured acceptance command exited 0
//!    (unchanged piece; evaluated by the acceptance gate);
//! 2. committed evidence — the session journal window holds a passing
//!    verify run or a file checkpoint, with the chain-tail digest carried
//!    for audit;
//! 3. breakpoint freshness — a handover-written or todo-snapshot event in
//!    the window is strictly newer than the last completed-class todo
//!    transition recorded in the window.
//!
//! The verdict is three-valued: [`WriteoffVerdict::Pass`],
//! [`WriteoffVerdict::Reject`] (itemized failed/missing pieces), and
//! [`WriteoffVerdict::Unknown`] (a piece cannot be established, e.g. the
//! journal is disabled or unreadable). Unknown never degrades to Pass: the
//! caller refuses the submission with the unknowable faces spelled out,
//! mirroring the verify gate's `unknown` verdict discipline (an unproven
//! state is not a pass). Non-long-horizon agents never enter this path;
//! the acceptance gate keeps its criteria-only behavior (default
//! invariant).
//!
//! Everything in this module is a pure function over journal lines or
//! piece states: zero IO, zero clock reads, deterministic, fully
//! unit-testable. Projection failures map to Unknown piece states, never
//! to Pass.

use crate::agent::journal::{JournalEvent, JournalLine};

/// State of the acceptance-criteria piece (conjunction piece 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CriteriaState {
    /// Acceptance command exited 0.
    Passed,
    /// Acceptance command failed (non-zero exit, spawn failure or budget
    /// timeout); the payload carries the machine-side detail for feedback.
    Failed(String),
    /// No acceptance command is configured. Inside the conjunction this is
    /// not a pass: the criteria piece cannot be established (unknown is
    /// not pass).
    Unconfigured,
}

/// State of the committed-evidence piece (conjunction piece 2), projected
/// from the journal window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceState {
    /// A passing verify run or a file checkpoint exists in the window;
    /// `digest` is the digest of the newest such event in chain order.
    Present {
        /// Digest carried by the newest evidence event (verify output
        /// digest, or the checkpointed blob hash).
        digest: String,
    },
    /// The window is readable but holds no verify/checkpoint evidence:
    /// nothing proves the work was verified or captured.
    Missing,
    /// Evidence cannot be established (journal disabled or unreadable).
    /// Never treated as present: unknown is not pass.
    Unknown,
}

/// State of the breakpoint-freshness piece (conjunction piece 3), projected
/// from the journal window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreshnessState {
    /// A handover-written or todo-snapshot event in the window is strictly
    /// newer (by event timestamp) than the last completed-class todo
    /// transition; trivially satisfied when the window records no
    /// completed-class transition at all.
    Fresh,
    /// Breakpoint events exist in the window, but the newest one is not
    /// newer than the last completed-class transition: the checkpoint
    /// lags the latest progress.
    Stale,
    /// The window holds no breakpoint event (neither handover-written nor
    /// todo-snapshot): no checkpoint was written during this window.
    Missing,
    /// Freshness cannot be established (journal disabled or unreadable).
    /// Never treated as fresh: unknown is not pass.
    Unknown,
}

/// Journal-derived pieces of the conjunction, projected by the runner from
/// the session journal and handed to the acceptance gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalPieces {
    /// Piece 2: committed evidence within the window.
    pub evidence: EvidenceState,
    /// Piece 3: breakpoint freshness within the window.
    pub freshness: FreshnessState,
}

/// Write-off verdict. The caller (acceptance gate) refuses the `task_done`
/// submission on everything except [`WriteoffVerdict::Pass`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteoffVerdict {
    /// All three pieces are established and satisfied.
    Pass,
    /// At least one piece failed or is missing; the payload itemizes every
    /// failing piece in one report (listed at once, not drip-fed).
    Reject(String),
    /// At least one piece cannot be established; the payload names the
    /// unknowable faces. The caller refuses the submission without
    /// pretending the unknowable piece passed.
    Unknown(String),
}

/// Window start for the write-off projection: the seq of the newest
/// `TurnStarted` event (the current goal turn), or `0` to project the
/// whole session when the journal never recorded a turn boundary.
pub fn window_start_seq(lines: &[JournalLine]) -> u64 {
    lines
        .iter()
        .rev()
        .find(|l| matches!(l.event, JournalEvent::TurnStarted { .. }))
        .map(|l| l.seq)
        .unwrap_or(0)
}

/// Project the two journal-derived pieces for the window starting at
/// `window_start_seq` (inclusive). Pure: iterates the given lines once,
/// no IO, no clock reads.
///
/// Evidence: the newest passing verify run or file checkpoint in chain
/// order wins (later lines overwrite earlier ones). Freshness: a
/// todo-snapshot event whose items contain any completed entry counts as
/// the completed-class transition; the same event also counts as a
/// breakpoint event, so freshness requires a breakpoint event *strictly*
/// newer than it (a checkpoint that merely records the completion does
/// not refresh itself).
pub fn project_window(lines: &[JournalLine], window_start_seq: u64) -> JournalPieces {
    let mut evidence_digest: Option<String> = None;
    let mut last_breakpoint_ts: Option<u64> = None;
    let mut last_completed_ts: Option<u64> = None;
    for line in lines.iter().filter(|l| l.seq >= window_start_seq) {
        match &line.event {
            JournalEvent::VerifyRan {
                verdict,
                output_digest,
                ..
            } => {
                if verdict == "pass" {
                    evidence_digest = Some(output_digest.clone());
                }
            }
            JournalEvent::FileCheckpointTaken { hash, .. } => {
                evidence_digest = Some(hash.clone());
            }
            JournalEvent::HandoverWritten { .. } => {
                last_breakpoint_ts = Some(line.ts);
            }
            JournalEvent::TodoUpdated { items } => {
                last_breakpoint_ts = Some(line.ts);
                if items.iter().any(|i| i.status == "completed") {
                    last_completed_ts = Some(line.ts);
                }
            }
            _ => {}
        }
    }
    let evidence = match evidence_digest {
        Some(digest) => EvidenceState::Present { digest },
        None => EvidenceState::Missing,
    };
    let freshness = match (last_breakpoint_ts, last_completed_ts) {
        (None, _) => FreshnessState::Missing,
        (Some(_), None) => FreshnessState::Fresh,
        (Some(breakpoint_ts), Some(completed_ts)) if breakpoint_ts > completed_ts => {
            FreshnessState::Fresh
        }
        (Some(_), Some(_)) => FreshnessState::Stale,
    };
    JournalPieces {
        evidence,
        freshness,
    }
}

/// Build the journal-derived pieces from a session journal read. `None`
/// (journal disabled, or the read failed) maps both pieces to Unknown —
/// an unknowable journal is never treated as satisfied evidence. The
/// caller passes the whole session; the window is derived internally from
/// the newest turn boundary.
pub fn pieces_from_journal(lines: Option<&[JournalLine]>) -> JournalPieces {
    match lines {
        None => JournalPieces {
            evidence: EvidenceState::Unknown,
            freshness: FreshnessState::Unknown,
        },
        Some(lines) => {
            let start = window_start_seq(lines);
            project_window(lines, start)
        }
    }
}

/// Evaluate the three-piece conjunction. Pure aggregation over piece
/// states; the detail strings itemize every failing or unknowable piece in
/// one report.
pub fn evaluate_conjunction(criteria: CriteriaState, pieces: &JournalPieces) -> WriteoffVerdict {
    let mut unknown_faces: Vec<String> = Vec::new();
    let mut failed_faces: Vec<String> = Vec::new();
    match criteria {
        CriteriaState::Passed => {}
        CriteriaState::Failed(detail) => {
            failed_faces.push(format!("acceptance criteria not met: {detail}"));
        }
        CriteriaState::Unconfigured => {
            unknown_faces.push(
                "acceptance criteria cannot be established (no acceptance command configured)"
                    .to_string(),
            );
        }
    }
    match &pieces.evidence {
        EvidenceState::Present { .. } => {}
        EvidenceState::Missing => {
            failed_faces.push(
                "committed evidence missing: no passing verify run and no file checkpoint in the window"
                    .to_string(),
            );
        }
        EvidenceState::Unknown => {
            unknown_faces.push(
                "committed evidence cannot be established (journal disabled or unreadable)"
                    .to_string(),
            );
        }
    }
    match pieces.freshness {
        FreshnessState::Fresh => {}
        FreshnessState::Stale => failed_faces.push(
            "breakpoint stale: the last completed todo transition is newer than the newest handover/todo snapshot; write a fresh breakpoint update"
                .to_string(),
        ),
        FreshnessState::Missing => failed_faces.push(
            "breakpoint missing: neither handover-written nor todo-snapshot recorded in the window"
                .to_string(),
        ),
        FreshnessState::Unknown => unknown_faces.push(
            "breakpoint freshness cannot be established (journal disabled or unreadable)"
                .to_string(),
        ),
    }
    if !unknown_faces.is_empty() {
        // Unknown dominates: an unknowable piece must not be traded for a
        // pass. The report still lists outright failures alongside so the
        // feedback is complete in one shot.
        let mut detail = unknown_faces.join("; ");
        if !failed_faces.is_empty() {
            detail.push_str("; additionally failed: ");
            detail.push_str(&failed_faces.join("; "));
        }
        return WriteoffVerdict::Unknown(detail);
    }
    if !failed_faces.is_empty() {
        return WriteoffVerdict::Reject(failed_faces.join("; "));
    }
    WriteoffVerdict::Pass
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Line builder: seq ordered, explicit ts (unix ms), no chain hashes
    /// (projection never reads the hash column).
    fn line(seq: u64, ts: u64, event: JournalEvent) -> JournalLine {
        JournalLine {
            seq,
            ts,
            event,
            hashes: None,
        }
    }

    fn turn_started(seq: u64, ts: u64) -> JournalLine {
        line(
            seq,
            ts,
            JournalEvent::TurnStarted {
                turn_seq: 1,
                goal: "g".to_string(),
            },
        )
    }

    fn verify(seq: u64, ts: u64, verdict: &str) -> JournalLine {
        line(
            seq,
            ts,
            JournalEvent::VerifyRan {
                name: "check".to_string(),
                command: "true".to_string(),
                exit_code: Some(0),
                error_count: 0,
                diags_digest: "d".to_string(),
                verdict: verdict.to_string(),
                output_digest: format!("out-{seq}"),
            },
        )
    }

    fn checkpoint(seq: u64, ts: u64) -> JournalLine {
        line(
            seq,
            ts,
            JournalEvent::FileCheckpointTaken {
                path: "a.txt".to_string(),
                kind: "content".to_string(),
                hash: format!("hash-{seq}"),
                bytes: 3,
                tool: "file_write".to_string(),
            },
        )
    }

    fn handover(seq: u64, ts: u64) -> JournalLine {
        line(
            seq,
            ts,
            JournalEvent::HandoverWritten {
                session: "s".to_string(),
                path: "shared.n.handovers.h1".to_string(),
                schema_ok: true,
            },
        )
    }

    fn todo(seq: u64, ts: u64, statuses: &[&str]) -> JournalLine {
        line(
            seq,
            ts,
            JournalEvent::TodoUpdated {
                items: statuses
                    .iter()
                    .enumerate()
                    .map(|(i, status)| crate::agent::journal::TodoItemSnapshot {
                        id: format!("t{i}"),
                        content: format!("item {i}"),
                        status: (*status).to_string(),
                        priority: "medium".to_string(),
                    })
                    .collect(),
            },
        )
    }

    fn all_passing() -> JournalPieces {
        JournalPieces {
            evidence: EvidenceState::Present {
                digest: "out-2".to_string(),
            },
            freshness: FreshnessState::Fresh,
        }
    }

    // ===== Three-branch verdict coverage (pure function) =====

    #[test]
    fn test_all_pieces_satisfied_is_pass() {
        let verdict = evaluate_conjunction(CriteriaState::Passed, &all_passing());
        assert_eq!(verdict, WriteoffVerdict::Pass);
    }

    #[test]
    fn test_failed_criteria_is_reject_with_detail() {
        let verdict = evaluate_conjunction(
            CriteriaState::Failed("exit=Some(3)".to_string()),
            &all_passing(),
        );
        match verdict {
            WriteoffVerdict::Reject(detail) => {
                assert!(detail.contains("acceptance criteria not met"), "{detail}");
                assert!(detail.contains("exit=Some(3)"), "{detail}");
                // Only the failing piece is listed.
                assert!(!detail.contains("evidence"), "{detail}");
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn test_missing_evidence_is_reject_with_detail() {
        let pieces = JournalPieces {
            evidence: EvidenceState::Missing,
            freshness: FreshnessState::Fresh,
        };
        let verdict = evaluate_conjunction(CriteriaState::Passed, &pieces);
        match verdict {
            WriteoffVerdict::Reject(detail) => {
                assert!(detail.contains("committed evidence missing"), "{detail}");
                assert!(!detail.contains("breakpoint"), "{detail}");
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn test_unknown_piece_is_unknown_not_pass() {
        let pieces = JournalPieces {
            evidence: EvidenceState::Unknown,
            freshness: FreshnessState::Fresh,
        };
        let verdict = evaluate_conjunction(CriteriaState::Passed, &pieces);
        assert!(
            matches!(verdict, WriteoffVerdict::Unknown(_)),
            "unknown evidence must not degrade to Pass/Reject: {verdict:?}"
        );
        // Unconfigured criteria is likewise unknown-not-pass.
        let verdict = evaluate_conjunction(CriteriaState::Unconfigured, &all_passing());
        assert!(
            matches!(verdict, WriteoffVerdict::Unknown(_)),
            "unconfigured criteria must not pass: {verdict:?}"
        );
    }

    // ===== Missing-one matrix: every single-missing and all-missing
    // combination lands on Reject or Unknown, never Pass. =====

    #[test]
    fn test_missing_one_matrix_never_passes() {
        let pass = all_passing();
        let no_evidence = JournalPieces {
            evidence: EvidenceState::Missing,
            freshness: FreshnessState::Fresh,
        };
        let stale = JournalPieces {
            evidence: EvidenceState::Present {
                digest: "h".to_string(),
            },
            freshness: FreshnessState::Stale,
        };
        let no_breakpoint = JournalPieces {
            evidence: EvidenceState::Present {
                digest: "h".to_string(),
            },
            freshness: FreshnessState::Missing,
        };
        let none = JournalPieces {
            evidence: EvidenceState::Missing,
            freshness: FreshnessState::Missing,
        };
        let matrix: Vec<(CriteriaState, JournalPieces, &str)> = vec![
            (
                CriteriaState::Failed("exit=Some(1)".to_string()),
                pass.clone(),
                "criteria only",
            ),
            (CriteriaState::Passed, no_evidence.clone(), "evidence only"),
            (CriteriaState::Passed, stale, "breakpoint stale"),
            (CriteriaState::Passed, no_breakpoint, "breakpoint missing"),
            (
                CriteriaState::Failed("exit=Some(1)".to_string()),
                no_evidence.clone(),
                "criteria + evidence",
            ),
            (
                CriteriaState::Failed("exit=Some(1)".to_string()),
                none.clone(),
                "criteria + evidence + breakpoint",
            ),
            (CriteriaState::Passed, none, "evidence + breakpoint"),
        ];
        for (criteria, pieces, label) in matrix {
            let verdict = evaluate_conjunction(criteria, &pieces);
            assert!(
                matches!(
                    verdict,
                    WriteoffVerdict::Reject(_) | WriteoffVerdict::Unknown(_)
                ),
                "{label} must not admit done: {verdict:?}"
            );
            if let WriteoffVerdict::Reject(detail) | WriteoffVerdict::Unknown(detail) = verdict {
                assert!(!detail.is_empty(), "{label} detail must be visible");
            }
        }
    }

    #[test]
    fn test_unknown_dominates_and_lists_failures_too() {
        let pieces = JournalPieces {
            evidence: EvidenceState::Unknown,
            freshness: FreshnessState::Missing,
        };
        let verdict =
            evaluate_conjunction(CriteriaState::Failed("exit=Some(2)".to_string()), &pieces);
        match verdict {
            WriteoffVerdict::Unknown(detail) => {
                assert!(detail.contains("cannot be established"), "{detail}");
                // Outright failures are still listed in one shot.
                assert!(detail.contains("additionally failed"), "{detail}");
                assert!(detail.contains("acceptance criteria not met"), "{detail}");
                assert!(detail.contains("breakpoint missing"), "{detail}");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    // ===== Window projection =====

    #[test]
    fn test_window_start_seq_prefers_newest_turn() {
        let lines = vec![
            turn_started(1, 100),
            verify(2, 110, "pass"),
            turn_started(3, 200),
            handover(4, 210),
        ];
        assert_eq!(window_start_seq(&lines), 3);
        // No turn boundary in the file: project the whole session.
        let lines = vec![verify(1, 100, "pass")];
        assert_eq!(window_start_seq(&lines), 0);
        assert_eq!(window_start_seq(&[]), 0);
    }

    #[test]
    fn test_project_evidence_chain_tail_digest_wins() {
        let lines = vec![
            turn_started(1, 100),
            verify(2, 110, "pass"),
            checkpoint(3, 120),
            verify(4, 130, "pass"),
            // A failed verify after the last pass must not be picked up.
            verify(5, 140, "fail"),
        ];
        let pieces = project_window(&lines, 1);
        assert_eq!(
            pieces.evidence,
            EvidenceState::Present {
                digest: "out-4".to_string()
            },
            "newest passing verify in chain order provides the digest"
        );
    }

    #[test]
    fn test_project_evidence_checkpoint_only_window() {
        let lines = vec![checkpoint(1, 100), checkpoint(2, 110)];
        let pieces = project_window(&lines, 0);
        assert_eq!(
            pieces.evidence,
            EvidenceState::Present {
                digest: "hash-2".to_string()
            }
        );
        assert_eq!(pieces.freshness, FreshnessState::Missing);
    }

    #[test]
    fn test_project_freshness_strictly_newer_breakpoint() {
        // Completed transition at 130; a handover written at 140 is
        // strictly newer: fresh.
        let lines = vec![
            turn_started(1, 100),
            verify(2, 110, "pass"),
            todo(3, 130, &["completed", "pending"]),
            handover(4, 140),
        ];
        let pieces = project_window(&lines, 1);
        assert_eq!(pieces.freshness, FreshnessState::Fresh);
        // Same timestamps: the completing snapshot does not refresh
        // itself (strictly-newer requirement) — stale.
        let lines = vec![
            turn_started(1, 100),
            verify(2, 110, "pass"),
            todo(3, 130, &["completed"]),
        ];
        let pieces = project_window(&lines, 1);
        assert_eq!(pieces.freshness, FreshnessState::Stale);
        // Completion newer than the newest breakpoint: stale.
        let lines = vec![
            turn_started(1, 100),
            handover(2, 110),
            todo(3, 130, &["completed"]),
        ];
        let pieces = project_window(&lines, 1);
        assert_eq!(pieces.freshness, FreshnessState::Stale);
    }

    #[test]
    fn test_project_freshness_without_completions() {
        // No completed-class transition: any breakpoint event is fresh.
        let lines = vec![
            turn_started(1, 100),
            todo(2, 110, &["in_progress"]),
            verify(3, 120, "pass"),
        ];
        let pieces = project_window(&lines, 1);
        assert_eq!(pieces.freshness, FreshnessState::Fresh);
        assert_eq!(
            pieces.evidence,
            EvidenceState::Present {
                digest: "out-3".to_string()
            }
        );
        // Empty window: both pieces missing.
        let pieces = project_window(&lines, 99);
        assert_eq!(pieces.evidence, EvidenceState::Missing);
        assert_eq!(pieces.freshness, FreshnessState::Missing);
    }

    #[test]
    fn test_project_window_scopes_to_turn_boundary() {
        // Evidence before the newest turn boundary is out of window.
        let lines = vec![
            turn_started(1, 100),
            verify(2, 110, "pass"),
            todo(3, 120, &["completed"]),
            turn_started(4, 200),
            handover(5, 210),
        ];
        let pieces = project_window(&lines, 4);
        assert_eq!(
            pieces.evidence,
            EvidenceState::Missing,
            "verify before the turn boundary is out of window"
        );
        assert_eq!(
            pieces.freshness,
            FreshnessState::Fresh,
            "the handover after the boundary is in window and fresh"
        );
    }

    #[test]
    fn test_pieces_from_journal_unknown_without_lines() {
        let pieces = pieces_from_journal(None);
        assert_eq!(pieces.evidence, EvidenceState::Unknown);
        assert_eq!(pieces.freshness, FreshnessState::Unknown);
        let verdict = evaluate_conjunction(CriteriaState::Passed, &pieces);
        assert!(
            matches!(verdict, WriteoffVerdict::Unknown(_)),
            "journal unavailable must be unknown, never pass: {verdict:?}"
        );
    }

    #[test]
    fn test_pieces_from_journal_derives_window_internally() {
        let lines = vec![
            turn_started(1, 100),
            verify(2, 110, "pass"),
            turn_started(3, 200),
            handover(4, 210),
        ];
        let pieces = pieces_from_journal(Some(&lines));
        // Current turn has a breakpoint but no evidence: conjunction would
        // reject on the evidence face, not pass on stale-window proof.
        assert_eq!(pieces.evidence, EvidenceState::Missing);
        assert_eq!(pieces.freshness, FreshnessState::Fresh);
    }
}
