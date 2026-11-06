//! GPU test of the capture path up to the encoder input: a GBM XRGB8888 LINEAR buffer (what
//! PipeWire delivers), imported with an explicit LINEAR modifier and converted by the VPP into
//! an NV12 surface, then read back and compared with BT.709 limited range. Nothing reaches the
//! VCE (no encoder is created), so it does not depend on the kernel lock.
//!
//! `cargo test -p vaapi-encoder --features gpu-tests -- --ignored`
#![cfg(feature = "gpu-tests")]

use libva::{Display, Image, UsageHint};
use std::{
    fs::File,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};
use vaapi_encoder::{
    synthetic::rgb_to_bt709_limited,
    vpp::{self, DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888, DmaBufLayout, DmaBufXrgb, Vpp},
};

const RENDER_NODE: &str = "/dev/dri/renderD128";
const GBM_BO_USE_LINEAR: u32 = 1 << 4;
const BARS: [[u8; 3]; 8] = [
    [255, 255, 255],
    [255, 255, 0],
    [0, 255, 255],
    [0, 255, 0],
    [255, 0, 255],
    [255, 0, 0],
    [0, 0, 255],
    [0, 0, 0],
];

/// A GBM buffer object with its device; destroyed on drop.
struct Gbm {
    _file: File,
    device: *mut gbm_sys::gbm_device,
    bo: *mut gbm_sys::gbm_bo,
}

impl Gbm {
    fn xrgb_linear(width: u32, height: u32) -> Self {
        let file = File::options()
            .read(true)
            .write(true)
            .open(RENDER_NODE)
            .unwrap();
        // SAFETY: `file` outlives the device (both in the struct, file dropped last).
        let device = unsafe { gbm_sys::gbm_create_device(file.as_raw_fd()) };
        assert!(!device.is_null(), "gbm_create_device");
        // SAFETY: `device` is valid; a null result is checked.
        let bo = unsafe {
            gbm_sys::gbm_bo_create(
                device,
                width,
                height,
                DRM_FORMAT_XRGB8888,
                GBM_BO_USE_LINEAR,
            )
        };
        assert!(!bo.is_null(), "gbm_bo_create(XRGB8888, LINEAR)");
        Self {
            _file: file,
            device,
            bo,
        }
    }

    /// Fills eight vertical color bars through a CPU mapping; returns the stride.
    fn fill_bars(&self, width: u32, height: u32) -> u32 {
        let (mut stride, mut map_data) = (0u32, std::ptr::null_mut());
        // SAFETY: `bo` is valid; the mapping is unmapped below and not used after.
        let ptr = unsafe {
            gbm_sys::gbm_bo_map(
                self.bo,
                0,
                0,
                width,
                height,
                gbm_sys::gbm_bo_transfer_flags::GBM_BO_TRANSFER_WRITE,
                &mut stride,
                &mut map_data,
            )
        };
        assert!(!ptr.is_null(), "gbm_bo_map");
        // SAFETY: the mapping is `stride * height` bytes long.
        let data =
            unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), (stride * height) as usize) };
        for row in 0..height as usize {
            let line = &mut data[row * stride as usize..][..width as usize * 4];
            for (col, px) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let [r, g, b] = BARS[col * BARS.len() / width as usize];
                px.copy_from_slice(&[b, g, r, 0xff]);
            }
        }
        // SAFETY: `map_data` comes from the gbm_bo_map above.
        unsafe { gbm_sys::gbm_bo_unmap(self.bo, map_data) };
        stride
    }

    fn export_fd(&self) -> OwnedFd {
        // SAFETY: `bo` is valid; gbm_bo_get_fd returns a new fd or -1.
        let fd = unsafe { gbm_sys::gbm_bo_get_fd(self.bo) };
        assert!(fd >= 0, "gbm_bo_get_fd");
        // SAFETY: the fd is new and owned by nobody else.
        unsafe { OwnedFd::from_raw_fd(fd) }
    }
}

impl Drop for Gbm {
    fn drop(&mut self) {
        // SAFETY: both come from `xrgb_linear` and are not used after this.
        unsafe {
            gbm_sys::gbm_bo_destroy(self.bo);
            gbm_sys::gbm_device_destroy(self.device);
        }
    }
}

#[test]
#[ignore = "needs the GPU (VPP only, no encode): run with --features gpu-tests -- --ignored"]
fn test_vpp_converts_a_linear_xrgb_dmabuf_to_bt709_limited_nv12() {
    let (width, height) = (1920u32, 1080u32);
    let gbm = Gbm::xrgb_linear(width, height);
    let stride = gbm.fill_bars(width, height);
    let layout = DmaBufLayout {
        width,
        height,
        stride,
        offset: 0,
        size: stride * height,
        // Explicit LINEAR, as the capture negotiates it (GBM itself reports INVALID here)
        modifier: DRM_FORMAT_MOD_LINEAR,
    };

    let display = Display::open().expect("VA display");
    let input = vpp::import(&display, DmaBufXrgb::new(gbm.export_fd(), layout).unwrap()).unwrap();
    // Like the encoder's input surface: NV12 at the picture size, written by the VPP
    let output = display
        .create_surfaces(
            libva::VA_RT_FORMAT_YUV420,
            Some(libva::VA_FOURCC_NV12),
            width,
            height,
            Some(UsageHint::USAGE_HINT_ENCODER | UsageHint::USAGE_HINT_VPP_WRITE),
            vec![()],
        )
        .unwrap()
        .pop()
        .unwrap();
    let mut vpp = Vpp::new(&display, width, height).unwrap();
    vpp.convert(&input, &output).unwrap();
    // A second conversion through the same import, as every captured frame will do
    vpp.convert(&input, &output).unwrap();

    let nv12 = display
        .query_image_formats()
        .unwrap()
        .into_iter()
        .find(|f| f.fourcc == libva::VA_FOURCC_NV12)
        .unwrap();
    let image = Image::create_from(&output, nv12, (width, height), (width, height)).unwrap();
    let va_image = *image.image();
    let (pitches, offsets) = (va_image.pitches, va_image.offsets);
    let data: &[u8] = image.as_ref();

    let bar_width = width as usize / BARS.len();
    for (k, rgb) in BARS.iter().enumerate() {
        // Center of the bar, away from the chroma filter at the edges
        let (col, row) = (k * bar_width + bar_width / 2, height as usize / 2);
        let y = data[offsets[0] as usize + row * pitches[0] as usize + col];
        let c = offsets[1] as usize + (row / 2) * pitches[1] as usize + (col / 2) * 2;
        let measured = [y, data[c], data[c + 1]].map(f64::from);
        let expected = rgb_to_bt709_limited(*rgb);
        for i in 0..3 {
            assert!(
                (measured[i] - expected[i]).abs() <= 2.0,
                "bar {k} {rgb:?}: measured {measured:?}, expected {expected:?}"
            );
        }
    }
}
