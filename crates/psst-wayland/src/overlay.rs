use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use psst_overlay::{Levels, OverlayState, Pixmap, Renderer};
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
    compositor: Option<WlCompositor>,
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

pub(crate) struct Overlay {
    renderer: Renderer,
    levels: Levels,
    desired: Option<OverlayState>,
    surface: Option<Surface>,
    next_id: u64,
    warned: bool,
}

impl Overlay {
    pub(crate) fn new() -> Self {
        Self {
            renderer: Renderer::new(),
            levels: Levels::new(),
            desired: None,
            surface: None,
            next_id: 1,
            warned: false,
        }
    }

    /// The connection is gone, and its objects with it.
    pub(crate) fn forget_surface(&mut self) {
        self.surface = None;
    }
}

/// Tags a surface's callbacks, so ones from a destroyed surface are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SurfaceId(u64);

struct Surface {
    id: SurfaceId,
    wl_surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    configured: bool,
    /// Logical, from the last configure.
    size: (u32, u32),
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
        let Some(state) = state else {
            self.overlay_hide();
            return;
        };
        if self.overlay.desired.is_none() {
            self.overlay.levels.clear();
        }
        if let Some(level) = state.level() {
            self.overlay.levels.push(level);
        }
        self.overlay.desired = Some(state);
        if self.ensure_connected().is_err() {
            return;
        }
        if self.overlay.surface.is_none() {
            self.create_surface();
        }
        if let Some(surface) = &mut self.overlay.surface {
            surface.dirty = true;
        }
        self.draw_overlay();
    }

    pub(crate) fn overlay_hide(&mut self) {
        self.overlay.desired = None;
        self.overlay.levels.clear();
        if let Some(surface) = self.overlay.surface.take() {
            surface.destroy();
        }
    }

    fn create_surface(&mut self) {
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
        // Clicks go through: an empty input region.
        let region = compositor.create_region(qh, ());
        wl_surface.set_input_region(Some(&region));
        region.destroy();
        let layer = layer_shell.get_layer_surface(&wl_surface, None, Layer::Overlay, "psst".into(), qh, id);
        let cfg = &self.opts.overlay;
        layer.set_size(cfg.width, cfg.height);
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
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        let viewport = g.viewporter.as_ref().map(|v| v.get_viewport(&wl_surface, qh, ()));
        let fractional = g.fractional.as_ref().map(|f| f.get_fractional_scale(&wl_surface, qh, id));
        // The first commit has no buffer; the compositor answers with a configure.
        wl_surface.commit();
        self.overlay.surface = Some(Surface {
            id,
            wl_surface,
            layer,
            viewport,
            fractional,
            configured: false,
            size: (cfg.width, cfg.height),
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
        let Overlay { renderer, levels, desired: Some(state), surface: Some(s), .. } = &mut self.overlay else {
            return;
        };
        if !s.configured || s.frame_pending || !s.dirty {
            return;
        }
        let Some(shm) = &wl.surfaces.shm else { return };
        let qh = &wl.qh;
        let (w, h) = s.size;
        let scale = match (s.scale120, &s.viewport) {
            (Some(s120), Some(_)) => s120 as f64 / 120.0,
            _ => s.buffer_scale as f64,
        };
        let pixmap = renderer.render(state, levels, &self.opts.overlay.style, w, h, scale);
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
                let (cw, ch) = (st.opts.overlay.width, st.opts.overlay.height);
                if let Some(s) = st.surface_mut(*id) {
                    s.size = (if width > 0 { width } else { cw }, if height > 0 { height } else { ch });
                    s.configured = true;
                    s.dirty = true;
                }
                st.draw_overlay();
            }
            zwlr_layer_surface_v1::Event::Closed => {
                // The compositor took it away (its output went, say); the next state makes a new one.
                if st.surface_mut(*id).is_some()
                    && let Some(s) = st.overlay.surface.take()
                {
                    s.destroy();
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
