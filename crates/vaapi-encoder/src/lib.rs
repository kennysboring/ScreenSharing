//! H.264 Constrained Baseline encoding through VA-API, as validated in `spikes/encoder` on an
//! RX 580 (Mesa radeonsi, VCE).
//!
//! The GPU-free parts (bitstream headers, IDR scheduling, synthetic frames, the kernel lock)
//! always build and are tested by `cargo test`. The encoder itself is in [`vaapi`], behind the
//! `vaapi` feature, because it needs libva and libclang to build.

pub mod bitstream;
pub mod kernel;
pub mod schedule;
pub mod synthetic;

#[cfg(feature = "vaapi")]
pub mod vaapi;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// One NAL unit without its start code. SPS and PPS are not timed; slices are (they advance the
/// presentation clock), as the streamer-client's `EncodedFrame` expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nal {
    pub data: Vec<u8>,
    pub timed: bool,
}
