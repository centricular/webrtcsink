use anyhow::Error;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::ObjectSubclassExt;

mod imp;
mod utils;

glib::wrapper! {
    pub struct WebRTCSink(ObjectSubclass<imp::WebRTCSink>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy;
}

unsafe impl Send for WebRTCSink {}
unsafe impl Sync for WebRTCSink {}

pub trait Signallable: Sync + Send + 'static {
    fn start(&mut self, element: &WebRTCSink) -> Result<(), Error>;

    fn handle_sdp(
        &mut self,
        element: &WebRTCSink,
        peer_id: &str,
        sdp: &gst_webrtc::WebRTCSessionDescription,
    ) -> Result<(), Error>;

    /// sdp_mid is exposed for future proofing, see
    /// https://gitlab.freedesktop.org/gstreamer/gst-plugins-bad/-/issues/1174,
    /// at the moment sdp_mline_index will always be Some and sdp_mid will always
    /// be None
    fn handle_ice(
        &mut self,
        element: &WebRTCSink,
        peer_id: &str,
        candidate: &str,
        sdp_mline_index: Option<u32>,
        sdp_mid: Option<String>,
    ) -> Result<(), Error>;

    fn consumer_removed(&mut self, element: &WebRTCSink, peer_id: &str);

    fn stop(&mut self, element: &WebRTCSink);
}

/// When providing a signaller, we expect it to both be a GObject
/// and be Signallable. This is arguably a bit strange, but exposing
/// a GInterface from rust is at the moment a bit awkward, so I went
/// for a rust interface for now. The reason the signaller needs to be
/// a GObject is to make its properties available through the GstChildProxy
/// interface.
pub trait SignallableObject: AsRef<glib::Object> + Signallable {}

impl<T: AsRef<glib::Object> + Signallable> SignallableObject for T {}

impl WebRTCSink {
    pub fn new() -> Self {
        glib::Object::new(&[]).unwrap()
    }

    pub fn with_signaller(signaller: Box<dyn SignallableObject>) -> Self {
        let ret: WebRTCSink = glib::Object::new(&[]).unwrap();

        let ws = imp::WebRTCSink::from_instance(&ret);

        ws.set_signaller(signaller).unwrap();

        ret
    }

    pub fn handle_sdp(
        &self,
        peer_id: &str,
        sdp: &gst_webrtc::WebRTCSessionDescription,
    ) -> Result<(), Error> {
        let ws = imp::WebRTCSink::from_instance(self);

        ws.handle_sdp(self, peer_id, sdp)
    }

    /// sdp_mid is exposed for future proofing, see
    /// https://gitlab.freedesktop.org/gstreamer/gst-plugins-bad/-/issues/1174,
    /// at the moment sdp_mline_index must be Some
    pub fn handle_ice(
        &self,
        peer_id: &str,
        sdp_mline_index: Option<u32>,
        sdp_mid: Option<String>,
        candidate: &str,
    ) -> Result<(), Error> {
        let ws = imp::WebRTCSink::from_instance(self);

        ws.handle_ice(self, peer_id, sdp_mline_index, sdp_mid, candidate)
    }

    pub fn handle_signalling_error(&self, error: anyhow::Error) {
        let ws = imp::WebRTCSink::from_instance(self);

        ws.handle_signalling_error(self, error);
    }

    pub fn add_consumer(&self, peer_id: &str) -> Result<(), Error> {
        let ws = imp::WebRTCSink::from_instance(self);

        ws.add_consumer(self, peer_id)
    }

    pub fn remove_consumer(&self, peer_id: &str) -> Result<(), Error> {
        let ws = imp::WebRTCSink::from_instance(self);

        ws.remove_consumer(self, peer_id, false)
    }
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstWebRTCSinkCongestionControl")]
pub enum WebRTCSinkCongestionControl {
    #[enum_value(name = "Disabled: no congestion control is applied", nick = "disabled")]
    Disabled,
    #[enum_value(name = "Homegrown: simple sender-side heuristic", nick = "homegrown")]
    Homegrown,
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "webrtcsink",
        gst::Rank::None,
        WebRTCSink::static_type(),
    )
}
