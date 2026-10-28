//! Interactive steering input source (REPL opt-in).
//!
//! Queues user-typed lines while a run is in progress and hands them to the
//! runner at step gaps (between tool rounds), where each note is merged into
//! the authoritative history as a user message before the next model call.
//!
//! Single-reader discipline: this module owns the only stdin line reader for
//! interactive steering. In-turn prompt readers (approval y/n, ask-user
//! options) read through the same std stdin lock, so a pending steering read
//! serializes them instead of racing them; the rustyline prompt itself uses
//! console APIs and does not hold the std lock. The reader therefore exits
//! on its first line whenever the gate is not open, keeping at most one
//! pending stdin consumer outside the steering path at any time.
//!
//! Default wiring is off (`None`): no thread is spawned and step-gap drain
//! is a no-op, so non-interactive paths are byte-for-byte unchanged.

use std::io::BufRead;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

/// Gate states: open (turn running) / paused (an in-turn prompt owns stdin)
/// / closed (turn ended).
const GATE_OPEN: u8 = 0;
const GATE_PAUSED: u8 = 1;
const GATE_CLOSED: u8 = 2;

/// What the reader does with one line under the current gate.
#[derive(Debug, PartialEq, Eq)]
enum LineDisposition {
    /// Queue as a steering note for the running turn.
    Queue,
    /// An approval/answer prompt owns stdin right now; drop with an echo.
    DropForPrompt,
    /// The turn has ended; queue as a pre-typed note for the next run.
    QueueAsPrenote,
}

fn classify(gate: u8) -> LineDisposition {
    match gate {
        GATE_PAUSED => LineDisposition::DropForPrompt,
        GATE_CLOSED => LineDisposition::QueueAsPrenote,
        _ => LineDisposition::Queue,
    }
}

/// Step-gap steering source: background line reader + queue (step-gap contract
/// surface: `spawn` + `try_recv`; turn lifecycle methods are REPL-side only).
pub struct SteeringSource {
    tx: Sender<String>,
    rx: Mutex<Receiver<String>>,
    gate: Arc<AtomicU8>,
    reader_alive: Arc<AtomicBool>,
}

impl SteeringSource {
    /// Create a steering source. No reader thread is spawned here; the REPL
    /// wiring arms it per turn via [`begin_turn`].
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx: Mutex::new(rx),
            gate: Arc::new(AtomicU8::new(GATE_CLOSED)),
            reader_alive: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Arm the reader for a running turn: open the gate, spawn the reader
    /// thread if none is alive (single-reader invariant: at most one alive).
    pub fn begin_turn(&self) {
        self.gate.store(GATE_OPEN, Ordering::Release);
        if !self.reader_alive.load(Ordering::Acquire) {
            self.spawn_reader();
        }
    }

    /// Close the turn: a pending reader stops queueing mid-run input and
    /// exits after at most one more line (reclassified, with an echo, as a
    /// pre-typed note for the next run).
    pub fn end_turn(&self) {
        self.gate.store(GATE_CLOSED, Ordering::Release);
    }

    /// Yield stdin to an in-turn prompt (approval / ask-user). A pending
    /// reader yields on its next line (echo + exit); the prompt reader,
    /// serialized behind the std stdin lock, then receives the re-typed
    /// answer. Callers re-open the gate on the matching result event.
    pub fn pause_for_stdin_owner(&self) {
        self.gate.store(GATE_PAUSED, Ordering::Release);
    }

    /// Re-open the gate after an in-turn prompt resolved (a still-pending
    /// reader resumes queueing; an exited one is re-armed next turn).
    pub fn resume_after_stdin_owner(&self) {
        self.gate.store(GATE_OPEN, Ordering::Release);
    }

    /// Drain one queued note. Runner step-gap contract surface: `None` =
    /// no pending input (zero behavior difference).
    pub fn try_recv(&self) -> Option<String> {
        let rx = self.rx.lock().ok()?;
        match rx.try_recv() {
            Ok(note) => Some(note),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }

    fn spawn_reader(&self) {
        self.reader_alive.store(true, Ordering::Release);
        let tx = self.tx.clone();
        let gate = self.gate.clone();
        let alive = self.reader_alive.clone();
        let spawned = std::thread::Builder::new()
            .name("steering-stdin".to_string())
            .spawn(move || reader_loop(tx, gate, alive));
        if spawned.is_err() {
            // Reader unavailable: steering stays inert for this turn.
            self.reader_alive.store(false, Ordering::Release);
        }
    }
}

fn reader_loop(tx: Sender<String>, gate: Arc<AtomicU8>, alive: Arc<AtomicBool>) {
    let stdin = std::io::stdin();
    loop {
        let mut line = String::new();
        let n = match stdin.lock().read_line(&mut line) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break; // EOF: no more stdin input
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match classify(gate.load(Ordering::Acquire)) {
            LineDisposition::Queue => {
                if tx.send(line.to_string()).is_err() {
                    break; // source dropped
                }
                // Keep reading: more mid-run input may arrive.
            }
            LineDisposition::DropForPrompt => {
                eprintln!("[steering] approval/answer pending; line not injected (re-enter it)");
                break;
            }
            LineDisposition::QueueAsPrenote => {
                let _ = tx.send(line.to_string());
                eprintln!("[steering] note queued for the next run: {line}");
                break;
            }
        }
    }
    alive.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Direct channel injection (no reader thread, no real stdin).
    fn test_source() -> (SteeringSource, Sender<String>) {
        let (tx, rx) = mpsc::channel();
        let src = SteeringSource {
            tx: tx.clone(),
            rx: Mutex::new(rx),
            gate: Arc::new(AtomicU8::new(GATE_CLOSED)),
            reader_alive: Arc::new(AtomicBool::new(false)),
        };
        (src, tx)
    }

    #[test]
    fn try_recv_empty_is_none() {
        let (src, _tx) = test_source();
        assert!(src.try_recv().is_none());
    }

    #[test]
    fn notes_queue_in_fifo_order() {
        let (src, tx) = test_source();
        for (i, text) in ["first", "second", "third"].iter().enumerate() {
            tx.send(text.to_string()).unwrap();
            assert_eq!(src.try_recv().as_deref(), Some(*text), "note {i}");
        }
        assert!(src.try_recv().is_none());
    }

    #[test]
    fn multi_value_queue_drains_completely() {
        let (src, tx) = test_source();
        for i in 0..10 {
            tx.send(format!("note-{i}")).unwrap();
        }
        for i in 0..10 {
            assert_eq!(
                src.try_recv().as_deref(),
                Some(format!("note-{i}").as_str())
            );
        }
        assert!(src.try_recv().is_none());
    }

    #[test]
    fn spawn_starts_closed_and_lifecycle_transitions() {
        let src = SteeringSource::spawn();
        assert_eq!(src.gate.load(Ordering::Acquire), GATE_CLOSED);
        src.begin_turn();
        assert_eq!(src.gate.load(Ordering::Acquire), GATE_OPEN);
        src.pause_for_stdin_owner();
        assert_eq!(src.gate.load(Ordering::Acquire), GATE_PAUSED);
        src.resume_after_stdin_owner();
        assert_eq!(src.gate.load(Ordering::Acquire), GATE_OPEN);
        src.end_turn();
        assert_eq!(src.gate.load(Ordering::Acquire), GATE_CLOSED);
    }

    #[test]
    fn classify_maps_gate_states() {
        assert_eq!(classify(GATE_OPEN), LineDisposition::Queue);
        assert_eq!(classify(GATE_PAUSED), LineDisposition::DropForPrompt);
        assert_eq!(classify(GATE_CLOSED), LineDisposition::QueueAsPrenote);
    }

    #[test]
    fn prenote_and_prompt_lines_still_reach_the_queue() {
        // Gate closed/paused classification is decided by the reader loop;
        // here we verify the queue side: a note queued under a closed gate
        // (pre-typed note path) is delivered by try_recv like any other.
        let (src, tx) = test_source();
        src.end_turn();
        tx.send("typed after the turn ended".to_string()).unwrap();
        assert_eq!(
            src.try_recv().as_deref(),
            Some("typed after the turn ended")
        );
    }
}
