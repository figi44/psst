//! ext-data-control-v1 and wlr-data-control-unstable-v1 behind one API (`docs/decisions/desktop.md`:
//! both behave identically on niri and sway). The two protocols have the same shape, so a macro
//! generates the dispatch for both. ext is preferred when both are offered.

use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};

pub use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
pub use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

/// A data-control protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    /// `ext_data_control_manager_v1`
    Ext,
    /// `zwlr_data_control_manager_v1` (version 2)
    Wlr,
}

/// Data a source serves, shared by every type it's offered under.
pub type Bytes = Arc<dyn AsRef<[u8]> + Send + Sync>;

#[derive(Default)]
pub struct OfferData {
    mimes: Mutex<Vec<String>>,
}

pub struct SourceData {
    pub items: Vec<(String, Bytes)>,
    live: AtomicBool,
}

impl SourceData {
    pub fn bytes_for(&self, mime: &str) -> Option<Bytes> {
        self.items.iter().find(|(m, _)| m == mime).map(|(_, b)| b.clone())
    }
}

/// Implemented by the state that owns a data-control device.
pub trait DcHandler: Sized {
    /// New selection; `None` means the clipboard is empty.
    fn dc_selection(&mut self, conn: &Connection, offer: Option<DcOffer>);
    /// Someone wants our data: write it to `fd` and close it.
    fn dc_send(&mut self, source: &DcSource, mime: String, fd: OwnedFd);
    /// Our source was replaced (it is already marked dead).
    fn dc_cancelled(&mut self, source: &DcSource);
    /// The device is gone (for example the seat was removed).
    fn dc_finished(&mut self);
}

#[derive(Clone, Debug)]
pub enum DcManager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

#[derive(Clone, Debug)]
pub enum DcDevice {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DcSource {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DcOffer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl DcManager {
    pub fn bind<D>(globals: &GlobalList, qh: &QueueHandle<D>, want: Option<Proto>) -> Result<Self, String>
    where
        D: Dispatch<ExtDataControlManagerV1, ()> + Dispatch<ZwlrDataControlManagerV1, ()> + 'static,
    {
        let ext = || globals.bind::<ExtDataControlManagerV1, D, ()>(qh, 1..=1, ()).map(DcManager::Ext);
        let wlr = || globals.bind::<ZwlrDataControlManagerV1, D, ()>(qh, 1..=2, ()).map(DcManager::Wlr);
        match want {
            Some(Proto::Ext) => ext().map_err(|e| format!("ext_data_control_manager_v1: {e}")),
            Some(Proto::Wlr) => wlr().map_err(|e| format!("zwlr_data_control_manager_v1: {e}")),
            None => ext().or_else(|_| wlr()).map_err(|_| "the compositor offers no data-control protocol".to_string()),
        }
    }

    pub fn proto(&self) -> Proto {
        match self {
            DcManager::Ext(_) => Proto::Ext,
            DcManager::Wlr(_) => Proto::Wlr,
        }
    }

    pub fn get_device<D>(&self, seat: &WlSeat, qh: &QueueHandle<D>) -> DcDevice
    where
        D: Dispatch<ExtDataControlDeviceV1, ()> + Dispatch<ZwlrDataControlDeviceV1, ()> + 'static,
    {
        match self {
            DcManager::Ext(m) => DcDevice::Ext(m.get_data_device(seat, qh, ())),
            DcManager::Wlr(m) => DcDevice::Wlr(m.get_data_device(seat, qh, ())),
        }
    }

    /// A source offering every `(mime, bytes)` item.
    pub fn create_source<D>(&self, qh: &QueueHandle<D>, items: Vec<(String, Bytes)>) -> DcSource
    where
        D: Dispatch<ExtDataControlSourceV1, SourceData> + Dispatch<ZwlrDataControlSourceV1, SourceData> + 'static,
    {
        let mimes: Vec<String> = items.iter().map(|(m, _)| m.clone()).collect();
        let data = SourceData { items, live: AtomicBool::new(true) };
        let src = match self {
            DcManager::Ext(m) => DcSource::Ext(m.create_data_source(qh, data)),
            DcManager::Wlr(m) => DcSource::Wlr(m.create_data_source(qh, data)),
        };
        for m in mimes {
            match &src {
                DcSource::Ext(s) => s.offer(m),
                DcSource::Wlr(s) => s.offer(m),
            }
        }
        src
    }
}

impl DcDevice {
    pub fn set_selection(&self, source: Option<&DcSource>) {
        match (self, source) {
            (DcDevice::Ext(d), Some(DcSource::Ext(s))) => d.set_selection(Some(s)),
            (DcDevice::Ext(d), None) => d.set_selection(None),
            (DcDevice::Wlr(d), Some(DcSource::Wlr(s))) => d.set_selection(Some(s)),
            (DcDevice::Wlr(d), None) => d.set_selection(None),
            // Sources always come from the same manager as the device.
            _ => unreachable!("data-control protocol mismatch"),
        }
    }
}

impl DcSource {
    pub fn data(&self) -> &SourceData {
        match self {
            DcSource::Ext(s) => s.data::<SourceData>().expect("source data"),
            DcSource::Wlr(s) => s.data::<SourceData>().expect("source data"),
        }
    }
    pub fn is_live(&self) -> bool {
        self.data().live.load(Ordering::SeqCst)
    }
    pub fn destroy(&self) {
        match self {
            DcSource::Ext(s) => s.destroy(),
            DcSource::Wlr(s) => s.destroy(),
        }
    }
}

impl DcOffer {
    pub fn mimes(&self) -> Vec<String> {
        let d = match self {
            DcOffer::Ext(o) => o.data::<OfferData>(),
            DcOffer::Wlr(o) => o.data::<OfferData>(),
        };
        d.map(|d| d.mimes.lock().unwrap().clone()).unwrap_or_default()
    }
    pub fn receive(&self, mime: String, fd: BorrowedFd<'_>) {
        match self {
            DcOffer::Ext(o) => o.receive(mime, fd),
            DcOffer::Wlr(o) => o.receive(mime, fd),
        }
    }
    pub fn destroy(&self) {
        match self {
            DcOffer::Ext(o) => o.destroy(),
            DcOffer::Wlr(o) => o.destroy(),
        }
    }
}

/// Delegate target: `delegate_dc!(State)` routes all data-control objects here.
pub struct DcDelegate;

macro_rules! impl_dc {
    ($variant:ident, $mgr:ty, $dev:ty, $devmod:ident, $offer:ty, $offermod:ident, $src:ty, $srcmod:ident) => {
        impl<D> Dispatch<$mgr, (), D> for DcDelegate
        where
            D: Dispatch<$mgr, ()>,
        {
            fn event(_: &mut D, _: &$mgr, _: <$mgr as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<D>) {}
        }

        impl<D> Dispatch<$dev, (), D> for DcDelegate
        where
            D: Dispatch<$dev, ()> + Dispatch<$offer, OfferData> + DcHandler + 'static,
        {
            fn event(
                state: &mut D,
                _: &$dev,
                event: $devmod::Event,
                _: &(),
                conn: &Connection,
                _: &QueueHandle<D>,
            ) {
                match event {
                    $devmod::Event::Selection { id } => state.dc_selection(conn, id.map(DcOffer::$variant)),
                    $devmod::Event::PrimarySelection { id: Some(o) } => o.destroy(),
                    $devmod::Event::Finished => state.dc_finished(),
                    _ => {}
                }
            }

            event_created_child!(D, $dev, [
                $devmod::EVT_DATA_OFFER_OPCODE => ($offer, OfferData::default()),
            ]);
        }

        impl<D> Dispatch<$offer, OfferData, D> for DcDelegate
        where
            D: Dispatch<$offer, OfferData>,
        {
            fn event(
                _: &mut D,
                _: &$offer,
                event: $offermod::Event,
                data: &OfferData,
                _: &Connection,
                _: &QueueHandle<D>,
            ) {
                if let $offermod::Event::Offer { mime_type } = event {
                    data.mimes.lock().unwrap().push(mime_type);
                }
            }
        }

        impl<D> Dispatch<$src, SourceData, D> for DcDelegate
        where
            D: Dispatch<$src, SourceData> + DcHandler,
        {
            fn event(
                state: &mut D,
                src: &$src,
                event: $srcmod::Event,
                data: &SourceData,
                _: &Connection,
                _: &QueueHandle<D>,
            ) {
                match event {
                    $srcmod::Event::Send { mime_type, fd } => {
                        state.dc_send(&DcSource::$variant(src.clone()), mime_type, fd)
                    }
                    $srcmod::Event::Cancelled => {
                        data.live.store(false, Ordering::SeqCst);
                        state.dc_cancelled(&DcSource::$variant(src.clone()));
                    }
                    _ => {}
                }
            }
        }
    };
}

impl_dc!(
    Ext,
    ExtDataControlManagerV1,
    ExtDataControlDeviceV1,
    ext_data_control_device_v1,
    ExtDataControlOfferV1,
    ext_data_control_offer_v1,
    ExtDataControlSourceV1,
    ext_data_control_source_v1
);
impl_dc!(
    Wlr,
    ZwlrDataControlManagerV1,
    ZwlrDataControlDeviceV1,
    zwlr_data_control_device_v1,
    ZwlrDataControlOfferV1,
    zwlr_data_control_offer_v1,
    ZwlrDataControlSourceV1,
    zwlr_data_control_source_v1
);

macro_rules! delegate_dc {
    ($ty:ty) => {
        wayland_client::delegate_dispatch!($ty: [$crate::dc::ExtDataControlManagerV1: ()] => $crate::dc::DcDelegate);
        wayland_client::delegate_dispatch!($ty: [$crate::dc::ExtDataControlDeviceV1: ()] => $crate::dc::DcDelegate);
        wayland_client::delegate_dispatch!($ty: [$crate::dc::ExtDataControlOfferV1: $crate::dc::OfferData] => $crate::dc::DcDelegate);
        wayland_client::delegate_dispatch!($ty: [$crate::dc::ExtDataControlSourceV1: $crate::dc::SourceData] => $crate::dc::DcDelegate);
        wayland_client::delegate_dispatch!($ty: [$crate::dc::ZwlrDataControlManagerV1: ()] => $crate::dc::DcDelegate);
        wayland_client::delegate_dispatch!($ty: [$crate::dc::ZwlrDataControlDeviceV1: ()] => $crate::dc::DcDelegate);
        wayland_client::delegate_dispatch!($ty: [$crate::dc::ZwlrDataControlOfferV1: $crate::dc::OfferData] => $crate::dc::DcDelegate);
        wayland_client::delegate_dispatch!($ty: [$crate::dc::ZwlrDataControlSourceV1: $crate::dc::SourceData] => $crate::dc::DcDelegate);
    };
}
pub(crate) use delegate_dc;
