//! The Wayland thread: one connection, one calloop loop, serving the
//! clipboard flow and the virtual keyboard (plan 5.4 / 9.2 / 9.5).
//!
//! Paste order on the one connection: data-control `set_selection` →
//! `wl_display.sync` → optional delay → virtual-keyboard chord →
//! `wl_display.sync` → `Pasted`. Clipboard pipes are non-blocking calloop
//! sources; snapshot reads are capped and time out.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{
    EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken, channel,
};
use calloop_wayland_source::WaylandSource;
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;
use rustix::pipe::{PipeFlags, pipe_with};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_callback::WlCallback, wl_registry::WlRegistry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

use crate::clipboard::{
    self, Action, Gen, Input, Machine, Note, OfferKey, OwnKind, Owner, ReadError, ReadId,
};
use crate::clock::{now_ms_u32, now_us};
use crate::dc::{Bytes, DcDevice, DcHandler, DcManager, DcOffer, DcSource, Proto};
use crate::keymap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chord {
    CtrlV,
    CtrlShiftV,
}

#[derive(Debug)]
pub enum Cmd {
    Snapshot {
        generation: Gen,
    },
    /// `t_ready_us`: when the caller had the text (CLOCK_MONOTONIC µs).
    Paste {
        generation: Gen,
        text: String,
        chord: Chord,
        delay_ms: u32,
        t_ready_us: u64,
    },
    Copy {
        generation: Gen,
        text: String,
    },
    /// Test probe: send a chord without touching the clipboard.
    ChordOnly {
        chord: Chord,
    },
    Shutdown,
}

/// CLOCK_MONOTONIC µs marks for one paste.
#[derive(Clone, Debug, Default)]
pub struct PasteTiming {
    pub ready: u64,
    pub cmd: u64,
    pub set_selection: u64,
    pub sync1: u64,
    pub chord: u64,
    pub sync2: u64,
}

#[derive(Debug)]
pub enum Evt {
    Pasted {
        generation: Gen,
        timing: PasteTiming,
    },
    Copied {
        generation: Gen,
    },
    ChordSent,
    Selection(Owner),
    Note(Note),
}

#[derive(Clone, Debug)]
pub struct Options {
    pub proto: Option<Proto>,
    pub machine: clipboard::Config,
    pub read_cap: usize,
    pub read_timeout: Duration,
    /// Use the self-contained keymap instead of the wtype-style one.
    pub minimal_keymap: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            proto: None,
            machine: clipboard::Config::default(),
            read_cap: 10 * 1024 * 1024,
            read_timeout: Duration::from_secs(2),
            minimal_keymap: false,
        }
    }
}

pub struct Handle {
    pub tx: channel::Sender<Cmd>,
    pub rx: mpsc::Receiver<Evt>,
    pub proto: Proto,
    join: Option<JoinHandle<()>>,
}

impl Handle {
    pub fn send(&self, cmd: Cmd) {
        self.tx.send(cmd).expect("wayland thread gone");
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Starts the Wayland thread; returns once it is connected and has seen the
/// initial selection.
pub fn spawn(opts: Options) -> Result<Handle, String> {
    crate::guard::assert_sandbox();
    let (tx, chan) = channel::channel::<Cmd>();
    let (etx, erx) = mpsc::channel();
    let (itx, irx) = mpsc::channel::<Result<Proto, String>>();
    let join = std::thread::Builder::new()
        .name("wayland".into())
        .spawn(move || {
            if let Err(e) = run(opts, chan, etx, itx.clone()) {
                let _ = itx.send(Err(e));
            }
        })
        .map_err(|e| e.to_string())?;
    let proto = irx
        .recv()
        .map_err(|_| "wayland thread died".to_string())??;
    Ok(Handle {
        tx,
        rx: erx,
        proto,
        join: Some(join),
    })
}

#[derive(Debug)]
enum SyncTag {
    Init,
    PasteSet(Gen),
    PasteChord(Gen),
    CopySet(Gen),
    Probe,
}

struct Job {
    text: String,
    chord: Chord,
    delay_ms: u32,
    timing: PasteTiming,
}

struct State {
    conn: Connection,
    qh: QueueHandle<State>,
    lh: LoopHandle<'static, State>,
    signal: LoopSignal,
    opts: Options,
    dc: DcManager,
    device: DcDevice,
    vk: ZwpVirtualKeyboardV1,
    machine: Machine,
    evt: mpsc::Sender<Evt>,
    offers: HashMap<OfferKey, DcOffer>,
    current_offer: Option<OfferKey>,
    next_offer: OfferKey,
    /// The source psst set last, and what it is.
    latest: Option<(DcSource, OwnKind)>,
    reads: HashMap<ReadId, (RegistrationToken, RegistrationToken)>,
    jobs: HashMap<Gen, Job>,
    initialized: bool,
}

fn run(
    opts: Options,
    chan: channel::Channel<Cmd>,
    evt: mpsc::Sender<Evt>,
    init: mpsc::Sender<Result<Proto, String>>,
) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("connect: {e}"))?;
    let (globals, queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();
    let seat: WlSeat = globals
        .bind(&qh, 1..=8, ())
        .map_err(|e| format!("wl_seat: {e}"))?;
    let dc = DcManager::bind(&globals, &qh, opts.proto)?;
    let device = dc.get_device(&seat, &qh);
    let vkm: ZwpVirtualKeyboardManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("zwp_virtual_keyboard_manager_v1: {e}"))?;
    let vk = vkm.create_virtual_keyboard(&seat, &qh, ());
    let km = if opts.minimal_keymap {
        keymap::PASTE_KEYMAP_MINIMAL
    } else {
        keymap::PASTE_KEYMAP
    };
    keymap::upload(&vk, km).map_err(|e| format!("keymap: {e}"))?;

    let mut el: EventLoop<'static, State> = EventLoop::try_new().map_err(|e| e.to_string())?;
    WaylandSource::new(conn.clone(), queue)
        .insert(el.handle())
        .map_err(|e| e.to_string())?;
    el.handle()
        .insert_source(chan, |ev, _, st: &mut State| match ev {
            channel::Event::Msg(cmd) => st.on_cmd(cmd),
            channel::Event::Closed => st.signal.stop(),
        })
        .map_err(|e| e.to_string())?;

    let proto = dc.proto();
    let mut st = State {
        conn: conn.clone(),
        qh: qh.clone(),
        lh: el.handle(),
        signal: el.get_signal(),
        machine: Machine::new(opts.machine),
        opts,
        dc,
        device,
        vk,
        evt,
        offers: HashMap::new(),
        current_offer: None,
        next_offer: 1,
        latest: None,
        reads: HashMap::new(),
        jobs: HashMap::new(),
        initialized: false,
    };
    conn.display().sync(&qh, SyncTag::Init);
    conn.flush().map_err(|e| e.to_string())?;
    while !st.initialized {
        el.dispatch(Duration::from_secs(2), &mut st)
            .map_err(|e| e.to_string())?;
    }
    let _ = init.send(Ok(proto));
    el.run(None, &mut st, |_| {}).map_err(|e| e.to_string())
}

impl State {
    fn emit(&self, e: Evt) {
        let _ = self.evt.send(e);
    }

    fn flush(&self) {
        if let Err(e) = self.conn.flush() {
            eprintln!("wayland flush: {e}");
        }
    }

    fn feed(&mut self, input: Input) {
        for a in self.machine.handle(input) {
            self.exec(a);
        }
    }

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Snapshot { generation } => self.feed(Input::SnapshotRequested { generation }),
            Cmd::Paste {
                generation,
                text,
                chord,
                delay_ms,
                t_ready_us,
            } => {
                let timing = PasteTiming {
                    ready: t_ready_us,
                    cmd: now_us(),
                    ..Default::default()
                };
                self.jobs.insert(
                    generation,
                    Job {
                        text: text.clone(),
                        chord,
                        delay_ms,
                        timing,
                    },
                );
                self.feed(Input::PasteRequested { generation, text });
            }
            Cmd::Copy { generation, text } => {
                let timing = PasteTiming {
                    cmd: now_us(),
                    ..Default::default()
                };
                self.jobs.insert(
                    generation,
                    Job {
                        text: text.clone(),
                        chord: Chord::CtrlV,
                        delay_ms: 0,
                        timing,
                    },
                );
                self.feed(Input::CopyRequested { generation, text });
            }
            Cmd::ChordOnly { chord } => {
                self.send_chord(chord);
                self.conn.display().sync(&self.qh, SyncTag::Probe);
                self.flush();
            }
            Cmd::Shutdown => self.signal.stop(),
        }
    }

    fn after<F>(&self, d: Duration, f: F) -> RegistrationToken
    where
        F: FnOnce(&mut State) + 'static,
    {
        let mut f = Some(f);
        self.lh
            .insert_source(Timer::from_duration(d), move |_, _, st| {
                if let Some(f) = f.take() {
                    f(st);
                }
                TimeoutAction::Drop
            })
            .expect("timer")
    }

    fn exec(&mut self, a: Action) {
        match a {
            Action::Read { id, offer, mime } => self.start_read(id, offer, mime),
            Action::CancelRead { id } => {
                if let Some((fd_tok, timer_tok)) = self.reads.remove(&id) {
                    self.lh.remove(fd_tok);
                    self.lh.remove(timer_tok);
                }
            }
            Action::WaitForSnapshot { generation, max } => {
                self.after(max, move |st| {
                    st.feed(Input::PasteWaitExpired { generation })
                });
            }
            Action::StartPaste { generation } => {
                let text = self
                    .jobs
                    .get(&generation)
                    .map(|j| j.text.clone())
                    .unwrap_or_default();
                self.set_own_selection(
                    Arc::new(clipboard::Snapshot::text(&text)),
                    OwnKind::Dictation(generation),
                );
                if let Some(j) = self.jobs.get_mut(&generation) {
                    j.timing.set_selection = now_us();
                }
                self.conn
                    .display()
                    .sync(&self.qh, SyncTag::PasteSet(generation));
                self.flush();
            }
            Action::StartCopy { generation } => {
                let text = self
                    .jobs
                    .get(&generation)
                    .map(|j| j.text.clone())
                    .unwrap_or_default();
                self.set_own_selection(
                    Arc::new(clipboard::Snapshot::text(&text)),
                    OwnKind::Copy(generation),
                );
                self.conn
                    .display()
                    .sync(&self.qh, SyncTag::CopySet(generation));
                self.flush();
            }
            Action::ScheduleRestore { generation, after } => {
                self.after(after, move |st| st.feed(Input::RestoreDue { generation }));
            }
            Action::OfferRestore {
                generation,
                snapshot,
            } => {
                self.set_own_selection(snapshot, OwnKind::Restore(generation));
                self.flush();
            }
            Action::Note(n) => self.emit(Evt::Note(n)),
        }
    }

    fn set_own_selection(&mut self, content: Arc<clipboard::Snapshot>, kind: OwnKind) {
        let items: Vec<(String, Bytes)> = content
            .offer_mimes()
            .into_iter()
            .map(|m| (m, content.clone() as Bytes))
            .collect();
        let src = self.dc.create_source(&self.qh, items);
        self.device.set_selection(Some(&src));
        self.latest = Some((src, kind));
    }

    fn send_chord(&self, chord: Chord) {
        let mods = match chord {
            Chord::CtrlV => keymap::MOD_CTRL,
            Chord::CtrlShiftV => keymap::MOD_CTRL | keymap::MOD_SHIFT,
        };
        // As wtype -M ctrl -k v -m ctrl: modifiers, press, release, clear.
        let t = now_ms_u32();
        self.vk.modifiers(mods, 0, 0, 0);
        self.vk.key(t, keymap::KEY_V, 1);
        self.vk.key(t, keymap::KEY_V, 0);
        self.vk.modifiers(0, 0, 0, 0);
    }

    fn on_sync(&mut self, tag: &SyncTag) {
        match *tag {
            SyncTag::Init => self.initialized = true,
            SyncTag::PasteSet(generation) => {
                let Some(job) = self.jobs.get_mut(&generation) else {
                    return;
                };
                job.timing.sync1 = now_us();
                let delay = job.delay_ms;
                if delay > 0 {
                    self.after(Duration::from_millis(delay as u64), move |st| {
                        st.chord_for(generation)
                    });
                } else {
                    self.chord_for(generation);
                }
            }
            SyncTag::PasteChord(generation) => {
                if let Some(mut job) = self.jobs.remove(&generation) {
                    job.timing.sync2 = now_us();
                    self.emit(Evt::Pasted {
                        generation,
                        timing: job.timing,
                    });
                    self.feed(Input::Pasted { generation });
                }
            }
            SyncTag::CopySet(generation) => {
                self.jobs.remove(&generation);
                self.emit(Evt::Copied { generation });
            }
            SyncTag::Probe => self.emit(Evt::ChordSent),
        }
    }

    fn chord_for(&mut self, generation: Gen) {
        let Some(job) = self.jobs.get_mut(&generation) else {
            return;
        };
        let chord = job.chord;
        job.timing.chord = now_us();
        self.send_chord(chord);
        self.conn
            .display()
            .sync(&self.qh, SyncTag::PasteChord(generation));
        self.flush();
    }

    fn start_read(&mut self, id: ReadId, offer: OfferKey, mime: String) {
        let Some(o) = self.offers.get(&offer).cloned() else {
            self.feed(Input::ReadDone {
                id,
                result: Err(ReadError::Io("offer is gone".into())),
            });
            return;
        };
        let (r, w) = match pipe_with(PipeFlags::CLOEXEC) {
            Ok(p) => p,
            Err(e) => {
                self.feed(Input::ReadDone {
                    id,
                    result: Err(ReadError::Io(e.to_string())),
                });
                return;
            }
        };
        set_nonblocking(&r);
        o.receive(mime, w.as_fd());
        drop(w);
        self.flush();
        let cap = self.opts.read_cap;
        let mut buf = Vec::new();
        let fd_tok = self
            .lh
            .insert_source(
                Generic::new(r, Interest::READ, Mode::Level),
                move |_, fd, st: &mut State| {
                    let mut chunk = [0u8; 65536];
                    loop {
                        match rustix::io::read(&**fd, &mut chunk) {
                            Ok(0) => {
                                let data = std::mem::take(&mut buf);
                                st.finish_read(id, Ok(data));
                                return Ok(PostAction::Remove);
                            }
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if buf.len() > cap {
                                    st.finish_read(id, Err(ReadError::TooLarge));
                                    return Ok(PostAction::Remove);
                                }
                            }
                            Err(Errno::AGAIN) => return Ok(PostAction::Continue),
                            Err(Errno::INTR) => continue,
                            Err(e) => {
                                st.finish_read(id, Err(ReadError::Io(e.to_string())));
                                return Ok(PostAction::Remove);
                            }
                        }
                    }
                },
            )
            .expect("read source");
        let timer_tok = self.after(self.opts.read_timeout, move |st| {
            if let Some((fd_tok, _)) = st.reads.remove(&id) {
                st.lh.remove(fd_tok);
                st.feed(Input::ReadDone {
                    id,
                    result: Err(ReadError::Timeout),
                });
            }
        });
        self.reads.insert(id, (fd_tok, timer_tok));
    }

    fn finish_read(&mut self, id: ReadId, result: Result<Vec<u8>, ReadError>) {
        if let Some((_, timer_tok)) = self.reads.remove(&id) {
            self.lh.remove(timer_tok);
            self.feed(Input::ReadDone { id, result });
        }
    }

    fn start_write(&mut self, fd: OwnedFd, data: Bytes) {
        set_nonblocking(&fd);
        let mut off = 0usize;
        self.lh
            .insert_source(
                Generic::new(fd, Interest::WRITE, Mode::Level),
                move |_, fd, _st: &mut State| {
                    let bytes = (*data).as_ref();
                    while off < bytes.len() {
                        match rustix::io::write(&**fd, &bytes[off..]) {
                            Ok(n) => off += n,
                            Err(Errno::AGAIN) => return Ok(PostAction::Continue),
                            Err(Errno::INTR) => continue,
                            Err(_) => return Ok(PostAction::Remove),
                        }
                    }
                    Ok(PostAction::Remove)
                },
            )
            .expect("write source");
    }
}

fn set_nonblocking(fd: &OwnedFd) {
    if let Ok(fl) = fcntl_getfl(fd) {
        let _ = fcntl_setfl(fd, fl | OFlags::NONBLOCK);
    }
}

impl DcHandler for State {
    fn dc_selection(&mut self, _: &Connection, offer: Option<DcOffer>) {
        // psst's own selection: our latest source is still alive (an external
        // set_selection cancels it before the new selection is announced).
        let ours = self
            .latest
            .as_ref()
            .filter(|(s, _)| s.is_live())
            .map(|(_, k)| k.clone());
        if let Some(old) = self.current_offer.take()
            && let Some(o) = self.offers.remove(&old)
        {
            o.destroy();
        }
        let owner = match (ours, offer) {
            (Some(kind), offer) => {
                if let Some(o) = offer {
                    o.destroy();
                }
                Owner::Ours(kind)
            }
            (None, Some(o)) => {
                let key = self.next_offer;
                self.next_offer += 1;
                let mimes = o.mimes();
                self.offers.insert(key, o);
                self.current_offer = Some(key);
                Owner::External { offer: key, mimes }
            }
            (None, None) => Owner::Empty,
        };
        self.emit(Evt::Selection(owner.clone()));
        self.feed(Input::SelectionChanged(owner));
    }

    fn dc_send(&mut self, source: &DcSource, mime: String, fd: OwnedFd) {
        match source.data().bytes_for(&mime) {
            Some(b) => self.start_write(fd, b),
            None => drop(fd),
        }
    }

    fn dc_cancelled(&mut self, source: &DcSource) {
        source.destroy();
    }
}

crate::delegate_dc!(State);

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCallback, SyncTag> for State {
    fn event(
        st: &mut Self,
        _: &WlCallback,
        _: <WlCallback as wayland_client::Proxy>::Event,
        tag: &SyncTag,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        st.on_sync(tag);
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
