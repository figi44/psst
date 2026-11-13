//! The Wayland thread: one calloop loop, one connection (made lazily, remade after a loss), the
//! clipboard glue around [`Machine`], and the paste sequence.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken, channel};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;
use rustix::pipe::{PipeFlags, pipe_with};
use wayland_client::backend::WaylandError;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_callback::WlCallback, wl_registry::WlRegistry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

use crate::clipboard::{self, Action, Content, Gen, Input, Machine, OfferKey, OwnKind, Owner, ReadError, ReadId};
use crate::dc::{Bytes, DcDevice, DcHandler, DcManager, DcOffer, DcSource, Proto, delegate_dc};
use crate::overlay::{Globals as SurfaceGlobals, Overlay};
use crate::{Chord, DataControl, Options, WaylandCmd, WaylandEvent, keymap};

/// A snapshot keeps at most this much (plan 9.5).
const READ_CAP: usize = 10 * 1024 * 1024;
/// A snapshot read gives up after this long.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Serving our selection to a reader that never reads gives up after this long.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// A paste or copy the compositor never confirms fails after this long (plus its delay).
const JOB_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) enum Msg {
    Cmd(WaylandCmd),
    Shutdown,
}

pub(crate) type OnEvent = Box<dyn FnMut(WaylandEvent) + Send>;

pub(crate) fn run(options: Options, rx: channel::Channel<Msg>, on_event: OnEvent) {
    let mut event_loop: EventLoop<'static, State> = match EventLoop::try_new() {
        Ok(el) => el,
        Err(err) => {
            log::error!("psst-wayland: cannot create the event loop: {err}");
            return;
        }
    };
    let inserted = event_loop.handle().insert_source(rx, |event, _, st: &mut State| match event {
        channel::Event::Msg(Msg::Cmd(cmd)) => st.on_cmd(cmd),
        channel::Event::Msg(Msg::Shutdown) | channel::Event::Closed => st.signal.stop(),
    });
    if let Err(err) = inserted {
        log::error!("psst-wayland: cannot listen for commands: {err}");
        return;
    }
    let mut st = State::new(options, on_event, event_loop.handle(), event_loop.get_signal());
    if let Err(err) = event_loop.run(None, &mut st, State::after_dispatch) {
        log::error!("psst-wayland: event loop failed: {err}");
    }
    st.overlay_hide();
    st.flush();
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SyncTag {
    PasteSet(Gen),
    PasteChord(Gen),
    CopySet(Gen),
}

enum JobKind {
    Paste { chord: Chord, delay_ms: u32 },
    Copy,
}

/// A paste or copy accepted from the daemon and not finished yet.
struct Job {
    kind: JobKind,
    /// Armed when the operation starts, not while it waits its turn.
    watchdog: Option<RegistrationToken>,
}

/// An in-flight pipe transfer: the fd source and its timeout.
struct Io {
    source: RegistrationToken,
    timer: RegistrationToken,
}

/// Lets calloop poll the connection's socket.
struct ConnFd(wayland_client::backend::Backend);

impl AsFd for ConnFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.poll_fd()
    }
}

/// Everything that lives as long as one connection.
pub(crate) struct Wl {
    pub(crate) conn: Connection,
    /// `None` only while its events are being dispatched.
    queue: Option<EventQueue<State>>,
    pub(crate) qh: QueueHandle<State>,
    fd_source: RegistrationToken,
    pub(crate) surfaces: SurfaceGlobals,
    dc: Option<(DcManager, DcDevice)>,
    vk: Option<ZwpVirtualKeyboardV1>,
    offers: HashMap<OfferKey, DcOffer>,
    current_offer: Option<OfferKey>,
    next_offer: OfferKey,
    /// The source psst set last, and what it serves.
    latest: Option<(DcSource, OwnKind)>,
}

pub(crate) struct State {
    pub(crate) opts: Options,
    on_event: OnEvent,
    pub(crate) lh: LoopHandle<'static, State>,
    signal: LoopSignal,
    machine: Machine,
    pub(crate) wl: Option<Wl>,
    jobs: HashMap<Gen, Job>,
    reads: HashMap<ReadId, Io>,
    writes: HashMap<u64, Io>,
    next_write: u64,
    pub(crate) overlay: Overlay,
    /// Set by event handlers; the connection is dropped once dispatching is done.
    pub(crate) broken: Option<String>,
    flush_retry: Option<RegistrationToken>,
    connect_failed: bool,
}

impl State {
    fn new(opts: Options, on_event: OnEvent, lh: LoopHandle<'static, State>, signal: LoopSignal) -> Self {
        let machine = Machine::new(clipboard::Config { restore_after: opts.restore_after, ..Default::default() });
        State {
            opts,
            on_event,
            lh,
            signal,
            machine,
            wl: None,
            jobs: HashMap::new(),
            reads: HashMap::new(),
            writes: HashMap::new(),
            next_write: 1,
            overlay: Overlay::new(),
            broken: None,
            flush_retry: None,
            connect_failed: false,
        }
    }

    pub(crate) fn emit(&mut self, event: WaylandEvent) {
        (self.on_event)(event);
    }

    /// Runs `f` once after `delay`.
    pub(crate) fn after(&self, delay: Duration, f: impl FnOnce(&mut State) + 'static) -> RegistrationToken {
        let mut f = Some(f);
        self.lh
            .insert_source(Timer::from_duration(delay), move |_, _, st| {
                if let Some(f) = f.take() {
                    f(st);
                }
                TimeoutAction::Drop
            })
            .expect("calloop accepts timers")
    }

    // ------------------------------------------------------------------ connection

    /// Connects if there's no connection.
    pub(crate) fn ensure_connected(&mut self) -> Result<(), String> {
        if self.wl.is_some() {
            return Ok(());
        }
        match self.connect() {
            Ok(()) => {
                self.connect_failed = false;
                Ok(())
            }
            Err(err) => {
                if !self.connect_failed {
                    log::warn!("psst-wayland: {err}");
                    self.connect_failed = true;
                }
                Err(err)
            }
        }
    }

    fn connect(&mut self) -> Result<(), String> {
        let conn = match &self.opts.display {
            Some(path) => {
                let stream = UnixStream::connect(path)
                    .map_err(|e| format!("cannot connect to the Wayland socket {}: {e}", path.display()))?;
                Connection::from_socket(stream).map_err(|e| format!("cannot connect to Wayland: {e}"))?
            }
            None => Connection::connect_to_env().map_err(|e| format!("cannot connect to Wayland: {e}"))?,
        };
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| format!("cannot read the Wayland globals: {e}"))?;
        let qh = queue.handle();
        let seat: Option<WlSeat> = globals.bind(&qh, 1..=8, ()).ok();
        let dc = seat.as_ref().and_then(|seat| {
            let want = match self.opts.data_control {
                DataControl::Auto => None,
                DataControl::Ext => Some(Proto::Ext),
                DataControl::Wlr => Some(Proto::Wlr),
            };
            match DcManager::bind(&globals, &qh, want) {
                Ok(manager) => {
                    log::debug!("psst-wayland: data control: {:?}", manager.proto());
                    let device = manager.get_device(seat, &qh);
                    Some((manager, device))
                }
                Err(err) => {
                    log::warn!("psst-wayland: {err}; pasting and copying won't work");
                    None
                }
            }
        });
        let vk = seat.as_ref().and_then(|seat| {
            let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).ok()?;
            let vk = manager.create_virtual_keyboard(seat, &qh, ());
            match keymap::upload(&vk) {
                Ok(()) => Some(vk),
                Err(err) => {
                    log::warn!("psst-wayland: cannot upload the paste keymap: {err}");
                    vk.destroy();
                    None
                }
            }
        });
        let surfaces = SurfaceGlobals::bind(&globals, &qh);
        let fd_source = self
            .lh
            .insert_source(Generic::new(ConnFd(conn.backend()), Interest::READ, Mode::Level), |_, _, st| {
                st.on_readable();
                Ok(PostAction::Continue)
            })
            .map_err(|e| format!("cannot watch the Wayland socket: {e}"))?;
        self.wl = Some(Wl {
            conn,
            queue: None,
            qh,
            fd_source,
            surfaces,
            dc,
            vk,
            offers: HashMap::new(),
            current_offer: None,
            next_offer: 1,
            latest: None,
        });
        // The initial selection arrives before any command runs.
        let result = queue.roundtrip(self);
        if let Some(wl) = self.wl.as_mut() {
            wl.queue = Some(queue);
        }
        if let Some(err) = result.err().map(|e| e.to_string()).or_else(|| self.broken.take()) {
            if let Some(wl) = self.wl.take() {
                self.lh.remove(wl.fd_source);
            }
            self.machine.handle(Input::Reset);
            return Err(format!("the Wayland connection failed while starting: {err}"));
        }
        log::debug!("psst-wayland: connected");
        Ok(())
    }

    fn on_readable(&mut self) {
        let Some(wl) = self.wl.as_mut() else { return };
        let conn = wl.conn.clone();
        let Some(mut queue) = wl.queue.take() else { return };
        let mut result = Ok(());
        if let Some(guard) = conn.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => result = Err(e.to_string()),
            }
        }
        if result.is_ok()
            && let Err(e) = queue.dispatch_pending(self)
        {
            result = Err(e.to_string());
        }
        if let Some(wl) = self.wl.as_mut() {
            wl.queue = Some(queue);
        }
        if let Err(err) = result {
            self.broken.get_or_insert(err);
        }
    }

    /// After every round of the event loop: drop a broken connection, or handle anything still
    /// queued and send what this round produced.
    fn after_dispatch(&mut self) {
        if let Some(wl) = self.wl.as_mut()
            && let Some(mut queue) = wl.queue.take()
        {
            let result = queue.dispatch_pending(self);
            if let Some(wl) = self.wl.as_mut() {
                wl.queue = Some(queue);
            }
            if let Err(err) = result {
                self.broken.get_or_insert(err.to_string());
            }
        }
        if let Some(reason) = self.broken.take() {
            self.disconnect(&reason);
            return;
        }
        self.flush();
    }

    pub(crate) fn flush(&mut self) {
        let Some(wl) = &self.wl else { return };
        match wl.conn.flush() {
            Ok(()) => {}
            Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                // The socket is full: try again shortly.
                if self.flush_retry.is_none() {
                    let token = self.after(Duration::from_millis(2), |st| st.flush_retry = None);
                    self.flush_retry = Some(token);
                }
            }
            Err(e) => self.disconnect(&e.to_string()),
        }
    }

    /// Forgets the connection and everything tied to it, fails what was in progress, and
    /// reports `Disconnected`. The next command connects again.
    fn disconnect(&mut self, reason: &str) {
        let Some(wl) = self.wl.take() else { return };
        log::warn!("psst-wayland: lost the Wayland connection: {reason}");
        self.lh.remove(wl.fd_source);
        for (_, io) in self.reads.drain().chain(self.writes.drain()) {
            self.lh.remove(io.source);
            self.lh.remove(io.timer);
        }
        self.machine.handle(Input::Reset);
        self.overlay.forget_surface();
        let mut jobs: Vec<_> = self.jobs.drain().collect();
        jobs.sort_by_key(|(generation, _)| *generation);
        for (generation, job) in jobs {
            if let Some(watchdog) = job.watchdog {
                self.lh.remove(watchdog);
            }
            self.emit(WaylandEvent::Failed { generation, reason: "the Wayland connection was lost".into() });
        }
        self.emit(WaylandEvent::Disconnected);
    }

    // ------------------------------------------------------------------ commands

    fn on_cmd(&mut self, cmd: WaylandCmd) {
        match cmd {
            WaylandCmd::Overlay(state) => self.set_overlay(state),
            WaylandCmd::SnapshotClipboard { generation } => {
                if self.clipboard_ready(false).is_ok() {
                    self.feed(Input::SnapshotRequested { generation });
                }
            }
            WaylandCmd::Paste { generation, text, chord, delay_ms } => match self.accept_job(generation, true) {
                Err(reason) => self.emit(WaylandEvent::Failed { generation, reason }),
                Ok(()) => {
                    self.jobs.insert(generation, Job { kind: JobKind::Paste { chord, delay_ms }, watchdog: None });
                    self.feed(Input::PasteRequested { generation, text });
                }
            },
            WaylandCmd::Copy { generation, text } => match self.accept_job(generation, false) {
                Err(reason) => self.emit(WaylandEvent::Failed { generation, reason }),
                Ok(()) => {
                    self.jobs.insert(generation, Job { kind: JobKind::Copy, watchdog: None });
                    self.feed(Input::CopyRequested { generation, text });
                }
            },
        }
    }

    /// Connected, with data control (and a virtual keyboard to paste).
    fn clipboard_ready(&mut self, paste: bool) -> Result<(), String> {
        self.ensure_connected()?;
        let wl = self.wl.as_ref().expect("connected");
        if wl.dc.is_none() {
            return Err("the compositor offers no data-control protocol".into());
        }
        if paste && wl.vk.is_none() {
            return Err("the compositor offers no virtual keyboard".into());
        }
        Ok(())
    }

    fn accept_job(&mut self, generation: Gen, paste: bool) -> Result<(), String> {
        self.clipboard_ready(paste)?;
        if self.jobs.contains_key(&generation) || self.machine.has_op(generation) {
            return Err(format!("generation {generation} is already being delivered"));
        }
        Ok(())
    }

    /// Fails the job if the compositor doesn't confirm it in time.
    fn arm_watchdog(&mut self, generation: Gen) {
        let Some(job) = self.jobs.get(&generation) else { return };
        let timeout = match job.kind {
            JobKind::Paste { delay_ms, .. } => JOB_TIMEOUT + Duration::from_millis(delay_ms.into()),
            JobKind::Copy => JOB_TIMEOUT,
        };
        let token = self.after(timeout, move |st| {
            if st.jobs.remove(&generation).is_some() {
                let reason = format!("the compositor didn't confirm within {} s", timeout.as_secs());
                st.emit(WaylandEvent::Failed { generation, reason });
                st.feed(Input::Failed { generation });
            }
        });
        if let Some(job) = self.jobs.get_mut(&generation) {
            job.watchdog = Some(token);
        }
    }

    // ------------------------------------------------------------------ clipboard

    fn feed(&mut self, input: Input) {
        for action in self.machine.handle(input) {
            self.exec(action);
        }
    }

    fn exec(&mut self, action: Action) {
        match action {
            Action::Read { id, offer, mime } => self.start_read(id, offer, mime),
            Action::CancelRead { id } => {
                if let Some(io) = self.reads.remove(&id) {
                    self.lh.remove(io.source);
                    self.lh.remove(io.timer);
                }
            }
            Action::ExpireSnapshot { epoch, after } => {
                self.after(after, move |st| st.feed(Input::SnapshotExpired { epoch }));
            }
            Action::WaitForSnapshot { generation, max } => {
                self.after(max, move |st| st.feed(Input::PasteWaitExpired { generation }));
            }
            Action::StartPaste { generation, content } => {
                self.arm_watchdog(generation);
                self.set_own_selection(content, OwnKind::Dictation(generation));
                self.sync(SyncTag::PasteSet(generation));
            }
            Action::StartCopy { generation, content } => {
                self.arm_watchdog(generation);
                self.set_own_selection(content, OwnKind::Copy(generation));
                self.sync(SyncTag::CopySet(generation));
            }
            Action::ScheduleRestore { generation, after } => {
                self.after(after, move |st| st.feed(Input::RestoreDue { generation }));
            }
            Action::OfferRestore { generation, content } => {
                self.set_own_selection(content, OwnKind::Restore(generation));
            }
            Action::Note(note) => log::debug!("psst-wayland: clipboard: {note:?}"),
        }
    }

    fn set_own_selection(&mut self, content: Arc<Content>, kind: OwnKind) {
        let Some(wl) = self.wl.as_mut() else { return };
        let Some((manager, device)) = &wl.dc else { return };
        let items: Vec<(String, Bytes)> =
            content.offer_mimes().into_iter().map(|mime| (mime, content.clone() as Bytes)).collect();
        let source = manager.create_source(&wl.qh, items);
        device.set_selection(Some(&source));
        wl.latest = Some((source, kind));
    }

    fn sync(&mut self, tag: SyncTag) {
        if let Some(wl) = &self.wl {
            wl.conn.display().sync(&wl.qh, tag);
        }
    }

    fn on_sync(&mut self, tag: SyncTag) {
        match tag {
            SyncTag::PasteSet(generation) => {
                let Some(Job { kind: JobKind::Paste { delay_ms, .. }, .. }) = self.jobs.get(&generation) else {
                    return;
                };
                match *delay_ms {
                    0 => self.press_chord(generation),
                    ms => {
                        self.after(Duration::from_millis(ms.into()), move |st| st.press_chord(generation));
                    }
                }
            }
            SyncTag::PasteChord(generation) => {
                if let Some(job) = self.jobs.remove(&generation) {
                    if let Some(watchdog) = job.watchdog {
                        self.lh.remove(watchdog);
                    }
                    self.emit(WaylandEvent::Pasted { generation });
                    self.feed(Input::Pasted { generation });
                }
            }
            SyncTag::CopySet(generation) => {
                if let Some(job) = self.jobs.remove(&generation) {
                    if let Some(watchdog) = job.watchdog {
                        self.lh.remove(watchdog);
                    }
                    self.emit(WaylandEvent::Copied { generation });
                    self.feed(Input::Copied { generation });
                }
            }
        }
    }

    fn press_chord(&mut self, generation: Gen) {
        let Some(Job { kind: JobKind::Paste { chord, .. }, .. }) = self.jobs.get(&generation) else { return };
        let chord = *chord;
        let Some(vk) = self.wl.as_ref().and_then(|wl| wl.vk.as_ref()) else { return };
        keymap::send_chord(vk, chord, now_ms());
        self.sync(SyncTag::PasteChord(generation));
    }

    fn start_read(&mut self, id: ReadId, offer: OfferKey, mime: String) {
        let Some(offer) = self.wl.as_ref().and_then(|wl| wl.offers.get(&offer)).cloned() else {
            self.feed(Input::ReadDone { id, result: Err(ReadError::Io("the offer is gone".into())) });
            return;
        };
        // Only our end is non-blocking: the source writes to its end as it likes.
        let (read, write) = match pipe_with(PipeFlags::CLOEXEC) {
            Ok(pipe) => pipe,
            Err(err) => {
                self.feed(Input::ReadDone { id, result: Err(ReadError::Io(err.to_string())) });
                return;
            }
        };
        set_nonblocking(&read);
        offer.receive(mime, write.as_fd());
        drop(write);
        self.flush();
        let mut buf = Vec::new();
        let source = self
            .lh
            .insert_source(Generic::new(read, Interest::READ, Mode::Level), move |_, fd, st: &mut State| {
                let mut chunk = [0u8; 64 * 1024];
                loop {
                    match rustix::io::read(&**fd, &mut chunk) {
                        Ok(0) => {
                            st.finish_read(id, Ok(std::mem::take(&mut buf)));
                            return Ok(PostAction::Remove);
                        }
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if buf.len() > READ_CAP {
                                buf = Vec::new();
                                st.finish_read(id, Err(ReadError::TooLarge));
                                return Ok(PostAction::Remove);
                            }
                        }
                        Err(Errno::AGAIN) => return Ok(PostAction::Continue),
                        Err(Errno::INTR) => continue,
                        Err(err) => {
                            st.finish_read(id, Err(ReadError::Io(err.to_string())));
                            return Ok(PostAction::Remove);
                        }
                    }
                }
            })
            .expect("calloop accepts pipes");
        let timer = self.after(READ_TIMEOUT, move |st| {
            if let Some(io) = st.reads.remove(&id) {
                st.lh.remove(io.source);
                st.feed(Input::ReadDone { id, result: Err(ReadError::Timeout) });
            }
        });
        self.reads.insert(id, Io { source, timer });
    }

    fn finish_read(&mut self, id: ReadId, result: Result<Vec<u8>, ReadError>) {
        if let Some(io) = self.reads.remove(&id) {
            self.lh.remove(io.timer);
            self.feed(Input::ReadDone { id, result });
        }
    }

    fn start_write(&mut self, fd: OwnedFd, data: Bytes) {
        set_nonblocking(&fd);
        let id = self.next_write;
        self.next_write += 1;
        let mut offset = 0usize;
        let source = self
            .lh
            .insert_source(Generic::new(fd, Interest::WRITE, Mode::Level), move |_, fd, st: &mut State| {
                let bytes = (*data).as_ref();
                while offset < bytes.len() {
                    match rustix::io::write(&**fd, &bytes[offset..]) {
                        Ok(n) => offset += n,
                        Err(Errno::AGAIN) => return Ok(PostAction::Continue),
                        Err(Errno::INTR) => continue,
                        // The reader went away (EPIPE) or worse: nothing more to do.
                        Err(_) => break,
                    }
                }
                st.finish_write(id);
                Ok(PostAction::Remove)
            })
            .expect("calloop accepts pipes");
        let timer = self.after(WRITE_TIMEOUT, move |st| {
            if let Some(io) = st.writes.remove(&id) {
                st.lh.remove(io.source);
            }
        });
        self.writes.insert(id, Io { source, timer });
    }

    fn finish_write(&mut self, id: u64) {
        if let Some(io) = self.writes.remove(&id) {
            self.lh.remove(io.timer);
        }
    }
}

fn set_nonblocking(fd: &OwnedFd) {
    if let Ok(flags) = fcntl_getfl(fd) {
        let _ = fcntl_setfl(fd, flags | OFlags::NONBLOCK);
    }
}

/// CLOCK_MONOTONIC in milliseconds, for key event timestamps.
fn now_ms() -> u32 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    (t.tv_sec as u64 * 1000 + t.tv_nsec as u64 / 1_000_000) as u32
}

impl DcHandler for State {
    fn dc_selection(&mut self, _: &Connection, offer: Option<DcOffer>) {
        let Some(wl) = self.wl.as_mut() else { return };
        // psst's selection: its latest source is still alive. (A compositor cancels the old
        // source before it announces a new selection.)
        let ours = wl.latest.as_ref().filter(|(source, _)| source.is_live()).map(|(_, kind)| kind.clone());
        if let Some(old) = wl.current_offer.take()
            && let Some(old) = wl.offers.remove(&old)
        {
            old.destroy();
        }
        let owner = match (ours, offer) {
            (Some(kind), offer) => {
                if let Some(offer) = offer {
                    offer.destroy();
                }
                Owner::Ours(kind)
            }
            (None, Some(offer)) => {
                let key = wl.next_offer;
                wl.next_offer += 1;
                let mimes = offer.mimes();
                wl.offers.insert(key, offer);
                wl.current_offer = Some(key);
                Owner::External { offer: key, mimes }
            }
            (None, None) => Owner::Empty,
        };
        self.feed(Input::SelectionChanged(owner));
    }

    fn dc_send(&mut self, source: &DcSource, mime: String, fd: OwnedFd) {
        if let Some(bytes) = source.data().bytes_for(&mime) {
            self.start_write(fd, bytes);
        }
    }

    fn dc_cancelled(&mut self, source: &DcSource) {
        // Drop our handle too: its user data holds the served bytes.
        if let Some(wl) = self.wl.as_mut()
            && wl.latest.as_ref().is_some_and(|(latest, _)| latest == source)
        {
            wl.latest = None;
        }
        source.destroy();
    }

    fn dc_finished(&mut self) {
        self.broken.get_or_insert("the data-control device was removed".into());
    }
}

delegate_dc!(State);

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Globals are bound once per connection; later ones are ignored.
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
        st.on_sync(*tag);
    }
}

delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ZwpVirtualKeyboardV1);
