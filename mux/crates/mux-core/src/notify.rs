//! Watches raw pty output for OSC desktop-notification sequences (OSC 9,
//! OSC 777, and the kitty notification protocol all parse into the same
//! command via `ghostty_vt::OscParser`), independent of and in parallel
//! with the authoritative VT parser in [`crate::surface`].
//!
//! This is a best-effort side channel, not a second terminal parser: it
//! only tracks enough state to recognize an OSC sequence's start (`ESC ]`)
//! and end (`BEL` or `ST`), and hands everything in between to
//! libghostty-vt's own OSC parser. A missed edge case here can't corrupt
//! or desync actual terminal rendering, since that's driven entirely by
//! the separate `term.vt_write()` call on the same bytes.

use ghostty_vt::OscParser;

/// Issue #92: per-pane cap on stored notification records. The ring is a
/// bounded FIFO: once it holds [`NOTIFICATION_RING_CAPACITY`] records, each
/// new one evicts the oldest, so a chatty pane can never grow the daemon's
/// memory without bound. Overridden in tests via
/// [`NotificationRing::with_capacity`].
pub const NOTIFICATION_RING_CAPACITY: usize = 100;

/// Issue #92: one durable desktop-notification record. `id` is a
/// daemon-global monotonic sequence (shared across panes), so a
/// per-client "last read id" is a single high-water mark — an `ack` for
/// a *different* pane's newer id also implicitly clears older unread
/// notifications in this pane, which is exactly the ordering a client
/// replays in.
///
/// `timestamp_ms` is a wall-clock UNIX-epoch millisecond value, purely
/// informational (display); ordering everywhere else uses `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationRecord {
    /// Daemon-global monotonic sequence number; also the ordering key.
    pub id: u64,
    /// Pane the notification came from.
    pub surface: u64,
    pub title: String,
    pub body: String,
    pub timestamp_ms: u64,
}

/// Issue #92: a bounded per-pane FIFO of [`NotificationRecord`]s. Records
/// are appended in id order; when the ring is full the oldest is evicted
/// ("keep the last N"). `unread_after` filters against a client's
/// last-read high-water mark.
#[derive(Debug)]
pub struct NotificationRing {
    records: std::collections::VecDeque<NotificationRecord>,
    capacity: usize,
}

impl NotificationRing {
    /// A ring at the default cap ([`NOTIFICATION_RING_CAPACITY`]).
    pub fn new() -> Self {
        Self::with_capacity(NOTIFICATION_RING_CAPACITY)
    }

    /// A ring with an explicit cap (tests). `0` is clamped to `1` so the
    /// ring can never silently drop every record.
    pub fn with_capacity(capacity: usize) -> Self {
        NotificationRing { records: std::collections::VecDeque::new(), capacity: capacity.max(1) }
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Push a record, evicting the oldest once at capacity. Returns the
    /// evicted record, if any (so callers can assert pruning).
    pub fn push(&mut self, record: NotificationRecord) -> Option<NotificationRecord> {
        let evicted =
            if self.records.len() >= self.capacity { self.records.pop_front() } else { None };
        self.records.push_back(record);
        evicted
    }

    /// Every record, oldest first.
    pub fn records(&self) -> impl Iterator<Item = &NotificationRecord> {
        self.records.iter()
    }

    /// Records with `id > after_id`, oldest first — the replay slice for
    /// a client whose last-read high-water mark is `after_id`.
    pub fn unread_after(&self, after_id: u64) -> impl Iterator<Item = &NotificationRecord> {
        self.records.iter().filter(move |r| r.id > after_id)
    }

    /// Highest id stored, or `None` when empty.
    pub fn latest_id(&self) -> Option<u64> {
        self.records.back().map(|r| r.id)
    }
}

impl Default for NotificationRing {
    fn default() -> Self {
        Self::new()
    }
}

/// Issue #92: wall-clock ms since the UNIX epoch (0 if the clock is
/// before the epoch, which cannot happen in practice). Kept here so the
/// mux has no extra time dependency.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    SawEsc,
    InOsc,
    InOscSawEsc,
}

pub struct OscWatcher {
    parser: OscParser,
    state: State,
}

impl OscWatcher {
    pub fn new() -> anyhow::Result<Self> {
        Ok(OscWatcher {
            parser: OscParser::new().map_err(|e| anyhow::anyhow!("{e:?}"))?,
            state: State::Idle,
        })
    }

    /// Scans a chunk of raw pty output, returning every desktop
    /// notification (title, body) whose terminating byte fell within it.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<(String, String)> {
        let mut found = Vec::new();
        for &byte in bytes {
            match self.state {
                State::Idle => {
                    if byte == 0x1b {
                        self.state = State::SawEsc;
                    }
                }
                State::SawEsc => {
                    self.state = if byte == b']' {
                        self.parser.reset();
                        State::InOsc
                    } else {
                        State::Idle
                    };
                }
                State::InOsc => match byte {
                    0x07 => {
                        if let Some(n) = self.parser.end(0x07).desktop_notification() {
                            found.push(n);
                        }
                        self.state = State::Idle;
                    }
                    0x1b => self.state = State::InOscSawEsc,
                    _ => self.parser.next(byte),
                },
                State::InOscSawEsc => {
                    if byte == b'\\' {
                        if let Some(n) = self.parser.end(0x5c).desktop_notification() {
                            found.push(n);
                        }
                    }
                    // Anything else after an ESC mid-OSC isn't a valid ST;
                    // abandon this sequence rather than misparse it.
                    self.state = State::Idle;
                }
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_bel_terminated_osc9() {
        let mut watcher = OscWatcher::new().unwrap();
        let found = watcher.feed(b"before \x1b]9;Build failed\x07 after");
        assert_eq!(found, vec![("".to_string(), "Build failed".to_string())]);
    }

    #[test]
    fn detects_st_terminated_osc777_with_title() {
        let mut watcher = OscWatcher::new().unwrap();
        let found = watcher.feed(b"\x1b]777;notify;Tests;3 failed\x1b\\");
        assert_eq!(found, vec![("Tests".to_string(), "3 failed".to_string())]);
    }

    #[test]
    fn ignores_non_notification_osc_sequences() {
        let mut watcher = OscWatcher::new().unwrap();
        // OSC 0: change window title - not a notification.
        let found = watcher.feed(b"\x1b]0;my shell\x07");
        assert!(found.is_empty());
    }

    #[test]
    fn sequence_split_across_multiple_feed_calls() {
        let mut watcher = OscWatcher::new().unwrap();
        assert!(watcher.feed(b"\x1b]9;Hello ").is_empty());
        let found = watcher.feed(b"world\x07");
        assert_eq!(found, vec![("".to_string(), "Hello world".to_string())]);
    }

    #[test]
    fn unrelated_escape_sequences_do_not_confuse_the_scanner() {
        let mut watcher = OscWatcher::new().unwrap();
        // A CSI sequence (cursor move) followed by a real notification.
        let found = watcher.feed(b"\x1b[2J\x1b]9;after clear\x07");
        assert_eq!(found, vec![("".to_string(), "after clear".to_string())]);
    }

    /// Issue #92 AC3: the ring keeps only the last N records.
    #[test]
    fn ring_prunes_to_capacity_keeping_the_newest() {
        let mut ring = NotificationRing::with_capacity(3);
        for id in 1..=5 {
            ring.push(NotificationRecord {
                id,
                surface: 7,
                title: String::new(),
                body: format!("n{id}"),
                timestamp_ms: id,
            });
        }
        assert_eq!(ring.len(), 3, "ring must not grow past its cap");
        let bodies: Vec<_> = ring.records().map(|r| r.body.clone()).collect();
        assert_eq!(bodies, vec!["n3", "n4", "n5"]);
        assert_eq!(ring.latest_id(), Some(5));
    }

    /// Issue #92: the evicted record is reported, and `unread_after` is a
    /// strict-greater filter over ids.
    #[test]
    fn ring_evicts_oldest_and_filters_unread() {
        let mut ring = NotificationRing::with_capacity(2);
        let rec = |id| NotificationRecord {
            id,
            surface: 1,
            title: String::new(),
            body: String::new(),
            timestamp_ms: 0,
        };
        assert!(ring.push(rec(10)).is_none());
        assert!(ring.push(rec(11)).is_none());
        let evicted = ring.push(rec(12)).expect("third push evicts the first");
        assert_eq!(evicted.id, 10);
        let unread: Vec<_> = ring.unread_after(10).map(|r| r.id).collect();
        assert_eq!(unread, vec![11, 12]);
        assert!(ring.unread_after(12).next().is_none(), "acked up to date replays nothing");
    }

    #[test]
    fn multiple_notifications_in_one_chunk() {
        let mut watcher = OscWatcher::new().unwrap();
        let found = watcher.feed(b"\x1b]9;first\x07 middle \x1b]9;second\x07");
        assert_eq!(
            found,
            vec![("".to_string(), "first".to_string()), ("".to_string(), "second".to_string())]
        );
    }
}
