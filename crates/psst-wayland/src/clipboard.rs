//! Pure: no Wayland, no clock.
//!
//! - While a restore is pending, a snapshot of psst's own selection, or of text equal to the
//!   pending dictation's (a clipboard manager took it over), is the pending restore's snapshot:
//!   two quick dictations keep the original.
//! - Restore only after `Pasted`, and only if psst still owns the selection or the clipboard's
//!   text still equals the dictated text. The generation check keeps an older timer away from a
//!   newer dictation. An empty or unreadable original isn't restored: the dictation stays.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

pub type Gen = u64;
pub type ReadId = u64;
pub type OfferKey = u64;

/// In order of preference.
pub const TEXT_MIMES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];

pub fn is_text_mime(mime: &str) -> bool {
    TEXT_MIMES.contains(&mime)
}

pub fn choose_mime(mimes: &[String]) -> Option<&str> {
    choose_text_mime(mimes).or(mimes.first().map(String::as_str))
}

pub fn choose_text_mime(mimes: &[String]) -> Option<&'static str> {
    TEXT_MIMES.iter().find(|t| mimes.iter().any(|m| m == *t)).copied()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnKind {
    Dictation(Gen),
    Copy(Gen),
    Restore(Gen),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    /// No selection event since connecting.
    Unknown,
    Empty,
    Ours(OwnKind),
    External {
        offer: OfferKey,
        mimes: Vec<String>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Content {
    pub mime: String,
    pub data: Vec<u8>,
}

impl Content {
    pub fn text(s: &str) -> Self {
        Content { mime: TEXT_MIMES[0].to_owned(), data: s.as_bytes().to_vec() }
    }

    pub fn is_text(&self) -> bool {
        is_text_mime(&self.mime)
    }

    pub fn offer_mimes(&self) -> Vec<String> {
        if self.is_text() { TEXT_MIMES.iter().map(|s| s.to_string()).collect() } else { vec![self.mime.clone()] }
    }

    /// For logs: never the content.
    pub fn describe(&self) -> String {
        format!("{} ({} bytes)", self.mime, self.data.len())
    }
}

impl AsRef<[u8]> for Content {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    Timeout,
    TooLarge,
    Io(String),
}

#[derive(Debug)]
pub enum Input {
    SnapshotRequested {
        generation: Gen,
    },
    SnapshotExpired {
        epoch: u64,
    },
    SelectionChanged(Owner),
    ReadDone {
        id: ReadId,
        result: Result<Vec<u8>, ReadError>,
    },
    PasteRequested {
        generation: Gen,
        text: String,
    },
    CopyRequested {
        generation: Gen,
        text: String,
    },
    PasteWaitExpired {
        generation: Gen,
    },
    /// The sync after the chord came back.
    Pasted {
        generation: Gen,
    },
    Copied {
        generation: Gen,
    },
    Failed {
        generation: Gen,
    },
    RestoreDue {
        generation: Gen,
    },
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    SnapshotTaken { what: Option<String> },
    SnapshotInherited { from: Gen },
    SnapshotFailed { error: ReadError },
    SnapshotExpired,
    Resnapshot,
    Frozen { generation: Gen, has_snapshot: bool },
    Restored { generation: Gen, what: String },
    RestoreSkipped { generation: Gen, reason: String },
    TimerIgnored { generation: Gen },
}

#[derive(Debug)]
pub enum Action {
    Read { id: ReadId, offer: OfferKey, mime: String },
    CancelRead { id: ReadId },
    ExpireSnapshot { epoch: u64, after: Duration },
    WaitForSnapshot { generation: Gen, max: Duration },
    StartPaste { generation: Gen, content: Arc<Content> },
    StartCopy { generation: Gen, content: Arc<Content> },
    ScheduleRestore { generation: Gen, after: Duration },
    OfferRestore { generation: Gen, content: Arc<Content> },
    Note(Note),
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub restore_after: Duration,
    pub snapshot_wait: Duration,
    pub snapshot_ttl: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            restore_after: Duration::from_secs(5),
            snapshot_wait: Duration::from_millis(100),
            snapshot_ttl: Duration::from_secs(120),
        }
    }
}

#[derive(Debug)]
enum SnapState {
    Reading { id: ReadId, mime: String },
    Done(Option<Arc<Content>>),
}

#[derive(Debug)]
struct Snap {
    epoch: u64,
    state: SnapState,
}

#[derive(Debug)]
enum Op {
    Paste { generation: Gen, text: String },
    Copy { generation: Gen, text: String },
}

#[derive(Debug)]
enum Active {
    WaitingForSnapshot { generation: Gen, text: String },
    Pasting { generation: Gen },
    Copying { generation: Gen },
}

impl Active {
    fn generation(&self) -> Gen {
        match self {
            Active::WaitingForSnapshot { generation, .. }
            | Active::Pasting { generation }
            | Active::Copying { generation } => *generation,
        }
    }
}

#[derive(Debug)]
struct Pending {
    generation: Gen,
    text: String,
    snapshot: Option<Arc<Content>>,
    armed: bool,
}

#[derive(Debug)]
pub struct Machine {
    cfg: Config,
    owner: Owner,
    next_read: ReadId,
    next_epoch: u64,
    snap: Option<Snap>,
    queue: VecDeque<Op>,
    active: Option<Active>,
    pending: Option<Pending>,
    restore_check: Option<(ReadId, Gen)>,
    /// Dropped once someone else owns the selection.
    own: Option<(OwnKind, Arc<Content>)>,
}

impl Machine {
    pub fn new(cfg: Config) -> Self {
        Machine {
            cfg,
            owner: Owner::Unknown,
            next_read: 1,
            next_epoch: 1,
            snap: None,
            queue: VecDeque::new(),
            active: None,
            pending: None,
            restore_check: None,
            own: None,
        }
    }

    pub fn owner(&self) -> &Owner {
        &self.owner
    }

    pub fn has_op(&self, generation: Gen) -> bool {
        self.active.as_ref().is_some_and(|a| a.generation() == generation)
            || self.queue.iter().any(|op| match op {
                Op::Paste { generation: g, .. } | Op::Copy { generation: g, .. } => *g == generation,
            })
    }

    pub fn has_pending_restore(&self) -> bool {
        self.pending.is_some()
    }

    /// Shared content is counted once per holder.
    pub fn held_bytes(&self) -> usize {
        let snap = match &self.snap {
            Some(Snap { state: SnapState::Done(Some(c)), .. }) => c.data.len(),
            _ => 0,
        };
        let pending = self.pending.as_ref().and_then(|p| p.snapshot.as_ref()).map_or(0, |c| c.data.len());
        let own = self.own.as_ref().map_or(0, |(_, c)| c.data.len());
        snap + pending + own
    }

    pub fn handle(&mut self, input: Input) -> Vec<Action> {
        let mut out = Vec::new();
        match input {
            Input::SnapshotRequested { generation: _ } => {
                let epoch = self.alloc_epoch();
                match &mut self.snap {
                    // Tracking keeps it current.
                    Some(snap) => snap.epoch = epoch,
                    None => {
                        let state = self.start_snapshot(&mut out);
                        self.snap = Some(Snap { epoch, state });
                    }
                }
                out.push(Action::ExpireSnapshot { epoch, after: self.cfg.snapshot_ttl });
            }
            Input::SnapshotExpired { epoch } => {
                let waiting = matches!(self.active, Some(Active::WaitingForSnapshot { .. }));
                if !waiting && self.snap.as_ref().is_some_and(|s| s.epoch == epoch) {
                    self.drop_snapshot(&mut out);
                    out.push(Action::Note(Note::SnapshotExpired));
                }
            }
            Input::SelectionChanged(owner) => {
                let external = !matches!(owner, Owner::Ours(_));
                self.owner = owner;
                if external {
                    self.own = None;
                    if let Some(snap) = self.snap.take() {
                        if let SnapState::Reading { id, .. } = snap.state {
                            out.push(Action::CancelRead { id });
                        }
                        out.push(Action::Note(Note::Resnapshot));
                        let state = self.start_snapshot(&mut out);
                        self.snap = Some(Snap { epoch: snap.epoch, state });
                    }
                }
                // A restore check read an older selection: check the new one instead.
                if let Some((id, generation)) = self.restore_check.take() {
                    out.push(Action::CancelRead { id });
                    self.restore_due(generation, &mut out);
                }
            }
            Input::ReadDone { id, result } => self.read_done(id, result, &mut out),
            Input::PasteRequested { generation, text } => {
                self.queue.push_back(Op::Paste { generation, text });
                self.pump(&mut out);
            }
            Input::CopyRequested { generation, text } => {
                self.queue.push_back(Op::Copy { generation, text });
                self.pump(&mut out);
            }
            Input::PasteWaitExpired { generation } => {
                if let Some(Active::WaitingForSnapshot { generation: g, .. }) = &self.active
                    && *g == generation
                    && let Some(Active::WaitingForSnapshot { generation, text }) = self.active.take()
                {
                    self.freeze(generation, text, &mut out);
                }
            }
            Input::Pasted { generation } => {
                if matches!(self.active, Some(Active::Pasting { generation: g }) if g == generation) {
                    self.active = None;
                    if let Some(p) = self.pending.as_mut().filter(|p| p.generation == generation) {
                        p.armed = true;
                        out.push(Action::ScheduleRestore { generation, after: self.cfg.restore_after });
                    }
                    self.pump(&mut out);
                }
            }
            Input::Copied { generation } => {
                if matches!(self.active, Some(Active::Copying { generation: g }) if g == generation) {
                    self.active = None;
                    self.pump(&mut out);
                }
            }
            Input::Failed { generation } => {
                self.queue.retain(|op| !matches!(op, Op::Paste { generation: g, .. } | Op::Copy { generation: g, .. } if *g == generation));
                if self.active.as_ref().is_some_and(|a| a.generation() == generation) {
                    if let Some(Active::WaitingForSnapshot { .. }) = self.active.take()
                        && let Some(Snap { state: SnapState::Reading { id, .. }, .. }) = self.snap.take()
                    {
                        out.push(Action::CancelRead { id });
                    }
                    // The dictation stays in the clipboard, where the user can paste it.
                    if self.pending.as_ref().is_some_and(|p| p.generation == generation) {
                        self.cancel_restore(generation, "the paste failed", &mut out);
                    }
                    self.pump(&mut out);
                }
            }
            Input::RestoreDue { generation } => self.restore_due(generation, &mut out),
            Input::Reset => {
                // Ids keep counting: timers from before the reset may still fire.
                let (next_read, next_epoch) = (self.next_read, self.next_epoch);
                *self = Machine { next_read, next_epoch, ..Machine::new(self.cfg) };
            }
        }
        out
    }

    fn alloc_read(&mut self) -> ReadId {
        let id = self.next_read;
        self.next_read += 1;
        id
    }

    fn alloc_epoch(&mut self) -> u64 {
        let epoch = self.next_epoch;
        self.next_epoch += 1;
        epoch
    }

    fn drop_snapshot(&mut self, out: &mut Vec<Action>) {
        if let Some(Snap { state: SnapState::Reading { id, .. }, .. }) = self.snap.take() {
            out.push(Action::CancelRead { id });
        }
    }

    fn cancel_restore(&mut self, generation: Gen, reason: &str, out: &mut Vec<Action>) {
        self.pending = None;
        if let Some((id, _)) = self.restore_check.take() {
            out.push(Action::CancelRead { id });
        }
        out.push(Action::Note(Note::RestoreSkipped { generation, reason: reason.into() }));
    }

    fn start_snapshot(&mut self, out: &mut Vec<Action>) -> SnapState {
        match self.owner.clone() {
            Owner::Ours(OwnKind::Dictation(g)) if self.pending.as_ref().is_some_and(|p| p.generation == g) => {
                out.push(Action::Note(Note::SnapshotInherited { from: g }));
                SnapState::Done(self.pending.as_ref().and_then(|p| p.snapshot.clone()))
            }
            Owner::Ours(_) => {
                let content = self.own.as_ref().map(|(_, c)| c.clone());
                out.push(Action::Note(Note::SnapshotTaken { what: content.as_ref().map(|c| c.describe()) }));
                SnapState::Done(content)
            }
            Owner::External { offer, mimes } => match choose_mime(&mimes) {
                Some(mime) => {
                    let id = self.alloc_read();
                    out.push(Action::Read { id, offer, mime: mime.to_owned() });
                    SnapState::Reading { id, mime: mime.to_owned() }
                }
                None => {
                    out.push(Action::Note(Note::SnapshotTaken { what: None }));
                    SnapState::Done(None)
                }
            },
            Owner::Empty | Owner::Unknown => {
                out.push(Action::Note(Note::SnapshotTaken { what: None }));
                SnapState::Done(None)
            }
        }
    }

    fn read_done(&mut self, id: ReadId, result: Result<Vec<u8>, ReadError>, out: &mut Vec<Action>) {
        if let Some(Snap { state: SnapState::Reading { id: rid, mime }, .. }) = &self.snap
            && *rid == id
        {
            let mime = mime.clone();
            let content = match result {
                Ok(data) if data.is_empty() => {
                    out.push(Action::Note(Note::SnapshotTaken { what: None }));
                    None
                }
                Ok(data) => {
                    let content = Content { mime, data };
                    match &self.pending {
                        Some(p) if content.is_text() && content.data == p.text.as_bytes() => {
                            out.push(Action::Note(Note::SnapshotInherited { from: p.generation }));
                            p.snapshot.clone()
                        }
                        _ => {
                            out.push(Action::Note(Note::SnapshotTaken { what: Some(content.describe()) }));
                            Some(Arc::new(content))
                        }
                    }
                }
                Err(error) => {
                    out.push(Action::Note(Note::SnapshotFailed { error }));
                    None
                }
            };
            if let Some(snap) = &mut self.snap {
                snap.state = SnapState::Done(content);
            }
            self.resume_waiting_paste(out);
        } else if let Some((rid, generation)) = self.restore_check
            && rid == id
        {
            self.restore_check = None;
            let still_dictated = self
                .pending
                .as_ref()
                .is_some_and(|p| p.generation == generation && matches!(&result, Ok(d) if d == p.text.as_bytes()));
            if still_dictated {
                self.restore(generation, out);
            } else if self.pending.as_ref().is_some_and(|p| p.generation == generation) {
                self.cancel_restore(generation, "the clipboard changed", out);
            }
        }
    }

    fn pump(&mut self, out: &mut Vec<Action>) {
        while self.active.is_none() {
            let Some(op) = self.queue.pop_front() else { return };
            match op {
                Op::Paste { generation, text } => {
                    if self.snap.is_none() {
                        let epoch = self.alloc_epoch();
                        let state = self.start_snapshot(out);
                        self.snap = Some(Snap { epoch, state });
                    }
                    if matches!(self.snap, Some(Snap { state: SnapState::Reading { .. }, .. })) {
                        self.active = Some(Active::WaitingForSnapshot { generation, text });
                        out.push(Action::WaitForSnapshot { generation, max: self.cfg.snapshot_wait });
                    } else {
                        self.freeze(generation, text, out);
                    }
                }
                Op::Copy { generation, text } => {
                    // The copy replaces the clipboard on purpose: nothing may undo it.
                    if let Some(p) = &self.pending {
                        let g = p.generation;
                        self.cancel_restore(g, "a copy replaced the clipboard", out);
                    }
                    self.drop_snapshot(out);
                    let content = Arc::new(Content::text(&text));
                    self.own = Some((OwnKind::Copy(generation), content.clone()));
                    self.owner = Owner::Ours(OwnKind::Copy(generation));
                    self.active = Some(Active::Copying { generation });
                    out.push(Action::StartCopy { generation, content });
                }
            }
        }
    }

    fn resume_waiting_paste(&mut self, out: &mut Vec<Action>) {
        if let Some(Active::WaitingForSnapshot { .. }) = self.active
            && matches!(self.snap, Some(Snap { state: SnapState::Done(_), .. }))
            && let Some(Active::WaitingForSnapshot { generation, text }) = self.active.take()
        {
            self.freeze(generation, text, out);
        }
    }

    fn freeze(&mut self, generation: Gen, text: String, out: &mut Vec<Action>) {
        let snapshot = match self.snap.take() {
            Some(Snap { state: SnapState::Done(content), .. }) => content,
            Some(Snap { state: SnapState::Reading { id, .. }, .. }) => {
                out.push(Action::CancelRead { id });
                out.push(Action::Note(Note::RestoreSkipped {
                    generation,
                    reason: format!("the snapshot wasn't ready within {:?}", self.cfg.snapshot_wait),
                }));
                None
            }
            None => None,
        };
        out.push(Action::Note(Note::Frozen { generation, has_snapshot: snapshot.is_some() }));
        if let Some((id, _)) = self.restore_check.take() {
            out.push(Action::CancelRead { id });
        }
        // The restore state exists before the selection changes.
        let content = Arc::new(Content::text(&text));
        self.pending = Some(Pending { generation, text, snapshot, armed: false });
        self.own = Some((OwnKind::Dictation(generation), content.clone()));
        self.owner = Owner::Ours(OwnKind::Dictation(generation));
        self.active = Some(Active::Pasting { generation });
        out.push(Action::StartPaste { generation, content });
    }

    fn restore_due(&mut self, generation: Gen, out: &mut Vec<Action>) {
        if !self.pending.as_ref().is_some_and(|p| p.generation == generation && p.armed) {
            out.push(Action::Note(Note::TimerIgnored { generation }));
            return;
        }
        match self.owner.clone() {
            Owner::Ours(OwnKind::Dictation(g)) if g == generation => self.restore(generation, out),
            Owner::External { offer, mimes } => match choose_text_mime(&mimes) {
                Some(mime) => {
                    // Maybe a clipboard manager holding the dictated text: compare.
                    let id = self.alloc_read();
                    out.push(Action::Read { id, offer, mime: mime.to_owned() });
                    self.restore_check = Some((id, generation));
                }
                None => self.cancel_restore(generation, "the clipboard changed", out),
            },
            _ => self.cancel_restore(generation, "the clipboard changed", out),
        }
    }

    fn restore(&mut self, generation: Gen, out: &mut Vec<Action>) {
        let Some(p) = self.pending.take() else { return };
        let Some(content) = p.snapshot else {
            out.push(Action::Note(Note::RestoreSkipped {
                generation,
                reason: "nothing to restore (the clipboard was empty or unreadable)".into(),
            }));
            return;
        };
        out.push(Action::Note(Note::Restored { generation, what: content.describe() }));
        self.own = Some((OwnKind::Restore(generation), content.clone()));
        self.owner = Owner::Ours(OwnKind::Restore(generation));
        // A tracked snapshot now sees the restored content.
        if let Some(snap) = &mut self.snap {
            if let SnapState::Reading { id, .. } = snap.state {
                out.push(Action::CancelRead { id });
            }
            snap.state = SnapState::Done(Some(content.clone()));
        }
        out.push(Action::OfferRestore { generation, content });
        self.resume_waiting_paste(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(offer: OfferKey, mimes: &[&str]) -> Owner {
        Owner::External { offer, mimes: mimes.iter().map(|s| s.to_string()).collect() }
    }

    fn machine() -> Machine {
        Machine::new(Config::default())
    }

    fn read_of(actions: &[Action]) -> Option<(ReadId, OfferKey, String)> {
        actions.iter().find_map(|a| match a {
            Action::Read { id, offer, mime } => Some((*id, *offer, mime.clone())),
            _ => None,
        })
    }

    fn restored(actions: &[Action]) -> Option<Arc<Content>> {
        actions.iter().find_map(|a| match a {
            Action::OfferRestore { content, .. } => Some(content.clone()),
            _ => None,
        })
    }

    fn started_paste(actions: &[Action]) -> Option<(Gen, Arc<Content>)> {
        actions.iter().find_map(|a| match a {
            Action::StartPaste { generation, content } => Some((*generation, content.clone())),
            _ => None,
        })
    }

    fn has(actions: &[Action], f: impl Fn(&Action) -> bool) -> bool {
        actions.iter().any(f)
    }

    fn noted(actions: &[Action], note: Note) -> bool {
        actions.iter().any(|a| matches!(a, Action::Note(n) if *n == note))
    }

    fn complete_paste(m: &mut Machine, generation: Gen) -> Vec<Action> {
        m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Dictation(generation))));
        let a = m.handle(Input::Pasted { generation });
        assert!(has(&a, |a| matches!(a, Action::ScheduleRestore { generation: g, .. } if *g == generation)));
        a
    }

    fn dictate(m: &mut Machine, generation: Gen, text: &str, clip: Option<&str>) {
        let a = m.handle(Input::SnapshotRequested { generation });
        if let Some(clip) = clip {
            let (id, _, _) = read_of(&a).expect("a read of the clipboard");
            m.handle(Input::ReadDone { id, result: Ok(clip.as_bytes().to_vec()) });
        }
        let a = m.handle(Input::PasteRequested { generation, text: text.into() });
        let (g, content) = started_paste(&a).expect("paste started");
        assert_eq!((g, content.data.as_slice()), (generation, text.as_bytes()));
        complete_paste(m, generation);
    }

    #[test]
    fn prefers_plain_text_then_the_first_type() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(choose_mime(&m(&["text/html", "STRING", "text/plain"])), Some("text/plain"));
        assert_eq!(choose_mime(&m(&["text/html", "TEXT", "UTF8_STRING"])), Some("UTF8_STRING"));
        assert_eq!(choose_mime(&m(&["image/png", "image/jpeg"])), Some("image/png"));
        assert_eq!(choose_mime(&m(&[])), None);
    }

    #[test]
    fn text_is_offered_under_all_five_types() {
        assert_eq!(Content::text("x").offer_mimes(), TEXT_MIMES);
        let png = Content { mime: "image/png".into(), data: vec![1] };
        assert_eq!(png.offer_mimes(), ["image/png"]);
    }

    #[test]
    fn restores_when_psst_still_owns_the_selection() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/html", "text/plain"])));
        dictate(&mut m, 1, "dictated", Some("orig"));
        let a = m.handle(Input::RestoreDue { generation: 1 });
        let c = restored(&a).unwrap();
        assert_eq!((c.mime.as_str(), c.data.as_slice()), ("text/plain", &b"orig"[..]));
        assert!(!m.has_pending_restore());
    }

    #[test]
    fn restores_only_after_pasted() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        let a = m.handle(Input::SnapshotRequested { generation: 1 });
        let (id, _, _) = read_of(&a).unwrap();
        m.handle(Input::ReadDone { id, result: Ok(b"orig".to_vec()) });
        m.handle(Input::PasteRequested { generation: 1, text: "d".into() });
        m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Dictation(1))));
        let a = m.handle(Input::RestoreDue { generation: 1 });
        assert!(restored(&a).is_none());
        assert!(m.has_pending_restore());
        m.handle(Input::Pasted { generation: 1 });
        assert!(restored(&m.handle(Input::RestoreDue { generation: 1 })).is_some());
    }

    #[test]
    fn external_change_before_the_paste_resnapshots_and_own_changes_do_not() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        let a = m.handle(Input::SnapshotRequested { generation: 1 });
        let (id, _, _) = read_of(&a).unwrap();
        m.handle(Input::ReadDone { id, result: Ok(b"first".to_vec()) });
        let a = m.handle(Input::SelectionChanged(ext(2, &["text/plain"])));
        let (id2, offer, _) = read_of(&a).expect("re-snapshot");
        assert!(noted(&a, Note::Resnapshot));
        assert_eq!(offer, 2);
        m.handle(Input::ReadDone { id: id2, result: Ok(b"second".to_vec()) });
        let a = m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Restore(0))));
        assert!(read_of(&a).is_none(), "own change must not re-snapshot");
        m.handle(Input::PasteRequested { generation: 1, text: "d".into() });
        complete_paste(&mut m, 1);
        assert_eq!(restored(&m.handle(Input::RestoreDue { generation: 1 })).unwrap().data, b"second");
    }

    #[test]
    fn the_snapshot_freezes_when_the_paste_starts() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        let a = m.handle(Input::SnapshotRequested { generation: 1 });
        let (id, _, _) = read_of(&a).unwrap();
        m.handle(Input::ReadDone { id, result: Ok(b"orig".to_vec()) });
        let a = m.handle(Input::PasteRequested { generation: 1, text: "d".into() });
        // The restore state is set up before the selection changes: StartPaste comes last.
        assert!(matches!(a.last(), Some(Action::StartPaste { .. })));
        assert!(m.has_pending_restore());
        assert!(noted(&a, Note::Frozen { generation: 1, has_snapshot: true }));
        let a = m.handle(Input::SelectionChanged(ext(2, &["text/plain"])));
        assert!(read_of(&a).is_none());
        let a = m.handle(Input::Pasted { generation: 1 });
        assert!(has(&a, |a| matches!(a, Action::ScheduleRestore { .. })));
        let (id, offer, _) = read_of(&m.handle(Input::RestoreDue { generation: 1 })).unwrap();
        assert_eq!(offer, 2);
        let a = m.handle(Input::ReadDone { id, result: Ok(b"user copy".to_vec()) });
        assert!(restored(&a).is_none());
    }

    #[test]
    fn two_quick_dictations_keep_the_original() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["image/png"])));
        dictate(&mut m, 1, "one", Some("PNGDATA"));
        let a = m.handle(Input::SnapshotRequested { generation: 2 });
        assert!(read_of(&a).is_none());
        assert!(noted(&a, Note::SnapshotInherited { from: 1 }));
        m.handle(Input::PasteRequested { generation: 2, text: "two".into() });
        complete_paste(&mut m, 2);
        let a = m.handle(Input::RestoreDue { generation: 1 });
        assert!(restored(&a).is_none());
        assert!(noted(&a, Note::TimerIgnored { generation: 1 }));
        let c = restored(&m.handle(Input::RestoreDue { generation: 2 })).unwrap();
        assert_eq!((c.mime.as_str(), c.data.as_slice()), ("image/png", &b"PNGDATA"[..]));
    }

    #[test]
    fn a_second_snapshot_before_the_first_paste_keeps_the_original() {
        // Toggle during Finishing: stop 2 (its snapshot) comes before the first paste.
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        let a = m.handle(Input::SnapshotRequested { generation: 1 });
        let (id, _, _) = read_of(&a).unwrap();
        m.handle(Input::ReadDone { id, result: Ok(b"orig".to_vec()) });
        let a = m.handle(Input::SnapshotRequested { generation: 2 });
        assert!(read_of(&a).is_none(), "the tracked snapshot is current");
        m.handle(Input::PasteRequested { generation: 1, text: "one".into() });
        complete_paste(&mut m, 1);
        let a = m.handle(Input::PasteRequested { generation: 2, text: "two".into() });
        assert!(noted(&a, Note::SnapshotInherited { from: 1 }));
        complete_paste(&mut m, 2);
        assert_eq!(restored(&m.handle(Input::RestoreDue { generation: 2 })).unwrap().data, b"orig");
    }

    #[test]
    fn inherits_when_a_clipboard_manager_took_over_the_dictation() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        dictate(&mut m, 1, "one", Some("orig"));
        m.handle(Input::SelectionChanged(ext(2, &["text/plain;charset=utf-8"])));
        let a = m.handle(Input::SnapshotRequested { generation: 2 });
        let (id, _, _) = read_of(&a).unwrap();
        let a = m.handle(Input::ReadDone { id, result: Ok(b"one".to_vec()) });
        assert!(noted(&a, Note::SnapshotInherited { from: 1 }));
        m.handle(Input::PasteRequested { generation: 2, text: "two".into() });
        complete_paste(&mut m, 2);
        assert_eq!(restored(&m.handle(Input::RestoreDue { generation: 2 })).unwrap().data, b"orig");
    }

    #[test]
    fn restores_through_a_manager_holding_the_dictated_text() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        dictate(&mut m, 1, "one", Some("orig"));
        m.handle(Input::SelectionChanged(ext(2, &["text/html", "text/plain"])));
        let a = m.handle(Input::RestoreDue { generation: 1 });
        let (id, offer, mime) = read_of(&a).unwrap();
        assert_eq!((offer, mime.as_str()), (2, "text/plain"));
        let a = m.handle(Input::ReadDone { id, result: Ok(b"one".to_vec()) });
        assert_eq!(restored(&a).unwrap().data, b"orig");
    }

    #[test]
    fn the_restore_check_follows_a_selection_change() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        dictate(&mut m, 1, "one", Some("orig"));
        // A manager holds the dictation; its answer is still pending when the user copies.
        m.handle(Input::SelectionChanged(ext(2, &["text/plain"])));
        let (stale, _, _) = read_of(&m.handle(Input::RestoreDue { generation: 1 })).unwrap();
        let a = m.handle(Input::SelectionChanged(ext(3, &["text/plain"])));
        assert!(has(&a, |a| matches!(a, Action::CancelRead { id } if *id == stale)));
        let (fresh, offer, _) = read_of(&a).expect("re-check the new selection");
        assert_eq!(offer, 3);
        assert!(restored(&m.handle(Input::ReadDone { id: stale, result: Ok(b"one".to_vec()) })).is_none());
        let a = m.handle(Input::ReadDone { id: fresh, result: Ok(b"user copy".to_vec()) });
        assert!(restored(&a).is_none());
        assert!(!m.has_pending_restore());
    }

    #[test]
    fn no_restore_after_the_user_copied_something_else() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        dictate(&mut m, 1, "one", Some("orig"));
        m.handle(Input::SelectionChanged(ext(2, &["text/plain"])));
        let (id, _, _) = read_of(&m.handle(Input::RestoreDue { generation: 1 })).unwrap();
        let a = m.handle(Input::ReadDone { id, result: Ok(b"user copy".to_vec()) });
        assert!(restored(&a).is_none());
        assert!(!m.has_pending_restore());
        dictate(&mut m, 2, "two", Some("user copy"));
        m.handle(Input::SelectionChanged(ext(3, &["image/png"])));
        let a = m.handle(Input::RestoreDue { generation: 2 });
        assert!(read_of(&a).is_none() && restored(&a).is_none());
        dictate(&mut m, 3, "three", Some("image"));
        m.handle(Input::SelectionChanged(Owner::Empty));
        assert!(restored(&m.handle(Input::RestoreDue { generation: 3 })).is_none());
    }

    #[test]
    fn a_slow_snapshot_skips_the_restore_after_the_wait() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        let a = m.handle(Input::SnapshotRequested { generation: 1 });
        let (id, _, _) = read_of(&a).unwrap();
        let a = m.handle(Input::PasteRequested { generation: 1, text: "d".into() });
        assert!(has(&a, |a| matches!(a, Action::WaitForSnapshot { max, .. } if *max == Duration::from_millis(100))));
        assert!(started_paste(&a).is_none());
        let a = m.handle(Input::PasteWaitExpired { generation: 1 });
        assert!(has(&a, |a| matches!(a, Action::CancelRead { id: i } if *i == id)));
        assert!(started_paste(&a).is_some());
        m.handle(Input::ReadDone { id, result: Ok(b"late".to_vec()) });
        complete_paste(&mut m, 1);
        let a = m.handle(Input::RestoreDue { generation: 1 });
        assert!(restored(&a).is_none());
    }

    #[test]
    fn a_snapshot_finishing_within_the_wait_starts_the_paste() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        let a = m.handle(Input::SnapshotRequested { generation: 1 });
        let (id, _, _) = read_of(&a).unwrap();
        m.handle(Input::PasteRequested { generation: 1, text: "d".into() });
        let a = m.handle(Input::ReadDone { id, result: Ok(b"orig".to_vec()) });
        assert!(started_paste(&a).is_some());
        assert!(noted(&a, Note::Frozen { generation: 1, has_snapshot: true }));
        assert!(m.handle(Input::PasteWaitExpired { generation: 1 }).is_empty());
    }

    #[test]
    fn a_paste_without_a_snapshot_request_takes_one() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        let a = m.handle(Input::PasteRequested { generation: 1, text: "d".into() });
        let (id, _, _) = read_of(&a).expect("snapshot read");
        let a = m.handle(Input::ReadDone { id, result: Ok(b"orig".to_vec()) });
        assert!(started_paste(&a).is_some());
        complete_paste(&mut m, 1);
        assert_eq!(restored(&m.handle(Input::RestoreDue { generation: 1 })).unwrap().data, b"orig");
    }

    #[test]
    fn an_empty_or_unreadable_clipboard_leaves_the_dictation() {
        for (owner, answer) in [
            (Owner::Empty, None),
            (ext(1, &["text/plain"]), Some(Ok(Vec::new()))),
            (ext(1, &["application/octet-stream"]), Some(Err(ReadError::TooLarge))),
            (ext(1, &["text/plain"]), Some(Err(ReadError::Timeout))),
        ] {
            let mut m = machine();
            m.handle(Input::SelectionChanged(owner));
            let a = m.handle(Input::SnapshotRequested { generation: 1 });
            if let Some(result) = answer {
                let (id, _, _) = read_of(&a).unwrap();
                m.handle(Input::ReadDone { id, result });
            }
            m.handle(Input::PasteRequested { generation: 1, text: "one".into() });
            complete_paste(&mut m, 1);
            let a = m.handle(Input::RestoreDue { generation: 1 });
            assert!(restored(&a).is_none());
            assert_eq!(m.owner(), &Owner::Ours(OwnKind::Dictation(1)));
        }
    }

    #[test]
    fn the_restored_snapshot_is_freed_once_replaced() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["image/png"])));
        dictate(&mut m, 1, "one", Some("PNG"));
        let c = restored(&m.handle(Input::RestoreDue { generation: 1 })).unwrap();
        m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Restore(1))));
        assert_eq!(m.held_bytes(), 3);
        drop(c);
        m.handle(Input::SelectionChanged(ext(2, &["text/plain"])));
        assert_eq!(m.held_bytes(), 0);
        dictate(&mut m, 2, "two", Some("x"));
        m.handle(Input::SelectionChanged(ext(3, &["image/png"])));
        m.handle(Input::RestoreDue { generation: 2 });
        assert_eq!(m.held_bytes(), 0);
    }

    #[test]
    fn copy_is_never_restored_and_cancels_a_pending_restore() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        dictate(&mut m, 1, "one", Some("orig"));
        let a = m.handle(Input::CopyRequested { generation: 2, text: "copied".into() });
        assert!(has(&a, |a| matches!(a, Action::StartCopy { generation: 2, content } if content.data == b"copied")));
        assert!(!m.has_pending_restore());
        m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Copy(2))));
        let a = m.handle(Input::Copied { generation: 2 });
        assert!(!has(&a, |a| matches!(a, Action::ScheduleRestore { .. })));
        assert!(restored(&m.handle(Input::RestoreDue { generation: 1 })).is_none());
        let a = m.handle(Input::SnapshotRequested { generation: 3 });
        assert!(read_of(&a).is_none());
        m.handle(Input::PasteRequested { generation: 3, text: "three".into() });
        complete_paste(&mut m, 3);
        assert_eq!(restored(&m.handle(Input::RestoreDue { generation: 3 })).unwrap().data, b"copied");
    }

    #[test]
    fn pastes_and_copies_run_one_at_a_time_in_order() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        let a = m.handle(Input::SnapshotRequested { generation: 1 });
        let (id, _, _) = read_of(&a).unwrap();
        let a = m.handle(Input::PasteRequested { generation: 1, text: "one".into() });
        assert!(started_paste(&a).is_none());
        assert!(m.handle(Input::CopyRequested { generation: 2, text: "copied".into() }).is_empty());
        assert!(m.handle(Input::PasteRequested { generation: 3, text: "three".into() }).is_empty());
        assert!(m.has_op(2) && m.has_op(3));
        let a = m.handle(Input::ReadDone { id, result: Ok(b"orig".to_vec()) });
        assert_eq!(started_paste(&a).unwrap().0, 1);
        assert!(!has(&a, |a| matches!(a, Action::StartCopy { .. })));
        m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Dictation(1))));
        let a = m.handle(Input::Pasted { generation: 1 });
        assert!(has(&a, |a| matches!(a, Action::StartCopy { generation: 2, .. })));
        let a = m.handle(Input::Copied { generation: 2 });
        let (g, content) = started_paste(&a).unwrap();
        assert_eq!((g, content.data.as_slice()), (3, &b"three"[..]));
        complete_paste(&mut m, 3);
        // The copy came before paste 3, so the copy is what comes back.
        assert_eq!(restored(&m.handle(Input::RestoreDue { generation: 3 })).unwrap().data, b"copied");
        assert!(!m.has_op(3));
    }

    #[test]
    fn a_failed_paste_keeps_the_dictation_and_frees_the_queue() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        let a = m.handle(Input::SnapshotRequested { generation: 1 });
        let (id, _, _) = read_of(&a).unwrap();
        m.handle(Input::ReadDone { id, result: Ok(b"orig".to_vec()) });
        m.handle(Input::PasteRequested { generation: 1, text: "one".into() });
        m.handle(Input::PasteRequested { generation: 2, text: "two".into() });
        let a = m.handle(Input::Failed { generation: 1 });
        assert!(noted(&a, Note::RestoreSkipped { generation: 1, reason: "the paste failed".into() }));
        assert_eq!(started_paste(&a).unwrap().0, 2);
        assert!(m.handle(Input::Pasted { generation: 1 }).is_empty());
        m.handle(Input::PasteRequested { generation: 3, text: "three".into() });
        m.handle(Input::Failed { generation: 3 });
        assert!(!m.has_op(3));
    }

    #[test]
    fn an_unused_snapshot_expires() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["image/png"])));
        let a = m.handle(Input::SnapshotRequested { generation: 1 });
        let epoch1 = a.iter().find_map(|a| match a {
            Action::ExpireSnapshot { epoch, after } => {
                assert_eq!(*after, Duration::from_secs(120));
                Some(*epoch)
            }
            _ => None,
        });
        let (id, _, _) = read_of(&a).unwrap();
        m.handle(Input::ReadDone { id, result: Ok(vec![0; 1000]) });
        assert_eq!(m.held_bytes(), 1000);
        let a = m.handle(Input::SnapshotRequested { generation: 2 });
        let epoch2 = a.iter().find_map(|a| match a {
            Action::ExpireSnapshot { epoch, .. } => Some(*epoch),
            _ => None,
        });
        assert!(m.handle(Input::SnapshotExpired { epoch: epoch1.unwrap() }).is_empty());
        let a = m.handle(Input::SnapshotExpired { epoch: epoch2.unwrap() });
        assert!(noted(&a, Note::SnapshotExpired));
        assert_eq!(m.held_bytes(), 0);
        assert!(read_of(&m.handle(Input::SelectionChanged(ext(2, &["text/plain"])))).is_none());
    }

    #[test]
    fn a_restore_during_a_snapshot_read_updates_the_snapshot() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        dictate(&mut m, 1, "one", Some("orig"));
        // A manager took over the dictation; the second snapshot reads it while the first
        // restore checks the same selection.
        m.handle(Input::SelectionChanged(ext(2, &["text/plain"])));
        let (snap_read, _, _) = read_of(&m.handle(Input::SnapshotRequested { generation: 2 })).unwrap();
        let (check, _, _) = read_of(&m.handle(Input::RestoreDue { generation: 1 })).unwrap();
        let a = m.handle(Input::ReadDone { id: check, result: Ok(b"one".to_vec()) });
        assert!(restored(&a).is_some());
        assert!(has(&a, |a| matches!(a, Action::CancelRead { id } if *id == snap_read)));
        m.handle(Input::SelectionChanged(Owner::Ours(OwnKind::Restore(1))));
        m.handle(Input::ReadDone { id: snap_read, result: Ok(b"one".to_vec()) });
        m.handle(Input::PasteRequested { generation: 2, text: "two".into() });
        complete_paste(&mut m, 2);
        assert_eq!(restored(&m.handle(Input::RestoreDue { generation: 2 })).unwrap().data, b"orig");
    }

    #[test]
    fn a_restore_completes_a_paste_waiting_for_its_snapshot() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        dictate(&mut m, 1, "one", Some("orig"));
        m.handle(Input::SelectionChanged(ext(2, &["text/plain"])));
        let a = m.handle(Input::PasteRequested { generation: 2, text: "two".into() });
        assert!(has(&a, |a| matches!(a, Action::WaitForSnapshot { .. })));
        let (check, _, _) = read_of(&m.handle(Input::RestoreDue { generation: 1 })).unwrap();
        let a = m.handle(Input::ReadDone { id: check, result: Ok(b"one".to_vec()) });
        assert!(restored(&a).is_some());
        assert_eq!(started_paste(&a).unwrap().0, 2);
        complete_paste(&mut m, 2);
        assert_eq!(restored(&m.handle(Input::RestoreDue { generation: 2 })).unwrap().data, b"orig");
    }

    #[test]
    fn a_snapshot_while_a_paste_is_in_flight_inherits() {
        // The compositor hasn't announced psst's selection yet; the machine already knows.
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        let (id, _, _) = read_of(&m.handle(Input::SnapshotRequested { generation: 1 })).unwrap();
        m.handle(Input::ReadDone { id, result: Ok(b"orig".to_vec()) });
        m.handle(Input::PasteRequested { generation: 1, text: "one".into() });
        let a = m.handle(Input::SnapshotRequested { generation: 2 });
        assert!(noted(&a, Note::SnapshotInherited { from: 1 }));
        assert!(read_of(&a).is_none());
    }

    #[test]
    fn a_timer_from_before_a_reset_leaves_a_new_snapshot_alone() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["image/png"])));
        let a = m.handle(Input::SnapshotRequested { generation: 1 });
        let stale = a.iter().find_map(|a| match a {
            Action::ExpireSnapshot { epoch, .. } => Some(*epoch),
            _ => None,
        });
        m.handle(Input::Reset);
        m.handle(Input::SelectionChanged(ext(1, &["image/png"])));
        let (id, _, _) = read_of(&m.handle(Input::SnapshotRequested { generation: 2 })).unwrap();
        m.handle(Input::ReadDone { id, result: Ok(vec![0; 10]) });
        assert!(m.handle(Input::SnapshotExpired { epoch: stale.unwrap() }).is_empty());
        assert_eq!(m.held_bytes(), 10);
    }

    #[test]
    fn reset_forgets_everything() {
        let mut m = machine();
        m.handle(Input::SelectionChanged(ext(1, &["text/plain"])));
        dictate(&mut m, 1, "one", Some("orig"));
        m.handle(Input::PasteRequested { generation: 2, text: "two".into() });
        m.handle(Input::Reset);
        assert!(!m.has_pending_restore() && !m.has_op(2));
        assert_eq!(m.held_bytes(), 0);
        assert_eq!(m.owner(), &Owner::Unknown);
        assert!(restored(&m.handle(Input::RestoreDue { generation: 1 })).is_none());
    }
}
