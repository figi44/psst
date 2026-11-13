//! Pure clipboard state machine (plan 5.4 / 9.5). No Wayland here: the glue
//! in `paster` feeds [`Input`]s and executes the returned [`Action`]s.
//!
//! Rules implemented:
//! - snapshot one representation, plain text preferred, else the first type;
//! - inheritance: with a restore pending, if psst owns the selection or the
//!   clipboard's text equals the pending dictation, inherit its snapshot;
//! - an external selection change before the paste re-snapshots; psst's own
//!   changes don't;
//! - the snapshot freezes when the paste starts; the paste waits at most
//!   `snapshot_wait` for an unfinished snapshot, then skips restore;
//! - restore only after `Pasted`, after `restore_after`, if psst still owns
//!   the selection or the clipboard's text still equals the dictated text;
//! - generations: an older timer never touches a newer dictation;
//! - the restored snapshot is freed once something else owns the selection.

use std::sync::Arc;
use std::time::Duration;

pub type Gen = u64;
pub type ReadId = u64;
/// Glue-assigned handle for a data-control offer.
pub type OfferKey = u64;

pub const TEXT_MIMES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnKind {
    Dictation(Gen),
    Copy(Gen),
    Restore(Gen),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    Unknown,
    Empty,
    Ours(OwnKind),
    External { offer: OfferKey, mimes: Vec<String> },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub mime: String,
    pub data: Vec<u8>,
}

impl Snapshot {
    pub fn text(s: &str) -> Self {
        Snapshot { mime: TEXT_MIMES[0].to_owned(), data: s.as_bytes().to_vec() }
    }
    pub fn is_text(&self) -> bool {
        is_text_mime(&self.mime)
    }
    /// The types a source serving this snapshot offers.
    pub fn offer_mimes(&self) -> Vec<String> {
        if self.is_text() { TEXT_MIMES.iter().map(|s| s.to_string()).collect() } else { vec![self.mime.clone()] }
    }
    pub fn describe(&self) -> String {
        if self.is_text() {
            format!("{} {:?}", self.mime, String::from_utf8_lossy(&self.data))
        } else {
            format!("{} ({} bytes)", self.mime, self.data.len())
        }
    }
}

impl AsRef<[u8]> for Snapshot {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

pub fn is_text_mime(m: &str) -> bool {
    TEXT_MIMES.contains(&m)
}

/// The representation a snapshot keeps: the first available plain-text type
/// in preference order, else the first type offered.
pub fn choose_mime(mimes: &[String]) -> Option<&str> {
    choose_text_mime(mimes).or(mimes.first().map(|s| s.as_str()))
}

pub fn choose_text_mime(mimes: &[String]) -> Option<&str> {
    TEXT_MIMES.iter().find(|t| mimes.iter().any(|m| m == *t)).copied()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    Timeout,
    TooLarge,
    Io(String),
}

#[derive(Debug)]
pub enum Input {
    SnapshotRequested { generation: Gen },
    SelectionChanged(Owner),
    ReadDone { id: ReadId, result: Result<Vec<u8>, ReadError> },
    PasteRequested { generation: Gen, text: String },
    PasteWaitExpired { generation: Gen },
    CopyRequested { generation: Gen, text: String },
    Pasted { generation: Gen },
    RestoreDue { generation: Gen },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    SnapshotTaken { generation: Gen, what: Option<String> },
    SnapshotInherited { generation: Gen, from: Gen },
    SnapshotFailed { generation: Gen, error: ReadError },
    Resnapshot { generation: Gen },
    Frozen { generation: Gen, has_snapshot: bool },
    Restored { generation: Gen, what: String },
    RestoreSkipped { generation: Gen, reason: String },
    TimerIgnored { generation: Gen },
}

#[derive(Debug)]
pub enum Action {
    /// Receive `mime` from `offer` (≤ cap, within the timeout) → `ReadDone`.
    Read {
        id: ReadId,
        offer: OfferKey,
        mime: String,
    },
    CancelRead {
        id: ReadId,
    },
    /// Wait at most `max` for the snapshot → `PasteWaitExpired`.
    WaitForSnapshot {
        generation: Gen,
        max: Duration,
    },
    /// Set the selection to the dictated text, sync, chord, sync → `Pasted`.
    StartPaste {
        generation: Gen,
    },
    /// Set the selection to the text, sync → `Copied` (no paste, no restore).
    StartCopy {
        generation: Gen,
    },
    ScheduleRestore {
        generation: Gen,
        after: Duration,
    },
    /// Set the selection to a source serving `snapshot`.
    OfferRestore {
        generation: Gen,
        snapshot: Arc<Snapshot>,
    },
    Note(Note),
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub restore_after: Duration,
    pub snapshot_wait: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config { restore_after: Duration::from_secs(5), snapshot_wait: Duration::from_millis(100) }
    }
}

#[derive(Debug)]
enum SnapState {
    Reading { id: ReadId, mime: String },
    Done(Option<Arc<Snapshot>>),
}

#[derive(Debug)]
struct SnapReq {
    generation: Gen,
    state: SnapState,
}

#[derive(Debug)]
struct Pending {
    generation: Gen,
    text: String,
    snapshot: Option<Arc<Snapshot>>,
    armed: bool,
}

#[derive(Debug)]
pub struct Machine {
    cfg: Config,
    owner: Owner,
    next_read: ReadId,
    snap: Option<SnapReq>,
    waiting_paste: Option<(Gen, String)>,
    pending: Option<Pending>,
    restore_check: Option<(ReadId, Gen)>,
    /// What psst's current (or most recent) source serves; dropped when
    /// someone else owns the selection.
    own_content: Option<(OwnKind, Arc<Snapshot>)>,
}

impl Machine {
    pub fn new(cfg: Config) -> Self {
        Machine {
            cfg,
            owner: Owner::Unknown,
            next_read: 1,
            snap: None,
            waiting_paste: None,
            pending: None,
            restore_check: None,
            own_content: None,
        }
    }

    pub fn owner(&self) -> &Owner {
        &self.owner
    }

    /// Bytes held for restore or for serving psst's own selection.
    pub fn held_bytes(&self) -> usize {
        let p = self.pending.as_ref().and_then(|p| p.snapshot.as_ref()).map_or(0, |s| s.data.len());
        let o = self.own_content.as_ref().map_or(0, |(_, s)| s.data.len());
        p + o
    }

    pub fn has_pending_restore(&self) -> bool {
        self.pending.is_some()
    }

    pub fn handle(&mut self, input: Input) -> Vec<Action> {
        let mut out = Vec::new();
        match input {
            Input::SnapshotRequested { generation } => {
                self.cancel_snapshot_read(&mut out);
                let state = self.start_snapshot(generation, &mut out);
                self.snap = Some(SnapReq { generation, state });
            }
            Input::SelectionChanged(owner) => {
                let external = !matches!(owner, Owner::Ours(_));
                self.owner = owner;
                if external {
                    // Freed once something else owns the selection.
                    self.own_content = None;
                    if let Some(generation) = self.snap.as_ref().map(|s| s.generation) {
                        self.cancel_snapshot_read(&mut out);
                        out.push(Action::Note(Note::Resnapshot { generation }));
                        let state = self.start_snapshot(generation, &mut out);
                        self.snap = Some(SnapReq { generation, state });
                        // A paste already waiting keeps waiting for the new read.
                    }
                }
            }
            Input::ReadDone { id, result } => {
                if let Some(SnapReq { generation, state: SnapState::Reading { id: rid, mime } }) = &self.snap
                    && *rid == id
                {
                    let (generation, mime) = (*generation, mime.clone());
                    let done = match result {
                        Ok(data) => {
                            let s = Snapshot { mime, data };
                            match &self.pending {
                                Some(p) if s.is_text() && s.data == p.text.as_bytes() => {
                                    out.push(Action::Note(Note::SnapshotInherited { generation, from: p.generation }));
                                    p.snapshot.clone()
                                }
                                _ => {
                                    out.push(Action::Note(Note::SnapshotTaken {
                                        generation,
                                        what: Some(s.describe()),
                                    }));
                                    Some(Arc::new(s))
                                }
                            }
                        }
                        Err(error) => {
                            out.push(Action::Note(Note::SnapshotFailed { generation, error }));
                            None
                        }
                    };
                    self.snap = Some(SnapReq { generation, state: SnapState::Done(done) });
                    if let Some((wgen, _)) = &self.waiting_paste
                        && *wgen == generation
                    {
                        let (wgen, text) = self.waiting_paste.take().unwrap();
                        self.freeze(wgen, text, &mut out);
                    }
                } else if let Some((rid, generation)) = self.restore_check
                    && rid == id
                {
                    self.restore_check = None;
                    let matches = self.pending.as_ref().is_some_and(|p| p.generation == generation)
                        && matches!(&result, Ok(d) if d == self.pending.as_ref().unwrap().text.as_bytes());
                    if matches {
                        self.restore(generation, &mut out);
                    } else if self.pending.as_ref().is_some_and(|p| p.generation == generation) {
                        self.pending = None;
                        out.push(Action::Note(Note::RestoreSkipped { generation, reason: "clipboard changed".into() }));
                    }
                }
            }
            Input::PasteRequested { generation, text } => {
                if self.snap.as_ref().is_none_or(|s| s.generation != generation) {
                    // No SnapshotClipboard for this generation: take one now.
                    self.cancel_snapshot_read(&mut out);
                    let state = self.start_snapshot(generation, &mut out);
                    self.snap = Some(SnapReq { generation, state });
                }
                if matches!(self.snap.as_ref().map(|s| &s.state), Some(SnapState::Reading { .. })) {
                    self.waiting_paste = Some((generation, text));
                    out.push(Action::WaitForSnapshot { generation, max: self.cfg.snapshot_wait });
                } else {
                    self.freeze(generation, text, &mut out);
                }
            }
            Input::PasteWaitExpired { generation } => {
                if self.waiting_paste.as_ref().is_some_and(|(g, _)| *g == generation) {
                    let (generation, text) = self.waiting_paste.take().unwrap();
                    self.freeze(generation, text, &mut out);
                }
            }
            Input::CopyRequested { generation, text } => {
                // Copy only: the text replaces the clipboard on purpose, so a
                // pending restore must not undo it.
                self.cancel_snapshot_read(&mut out);
                self.snap = None;
                self.waiting_paste = None;
                if let Some(p) = self.pending.take() {
                    out.push(Action::Note(Note::RestoreSkipped {
                        generation: p.generation,
                        reason: "copy requested".into(),
                    }));
                }
                self.own_content = Some((OwnKind::Copy(generation), Arc::new(Snapshot::text(&text))));
                out.push(Action::StartCopy { generation });
            }
            Input::Pasted { generation } => {
                if let Some(p) = self.pending.as_mut().filter(|p| p.generation == generation) {
                    p.armed = true;
                    out.push(Action::ScheduleRestore { generation, after: self.cfg.restore_after });
                }
            }
            Input::RestoreDue { generation } => {
                if !self.pending.as_ref().is_some_and(|p| p.generation == generation && p.armed) {
                    out.push(Action::Note(Note::TimerIgnored { generation }));
                    return out;
                }
                match self.owner.clone() {
                    Owner::Ours(OwnKind::Dictation(g)) if g == generation => self.restore(generation, &mut out),
                    Owner::External { offer, mimes } => match choose_text_mime(&mimes) {
                        Some(m) => {
                            let id = self.alloc_read();
                            out.push(Action::Read { id, offer, mime: m.to_owned() });
                            self.restore_check = Some((id, generation));
                        }
                        None => {
                            self.pending = None;
                            out.push(Action::Note(Note::RestoreSkipped {
                                generation,
                                reason: "clipboard changed (no text)".into(),
                            }));
                        }
                    },
                    other => {
                        let reason = format!("selection is now {other:?}");
                        self.pending = None;
                        out.push(Action::Note(Note::RestoreSkipped { generation, reason }));
                    }
                }
            }
        }
        out
    }

    fn alloc_read(&mut self) -> ReadId {
        let id = self.next_read;
        self.next_read += 1;
        id
    }

    fn cancel_snapshot_read(&mut self, out: &mut Vec<Action>) {
        if let Some(SnapReq { state: SnapState::Reading { id, .. }, .. }) = &self.snap {
            out.push(Action::CancelRead { id: *id });
        }
    }

    fn start_snapshot(&mut self, generation: Gen, out: &mut Vec<Action>) -> SnapState {
        match self.owner.clone() {
            Owner::Ours(OwnKind::Dictation(g)) if self.pending.as_ref().is_some_and(|p| p.generation == g) => {
                out.push(Action::Note(Note::SnapshotInherited { generation, from: g }));
                SnapState::Done(self.pending.as_ref().unwrap().snapshot.clone())
            }
            Owner::Ours(_) => {
                // psst serves the clipboard (a copy, a restore, or a dictation
                // whose restore was dropped): snapshot what it serves.
                let s = self.own_content.as_ref().map(|(_, s)| s.clone());
                out.push(Action::Note(Note::SnapshotTaken { generation, what: s.as_ref().map(|s| s.describe()) }));
                SnapState::Done(s)
            }
            Owner::External { offer, mimes } => match choose_mime(&mimes) {
                Some(m) => {
                    let id = self.alloc_read();
                    out.push(Action::Read { id, offer, mime: m.to_owned() });
                    SnapState::Reading { id, mime: m.to_owned() }
                }
                None => {
                    out.push(Action::Note(Note::SnapshotTaken { generation, what: None }));
                    SnapState::Done(None)
                }
            },
            Owner::Empty | Owner::Unknown => {
                out.push(Action::Note(Note::SnapshotTaken { generation, what: None }));
                SnapState::Done(None)
            }
        }
    }

    fn freeze(&mut self, generation: Gen, text: String, out: &mut Vec<Action>) {
        let snapshot = match self.snap.take() {
            Some(SnapReq { generation: g, state: SnapState::Done(s) }) if g == generation => s,
            Some(SnapReq { state: SnapState::Reading { id, .. }, .. }) => {
                out.push(Action::CancelRead { id });
                out.push(Action::Note(Note::RestoreSkipped {
                    generation,
                    reason: format!("snapshot not ready within {:?}", self.cfg.snapshot_wait),
                }));
                None
            }
            _ => None,
        };
        out.push(Action::Note(Note::Frozen { generation, has_snapshot: snapshot.is_some() }));
        // The restore state exists before the selection changes.
        self.own_content = Some((OwnKind::Dictation(generation), Arc::new(Snapshot::text(&text))));
        self.pending = Some(Pending { generation, text, snapshot, armed: false });
        out.push(Action::StartPaste { generation });
    }

    fn restore(&mut self, generation: Gen, out: &mut Vec<Action>) {
        let p = self.pending.take().expect("restore without pending");
        match p.snapshot {
            Some(s) => {
                out.push(Action::Note(Note::Restored { generation, what: s.describe() }));
                self.own_content = Some((OwnKind::Restore(generation), s.clone()));
                out.push(Action::OfferRestore { generation, snapshot: s });
            }
            None => out.push(Action::Note(Note::RestoreSkipped {
                generation,
                reason: "no snapshot (clipboard was empty or unreadable)".into(),
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(offer: OfferKey, mimes: &[&str]) -> Owner {
        Owner::External { offer, mimes: mimes.iter().map(|s| s.to_string()).collect() }
    }

    fn read_of(actions: &[Action]) -> Option<(ReadId, OfferKey, String)> {
        actions.iter().find_map(|a| match a {
            Action::Read { id, offer, mime } => Some((*id, *offer, mime.clone())),
            _ => None,
        })
    }

    fn restored(actions: &[Action]) -> Option<Arc<Snapshot>> {
        actions.iter().find_map(|a| match a {
            Action::OfferRestore { snapshot, .. } => Some(snapshot.clone()),
            _ => None,
        })
    }

    fn has(actions: &[Action], f: impl Fn(&Action) -> bool) -> bool {
        actions.iter().any(f)
    }

    /// Snapshot of an external text selection, paste, Pasted.
    fn dictate(m: &mut Machine, generation: Gen, text: &str, clip: Option<&str>) {
        let a = m.handle(Input::SnapshotRequested { generation });
        if let Some(clip) = clip {
            let (id, _, _) = read_of(&a).expect("read");
            m.handle(Input::ReadDone { id, result: Ok(clip.as_bytes().to_vec()) });
        }
        let a = m.handle(Input::PasteRequested { generation, text: text.into() });
        assert!(has(&a, |a| matches!(a, Action::StartPaste { generation: g } if *g == generation)));
        m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Dictation(generation))));
        let a = m.handle(Input::Pasted { generation });
        assert!(has(&a, |a| matches!(a, Action::ScheduleRestore { .. })));
    }

    #[test]
    fn prefers_plain_text_then_first_type() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(choose_mime(&m(&["text/html", "STRING", "text/plain"])), Some("text/plain"));
        assert_eq!(choose_mime(&m(&["image/png", "image/jpeg"])), Some("image/png"));
        assert_eq!(choose_mime(&m(&[])), None);
    }

    #[test]
    fn restores_when_psst_still_owns() {
        let mut m = Machine::new(Config::default());
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        dictate(&mut m, 1, "dictated", Some("orig"));
        let a = m.handle(Input::RestoreDue { generation: 1 });
        assert_eq!(restored(&a).unwrap().data, b"orig");
        assert!(!m.has_pending_restore());
    }

    #[test]
    fn external_change_before_paste_resnapshots_own_does_not() {
        let mut m = Machine::new(Config::default());
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        let a = m.handle(Input::SnapshotRequested { generation: 1 });
        let (id, _, _) = read_of(&a).unwrap();
        m.handle(Input::ReadDone { id, result: Ok(b"first".to_vec()) });
        let a = m.handle(Input::SelectionChanged(ext(2, &["text/plain"])));
        let (id2, offer, _) = read_of(&a).expect("re-snapshot");
        assert_eq!(offer, 2);
        m.handle(Input::ReadDone { id: id2, result: Ok(b"second".to_vec()) });
        let a = m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Copy(9))));
        assert!(read_of(&a).is_none(), "own change must not re-snapshot");
        m.handle(Input::PasteRequested { generation: 1, text: "d".into() });
        m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Dictation(1))));
        m.handle(Input::Pasted { generation: 1 });
        let a = m.handle(Input::RestoreDue { generation: 1 });
        assert_eq!(restored(&a).unwrap().data, b"second");
    }

    #[test]
    fn two_quick_dictations_keep_the_original() {
        let mut m = Machine::new(Config::default());
        m.handle(Input::SelectionChanged(ext(1, &["image/png"])));
        dictate(&mut m, 1, "one", Some("PNGDATA"));
        // Second dictation while psst owns the selection: inherits.
        let a = m.handle(Input::SnapshotRequested { generation: 2 });
        assert!(read_of(&a).is_none());
        assert!(has(&a, |a| matches!(a, Action::Note(Note::SnapshotInherited { generation: 2, from: 1 }))));
        m.handle(Input::PasteRequested { generation: 2, text: "two".into() });
        m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Dictation(2))));
        m.handle(Input::Pasted { generation: 2 });
        // The first timer fires: superseded, ignored.
        let a = m.handle(Input::RestoreDue { generation: 1 });
        assert!(restored(&a).is_none());
        assert!(has(&a, |a| matches!(a, Action::Note(Note::TimerIgnored { generation: 1 }))));
        let a = m.handle(Input::RestoreDue { generation: 2 });
        let s = restored(&a).unwrap();
        assert_eq!((s.mime.as_str(), s.data.as_slice()), ("image/png", &b"PNGDATA"[..]));
    }

    #[test]
    fn inherits_when_a_clipboard_manager_took_over_the_dictation() {
        let mut m = Machine::new(Config::default());
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        dictate(&mut m, 1, "one", Some("orig"));
        m.handle(Input::SelectionChanged(ext(2, &["text/plain;charset=utf-8"])));
        let a = m.handle(Input::SnapshotRequested { generation: 2 });
        let (id, _, _) = read_of(&a).unwrap();
        let a = m.handle(Input::ReadDone { id, result: Ok(b"one".to_vec()) });
        assert!(has(&a, |a| matches!(a, Action::Note(Note::SnapshotInherited { generation: 2, from: 1 }))));
        m.handle(Input::PasteRequested { generation: 2, text: "two".into() });
        m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Dictation(2))));
        m.handle(Input::Pasted { generation: 2 });
        assert_eq!(restored(&m.handle(Input::RestoreDue { generation: 2 })).unwrap().data, b"orig");
    }

    #[test]
    fn restores_through_a_manager_holding_the_dictated_text() {
        let mut m = Machine::new(Config::default());
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        dictate(&mut m, 1, "one", Some("orig"));
        m.handle(Input::SelectionChanged(ext(2, &["text/plain"])));
        let a = m.handle(Input::RestoreDue { generation: 1 });
        let (id, offer, _) = read_of(&a).unwrap();
        assert_eq!(offer, 2);
        let a = m.handle(Input::ReadDone { id, result: Ok(b"one".to_vec()) });
        assert_eq!(restored(&a).unwrap().data, b"orig");
    }

    #[test]
    fn no_restore_after_the_user_copied_something_else() {
        let mut m = Machine::new(Config::default());
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        dictate(&mut m, 1, "one", Some("orig"));
        m.handle(Input::SelectionChanged(ext(2, &["text/plain"])));
        let a = m.handle(Input::RestoreDue { generation: 1 });
        let (id, _, _) = read_of(&a).unwrap();
        let a = m.handle(Input::ReadDone { id, result: Ok(b"user copy".to_vec()) });
        assert!(restored(&a).is_none());
        assert!(!m.has_pending_restore());
        // Non-text owner: skipped without reading.
        m.handle(Input::SelectionChanged(Owner::Empty));
        dictate(&mut m, 2, "two", None);
        m.handle(Input::SelectionChanged(ext(3, &["image/png"])));
        let a = m.handle(Input::RestoreDue { generation: 2 });
        assert!(read_of(&a).is_none() && restored(&a).is_none());
    }

    #[test]
    fn slow_snapshot_skips_restore_after_the_wait() {
        let mut m = Machine::new(Config::default());
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        let a = m.handle(Input::SnapshotRequested { generation: 1 });
        let (id, _, _) = read_of(&a).unwrap();
        let a = m.handle(Input::PasteRequested { generation: 1, text: "d".into() });
        assert!(has(&a, |a| matches!(a, Action::WaitForSnapshot { .. })));
        let a = m.handle(Input::PasteWaitExpired { generation: 1 });
        assert!(has(&a, |a| matches!(a, Action::CancelRead { id: i } if *i == id)));
        assert!(has(&a, |a| matches!(a, Action::StartPaste { generation: 1 })));
        // A late read result is ignored.
        m.handle(Input::ReadDone { id, result: Ok(b"late".to_vec()) });
        m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Dictation(1))));
        m.handle(Input::Pasted { generation: 1 });
        let a = m.handle(Input::RestoreDue { generation: 1 });
        assert!(restored(&a).is_none());
    }

    #[test]
    fn snapshot_finishing_within_the_wait_starts_the_paste() {
        let mut m = Machine::new(Config::default());
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        let a = m.handle(Input::SnapshotRequested { generation: 1 });
        let (id, _, _) = read_of(&a).unwrap();
        m.handle(Input::PasteRequested { generation: 1, text: "d".into() });
        let a = m.handle(Input::ReadDone { id, result: Ok(b"orig".to_vec()) });
        assert!(has(&a, |a| matches!(a, Action::StartPaste { generation: 1 })));
        assert!(has(&a, |a| matches!(a, Action::Note(Note::Frozen { generation: 1, has_snapshot: true }))));
    }

    #[test]
    fn empty_clipboard_leaves_the_dictation() {
        let mut m = Machine::new(Config::default());
        m.handle(Input::SelectionChanged(Owner::Empty));
        dictate(&mut m, 1, "one", None);
        let a = m.handle(Input::RestoreDue { generation: 1 });
        assert!(restored(&a).is_none());
    }

    #[test]
    fn restored_snapshot_is_freed_when_replaced() {
        let mut m = Machine::new(Config::default());
        m.handle(Input::SelectionChanged(ext(1, &["image/png"])));
        dictate(&mut m, 1, "one", Some("PNG"));
        let s = restored(&m.handle(Input::RestoreDue { generation: 1 })).unwrap();
        m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Restore(1))));
        assert!(m.held_bytes() > 0);
        drop(s);
        m.handle(Input::SelectionChanged(ext(2, &["text/plain"])));
        assert_eq!(m.held_bytes(), 0);
    }

    #[test]
    fn copy_cancels_a_pending_restore() {
        let mut m = Machine::new(Config::default());
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        dictate(&mut m, 1, "one", Some("orig"));
        let a = m.handle(Input::CopyRequested { generation: 2, text: "copied".into() });
        assert!(has(&a, |a| matches!(a, Action::StartCopy { generation: 2 })));
        m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Copy(2))));
        let a = m.handle(Input::RestoreDue { generation: 1 });
        assert!(restored(&a).is_none());
    }
}
