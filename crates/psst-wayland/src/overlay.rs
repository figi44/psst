use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use calloop::RegistrationToken;
use psst_overlay::{Key, Menu, MenuOutcome, MenuSpec, Meter, OverlayState, Pixmap, Renderer};
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use wayland_client::globals::GlobalList;
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_callback::{self, WlCallback},
    wl_compositor::WlCompositor,
    wl_region::WlRegion,
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::{self, WlSurface},
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::presentation_time::client::{
    wp_presentation::WpPresentation,
    wp_presentation_feedback::{self, WpPresentationFeedback},
};
use wayland_protocols::wp::viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use crate::thread::State;
use crate::{Anchor, WaylandEvent};

pub(crate) struct Globals {
    pub(crate) compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    viewporter: Option<WpViewporter>,
    fractional: Option<WpFractionalScaleManagerV1>,
    presentation: Option<WpPresentation>,
}

impl Globals {
    pub(crate) fn bind(globals: &GlobalList, qh: &QueueHandle<State>) -> Self {
        Self {
            compositor: globals.bind(qh, 4..=6, ()).ok(),
            shm: globals.bind(qh, 1..=1, ()).ok(),
            layer_shell: globals.bind(qh, 1..=4, ()).ok(),
            viewporter: globals.bind(qh, 1..=1, ()).ok(),
            fractional: globals.bind(qh, 1..=1, ()).ok(),
            presentation: globals.bind(qh, 1..=1, ()).ok(),
        }
    }
}

/// The waveform redraws on the compositor's frame callbacks, but no more often than this: every
/// callback at 60 Hz, every other one at 120 or 144 Hz.
const WAVE_FRAME: Duration = Duration::from_millis(12);

pub(crate) struct Overlay {
    renderer: Renderer,
    meter: Meter,
    /// When the meter last advanced.
    advanced: Option<Instant>,
    /// The next waveform frame, when it waits for [`WAVE_FRAME`] to pass.
    tick: Option<RegistrationToken>,
    desired: Option<OverlayState>,
    /// Shown instead of the pill while it's open.
    menu: Option<Menu>,
    surface: Option<Surface>,
    next_id: u64,
    warned: bool,
}

impl Overlay {
    pub(crate) fn new() -> Self {
        Self {
            renderer: Renderer::new(),
            meter: Meter::default(),
            advanced: None,
            tick: None,
            desired: None,
            menu: None,
            surface: None,
            next_id: 1,
            warned: false,
        }
    }

    fn waving(&self) -> bool {
        self.menu.is_none() && self.desired.as_ref().is_some_and(|s| s.level().is_some())
    }

    fn wanted(&self) -> Option<Kind> {
        match (&self.menu, &self.desired) {
            (Some(_), _) => Some(Kind::Menu),
            (None, Some(_)) => Some(Kind::Pill),
            (None, None) => None,
        }
    }

    pub(crate) fn is_menu(&self, surface: &WlSurface) -> bool {
        self.surface.as_ref().is_some_and(|s| s.kind == Kind::Menu && s.wl_surface == *surface)
    }

    /// The connection is gone, and its objects with it. The menu goes too: it had the keyboard.
    pub(crate) fn forget_surface(&mut self) {
        self.surface = None;
        self.menu = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pill,
    /// Takes the keyboard and the pointer.
    Menu,
}

/// Tags a surface's callbacks, so ones from a destroyed surface are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SurfaceId(u64);

struct Surface {
    id: SurfaceId,
    kind: Kind,
    wl_surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    configured: bool,
    /// Logical, from the last configure.
    size: (u32, u32),
    /// Logical, as last asked for with `set_size`.
    requested: (u32, u32),
    scale120: Option<u32>,
    buffer_scale: i32,
    buffers: Option<Buffers>,
    frame_pending: bool,
    dirty: bool,
    shown: bool,
}

impl Surface {
    fn destroy(self) {
        if let Some(f) = self.fractional {
            f.destroy();
        }
        if let Some(v) = self.viewport {
            v.destroy();
        }
        self.layer.destroy();
        self.wl_surface.destroy();
        if let Some(b) = self.buffers {
            b.destroy();
        }
    }
}

impl State {
    pub(crate) fn set_overlay(&mut self, state: Option<OverlayState>) {
        if self.overlay.desired.is_none() {
            self.overlay.meter = Meter::default();
        }
        if let Some(level) = state.as_ref().and_then(OverlayState::level) {
            self.overlay.meter.set(level);
        }
        self.overlay.desired = state;
        self.show();
    }

    pub(crate) fn set_menu(&mut self, spec: Option<MenuSpec>) {
        match (spec, &mut self.overlay.menu) {
            (Some(spec), Some(menu)) => menu.update(spec),
            (Some(spec), None) => self.overlay.menu = Some(Menu::new(spec)),
            (None, _) => self.overlay.menu = None,
        }
        self.show();
    }

    pub(crate) fn overlay_hide(&mut self) {
        self.overlay.desired = None;
        self.overlay.menu = None;
        self.show();
    }

    /// Makes the surface match the menu and the pill: the menu if it's open, else the pill if
    /// there's one.
    fn show(&mut self) {
        let wanted = self.overlay.wanted();
        if self.overlay.surface.as_ref().is_some_and(|s| Some(s.kind) != wanted) {
            self.drop_surface();
        }
        let Some(kind) = wanted else { return };
        if self.ensure_connected().is_ok() && self.overlay.surface.is_none() {
            self.create_surface(kind);
        }
        match &mut self.overlay.surface {
            Some(surface) => surface.dirty = true,
            // Pastes wait for the menu to close, so one that can't be shown closes at once.
            None if kind == Kind::Menu => {
                self.overlay.menu = None;
                self.emit(WaylandEvent::Menu(MenuOutcome::Closed));
                return;
            }
            None => return,
        }
        self.draw_overlay();
    }

    fn drop_surface(&mut self) {
        if let Some(tick) = self.overlay.tick.take() {
            self.lh.remove(tick);
        }
        if let Some(surface) = self.overlay.surface.take() {
            if surface.kind == Kind::Menu {
                self.release_input();
            }
            surface.destroy();
        }
    }

    pub(crate) fn menu_key(&mut self, key: Key) {
        let Some(menu) = &mut self.overlay.menu else { return };
        let outcome = menu.key(key);
        self.menu_changed(outcome);
    }

    pub(crate) fn menu_hover(&mut self, x: f64, y: f64) {
        if self.overlay.menu.as_mut().is_some_and(|menu| menu.hover(x, y)) {
            self.menu_changed(None);
        }
    }

    pub(crate) fn menu_click(&mut self, x: f64, y: f64) {
        let Some(menu) = &mut self.overlay.menu else { return };
        let outcome = menu.click(x, y);
        self.menu_changed(outcome);
    }

    fn menu_changed(&mut self, outcome: Option<MenuOutcome>) {
        match outcome {
            Some(outcome) => {
                self.overlay.menu = None;
                self.show();
                self.emit(WaylandEvent::Menu(outcome));
            }
            None => {
                if let Some(surface) = &mut self.overlay.surface {
                    surface.dirty = true;
                }
                self.draw_overlay();
            }
        }
    }

    /// Called when a frame callback frees the surface for the next frame.
    fn next_wave_frame(&mut self) {
        if !self.overlay.waving() || self.overlay.tick.is_some() {
            return;
        }
        let due = self.overlay.advanced.map(|at| at + WAVE_FRAME);
        let wait = due.map_or(Duration::ZERO, |due| due.saturating_duration_since(Instant::now()));
        if wait.is_zero() {
            if let Some(s) = &mut self.overlay.surface {
                s.dirty = true;
            }
            return;
        }
        let tick = self.after(wait, |st| {
            st.overlay.tick = None;
            if st.overlay.waving()
                && let Some(s) = &mut st.overlay.surface
            {
                s.dirty = true;
                st.draw_overlay();
            }
        });
        self.overlay.tick = Some(tick);
    }

    fn create_surface(&mut self, kind: Kind) {
        let Some(wl) = &self.wl else { return };
        let g = &wl.surfaces;
        let (Some(compositor), Some(_), Some(layer_shell)) = (&g.compositor, &g.shm, &g.layer_shell) else {
            if !self.overlay.warned {
                log::warn!("psst-wayland: the compositor lacks layer-shell or shm; the overlay stays hidden");
                self.overlay.warned = true;
            }
            return;
        };
        let qh = &wl.qh;
        let id = SurfaceId(self.overlay.next_id);
        self.overlay.next_id += 1;
        let wl_surface = compositor.create_surface(qh, id);
        let cfg = &self.opts.overlay;
        let (namespace, size, keyboard) = match (kind, &self.overlay.menu) {
            (Kind::Menu, Some(menu)) => ("psst-menu", menu.size(), KeyboardInteractivity::Exclusive),
            _ => {
                // Clicks go through: an empty input region.
                let region = compositor.create_region(qh, ());
                wl_surface.set_input_region(Some(&region));
                region.destroy();
                ("psst", (cfg.width, cfg.height), KeyboardInteractivity::None)
            }
        };
        let layer = layer_shell.get_layer_surface(&wl_surface, None, Layer::Overlay, namespace.into(), qh, id);
        layer.set_size(size.0, size.1);
        match cfg.anchor {
            Anchor::Bottom => {
                layer.set_anchor(zwlr_layer_surface_v1::Anchor::Bottom);
                layer.set_margin(0, 0, cfg.margin as i32, 0);
            }
            Anchor::Top => {
                layer.set_anchor(zwlr_layer_surface_v1::Anchor::Top);
                layer.set_margin(cfg.margin as i32, 0, 0, 0);
            }
        }
        layer.set_keyboard_interactivity(keyboard);
        let viewport = g.viewporter.as_ref().map(|v| v.get_viewport(&wl_surface, qh, ()));
        let fractional = g.fractional.as_ref().map(|f| f.get_fractional_scale(&wl_surface, qh, id));
        // The first commit has no buffer; the compositor answers with a configure.
        wl_surface.commit();
        self.overlay.surface = Some(Surface {
            id,
            kind,
            wl_surface,
            layer,
            viewport,
            fractional,
            configured: false,
            size,
            requested: size,
            scale120: None,
            buffer_scale: 1,
            buffers: None,
            frame_pending: false,
            dirty: true,
            shown: false,
        });
    }

    fn draw_overlay(&mut self) {
        let Some(wl) = &self.wl else { return };
        let Overlay { renderer, meter, advanced, desired, menu, surface: Some(s), .. } = &mut self.overlay else {
            return;
        };
        // A menu page of another height: the compositor configures the new size before it's drawn.
        if let (Kind::Menu, Some(menu)) = (s.kind, &menu)
            && menu.size() != s.requested
        {
            s.requested = menu.size();
            s.layer.set_size(s.requested.0, s.requested.1);
            s.wl_surface.commit();
            s.configured = false;
        }
        if !s.configured || s.frame_pending || !s.dirty {
            return;
        }
        let Some(shm) = &wl.surfaces.shm else { return };
        let qh = &wl.qh;
        let scale = match (s.scale120, &s.viewport) {
            (Some(s120), Some(_)) => s120 as f64 / 120.0,
            _ => s.buffer_scale as f64,
        };
        let style = &self.opts.overlay.style;
        let ((w, h), pixmap) = match (s.kind, menu, desired) {
            (Kind::Menu, Some(menu), _) => (menu.size(), renderer.render_menu(menu, style, scale)),
            (Kind::Pill, None, Some(state)) => {
                let now = Instant::now();
                let wave = meter.advance(advanced.map_or(Duration::ZERO, |at| now - at));
                *advanced = Some(now);
                let (w, h) = s.size;
                ((w, h), renderer.render(state, wave, style, w, h, scale))
            }
            _ => return,
        };
        let (pw, ph) = (pixmap.width(), pixmap.height());
        if s.buffers.as_ref().is_some_and(|b| (b.width, b.height) != (pw, ph))
            && let Some(old) = s.buffers.take()
        {
            old.destroy();
        }
        if s.buffers.is_none() {
            match Buffers::new(shm, qh, pw, ph) {
                Ok(b) => s.buffers = Some(b),
                Err(err) => {
                    log::warn!("psst-wayland: cannot allocate overlay buffers: {err}");
                    return;
                }
            }
        }
        let buffer = match s.buffers.as_mut().expect("just allocated").next(qh, &pixmap) {
            Ok(buffer) => buffer,
            Err(err) => {
                log::warn!("psst-wayland: cannot fill an overlay buffer: {err}");
                return;
            }
        };
        s.wl_surface.attach(Some(&buffer), 0, 0);
        match &s.viewport {
            Some(viewport) => viewport.set_destination(w as i32, h as i32),
            None => s.wl_surface.set_buffer_scale(s.buffer_scale),
        }
        s.wl_surface.damage_buffer(0, 0, pw as i32, ph as i32);
        s.wl_surface.frame(qh, s.id);
        s.frame_pending = true;
        if !s.shown
            && let Some(presentation) = &wl.surfaces.presentation
        {
            presentation.feedback(&s.wl_surface, qh, s.id);
        }
        s.wl_surface.commit();
        s.dirty = false;
    }

    fn surface_mut(&mut self, id: SurfaceId) -> Option<&mut Surface> {
        self.overlay.surface.as_mut().filter(|s| s.id == id)
    }

    fn mark_shown(&mut self, id: SurfaceId) {
        if let Some(s) = self.surface_mut(id)
            && !s.shown
        {
            s.shown = true;
            self.emit(WaylandEvent::OverlayShown);
        }
    }
}

struct Buffers {
    fd: OwnedFd,
    pool: WlShmPool,
    pool_size: usize,
    width: u32,
    height: u32,
    slots: Vec<Slot>,
}

struct Slot {
    buffer: WlBuffer,
    offset: usize,
    busy: Arc<AtomicBool>,
}

impl Buffers {
    fn frame_bytes(width: u32, height: u32) -> usize {
        width as usize * height as usize * 4
    }

    fn new(shm: &WlShm, qh: &QueueHandle<State>, width: u32, height: u32) -> std::io::Result<Self> {
        let fd = memfd_create("psst-overlay", MemfdFlags::CLOEXEC)?;
        let size = Self::frame_bytes(width, height);
        ftruncate(&fd, size as u64)?;
        let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
        let mut buffers = Self { fd, pool, pool_size: size, width, height, slots: Vec::new() };
        buffers.add_slot(qh, 0);
        Ok(buffers)
    }

    fn add_slot(&mut self, qh: &QueueHandle<State>, offset: usize) {
        let busy = Arc::new(AtomicBool::new(false));
        let buffer = self.pool.create_buffer(
            offset as i32,
            self.width as i32,
            self.height as i32,
            self.width as i32 * 4,
            wl_shm::Format::Argb8888,
            qh,
            busy.clone(),
        );
        self.slots.push(Slot { buffer, offset, busy });
    }

    fn next(&mut self, qh: &QueueHandle<State>, pixmap: &Pixmap) -> std::io::Result<WlBuffer> {
        let index = match self.slots.iter().position(|s| !s.busy.load(Ordering::Acquire)) {
            Some(i) => i,
            None => {
                let offset = self.pool_size;
                self.pool_size += Self::frame_bytes(self.width, self.height);
                ftruncate(&self.fd, self.pool_size as u64)?;
                self.pool.resize(self.pool_size as i32);
                self.add_slot(qh, offset);
                self.slots.len() - 1
            }
        };
        // Premultiplied RGBA → ARGB8888, which is B, G, R, A in memory.
        let mut bgra = Vec::with_capacity(pixmap.data().len());
        for px in pixmap.data().as_chunks::<4>().0 {
            bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
        let slot = &self.slots[index];
        let mut written = 0;
        while written < bgra.len() {
            written += rustix::io::pwrite(&self.fd, &bgra[written..], (slot.offset + written) as u64)?;
        }
        slot.busy.store(true, Ordering::Release);
        Ok(slot.buffer.clone())
    }

    fn destroy(self) {
        for slot in self.slots {
            slot.buffer.destroy();
        }
        self.pool.destroy();
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, SurfaceId> for State {
    fn event(
        st: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        id: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                layer.ack_configure(serial);
                if let Some(s) = st.surface_mut(*id) {
                    let (rw, rh) = s.requested;
                    s.size = (if width > 0 { width } else { rw }, if height > 0 { height } else { rh });
                    s.configured = true;
                    s.dirty = true;
                }
                st.draw_overlay();
            }
            // The compositor took it away (its output went, say); the next state makes a new one.
            // The menu is closed instead: the user would have to find their place in it again.
            zwlr_layer_surface_v1::Event::Closed if st.surface_mut(*id).is_some() => {
                st.drop_surface();
                if st.overlay.menu.take().is_some() {
                    st.emit(WaylandEvent::Menu(MenuOutcome::Closed));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, SurfaceId> for State {
    fn event(
        st: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        id: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(s) = st.surface_mut(*id)
            && s.scale120 != Some(scale)
        {
            s.scale120 = Some(scale);
            s.dirty = true;
            st.draw_overlay();
        }
    }
}

impl Dispatch<WlSurface, SurfaceId> for State {
    fn event(
        st: &mut Self,
        _: &WlSurface,
        event: wl_surface::Event,
        id: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::PreferredBufferScale { factor } = event
            && let Some(s) = st.surface_mut(*id)
            && s.buffer_scale != factor
        {
            s.buffer_scale = factor.max(1);
            if s.scale120.is_none() {
                s.dirty = true;
                st.draw_overlay();
            }
        }
    }
}

impl Dispatch<WlCallback, SurfaceId> for State {
    fn event(
        st: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        id: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            let presentation = st.wl.as_ref().is_some_and(|wl| wl.surfaces.presentation.is_some());
            let Some(s) = st.surface_mut(*id) else { return };
            s.frame_pending = false;
            if !presentation {
                // Without wp_presentation, the first frame callback is the best sign it's shown.
                st.mark_shown(*id);
            }
            st.next_wave_frame();
            st.draw_overlay();
        }
    }
}

impl Dispatch<WpPresentationFeedback, SurfaceId> for State {
    fn event(
        st: &mut Self,
        _: &WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        id: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented { .. } => st.mark_shown(*id),
            wp_presentation_feedback::Event::Discarded => {
                // Replaced before it was shown: the next frame asks again.
                if let Some(s) = st.surface_mut(*id)
                    && !s.shown
                {
                    s.dirty = true;
                    st.draw_overlay();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlBuffer, Arc<AtomicBool>> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        busy: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            busy.store(false, Ordering::Release);
        }
    }
}

delegate_noop!(State: WlCompositor);
delegate_noop!(State: WlRegion);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ZwlrLayerShellV1);
delegate_noop!(State: WpViewporter);
delegate_noop!(State: WpViewport);
delegate_noop!(State: WpFractionalScaleManagerV1);
delegate_noop!(State: ignore WpPresentation);
