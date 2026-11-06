//! Spike v3: an XRGB8888 DMA-BUF (allocated by GBM, as a PipeWire capture would deliver it)
//! imported into VA-API, converted to NV12 by the VideoProc (VPP) and encoded, with no CPU copy
//! between the VPP and the encoder.
//!
//! - `--vpp-only`: import + one VPP conversion, then reads the NV12 back and checks every color
//!   bar (no encoder, nothing reaches the VCE);
//! - `--encode-vpp`: the same conversion feeding the IPPP encoder for every frame;
//! - `--psnr-bars <yuv>`: CPU only, PSNR of a decoded file against the pattern in BT.709 limited.

use crate::sequence::{self, SeqOptions};
use libva::{
    BufferType, Display, ExternalBufferDescriptor, Image, MemoryType, Picture, ProcColorProperties,
    ProcPipelineParameterBuffer, Surface, UsageHint, VADRMPRIMESurfaceDescriptor, VAEntrypoint,
    VAProfile,
};
use std::{
    fs::File,
    io::Read,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    rc::Rc,
    time::Instant,
};

type Error = Box<dyn std::error::Error>;

const RENDER_NODE: &str = "/dev/dri/renderD128";
/// DRM_FORMAT_XRGB8888 ('XR24'): 32-bit pixels, bytes in memory B, G, R, X.
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const GBM_BO_USE_LINEAR: u32 = 1 << 4;

// ---------------------------------------------------------------------------------------------
// The test pattern and its expected YUV (pure, CPU)
// ---------------------------------------------------------------------------------------------

/// Top 2/3: eight 100% color bars. Bottom 1/3: a gray ramp from black to white.
pub const BARS: [(&str, [u8; 3]); 8] = [
    ("white", [255, 255, 255]),
    ("yellow", [255, 255, 0]),
    ("cyan", [0, 255, 255]),
    ("green", [0, 255, 0]),
    ("magenta", [255, 0, 255]),
    ("red", [255, 0, 0]),
    ("blue", [0, 0, 255]),
    ("black", [0, 0, 0]),
];

/// RGB of pixel (`col`, `row`) of the pattern.
pub fn pattern_rgb(col: usize, row: usize, width: usize, height: usize) -> [u8; 3] {
    if row < height * 2 / 3 {
        BARS[(col * BARS.len() / width).min(BARS.len() - 1)].1
    } else {
        let gray = (col * 255 / (width - 1)) as u8;
        [gray, gray, gray]
    }
}

/// Matrix coefficients (Kr, Kb) of a color standard.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Matrix {
    Bt709,
    Bt601,
}

/// Y'CbCr of an R'G'B' color. `limited`: 16-235 / 16-240 (TV range), else 0-255.
pub fn rgb_to_ycbcr(rgb: [u8; 3], matrix: Matrix, limited: bool) -> [f64; 3] {
    let (kr, kb) = match matrix {
        Matrix::Bt709 => (0.2126, 0.0722),
        Matrix::Bt601 => (0.299, 0.114),
    };
    let [r, g, b] = rgb.map(|c| f64::from(c) / 255.0);
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    let cb = (b - y) / (2.0 * (1.0 - kb));
    let cr = (r - y) / (2.0 * (1.0 - kr));
    if limited {
        [16.0 + 219.0 * y, 128.0 + 224.0 * cb, 128.0 + 224.0 * cr]
    } else {
        [255.0 * y, 128.0 + 255.0 * cb, 128.0 + 255.0 * cr]
    }
}

/// Expected NV12 (BT.709, limited) of the pattern: luma per pixel, chroma per 2x2 block as the
/// average of its four pixels.
fn expected_planes(width: usize, height: usize) -> (Vec<u8>, Vec<[u8; 2]>) {
    let ycc = |col, row| rgb_to_ycbcr(pattern_rgb(col, row, width, height), Matrix::Bt709, true);
    let luma = (0..height)
        .flat_map(|row| (0..width).map(move |col| (col, row)))
        .map(|(col, row)| ycc(col, row)[0].round() as u8)
        .collect();
    let chroma = (0..height / 2)
        .flat_map(|row| (0..width / 2).map(move |col| (col, row)))
        .map(|(col, row)| {
            let mut sum = [0.0, 0.0];
            for (dc, dr) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let p = ycc(2 * col + dc, 2 * row + dr);
                sum[0] += p[1];
                sum[1] += p[2];
            }
            [(sum[0] / 4.0).round() as u8, (sum[1] / 4.0).round() as u8]
        })
        .collect();
    (luma, chroma)
}

fn psnr(sum_sq: f64, n: usize) -> f64 {
    if sum_sq == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / (sum_sq / n as f64)).log10()
    }
}

// ---------------------------------------------------------------------------------------------
// GBM: an XRGB8888 linear buffer filled by the CPU, exported as a DMA-BUF
// ---------------------------------------------------------------------------------------------

struct GbmBuffer {
    _file: File,
    device: *mut gbm_sys::gbm_device,
    bo: *mut gbm_sys::gbm_bo,
    stride: u32,
    modifier: u64,
}

impl GbmBuffer {
    fn allocate(width: u32, height: u32) -> Result<Self, Error> {
        let file = File::options()
            .read(true)
            .write(true)
            .open(RENDER_NODE)
            .map_err(|e| format!("open {RENDER_NODE}: {e}"))?;
        // SAFETY: `file` is kept in the struct and outlives the device.
        let device = unsafe { gbm_sys::gbm_create_device(file.as_raw_fd()) };
        if device.is_null() {
            return Err("gbm_create_device failed".into());
        }
        // SAFETY: `device` is valid; a null result is handled.
        let bo = unsafe {
            gbm_sys::gbm_bo_create(
                device,
                width,
                height,
                DRM_FORMAT_XRGB8888,
                GBM_BO_USE_LINEAR,
            )
        };
        if bo.is_null() {
            // SAFETY: `device` is valid and not used afterwards.
            unsafe { gbm_sys::gbm_device_destroy(device) };
            return Err("gbm_bo_create(XRGB8888, LINEAR) failed".into());
        }
        // SAFETY: `bo` is a valid buffer object.
        let (stride, modifier) = unsafe {
            (
                gbm_sys::gbm_bo_get_stride(bo),
                gbm_sys::gbm_bo_get_modifier(bo),
            )
        };
        Ok(Self {
            _file: file,
            device,
            bo,
            stride,
            modifier,
        })
    }

    /// Writes the test pattern through a CPU mapping (bytes B, G, R, X per pixel).
    fn fill_pattern(&self, width: u32, height: u32) -> Result<(), Error> {
        let mut stride = 0u32;
        let mut map_data = std::ptr::null_mut();
        // SAFETY: `bo` is valid; the mapping covers width x height and is unmapped below.
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
        if ptr.is_null() {
            return Err("gbm_bo_map failed".into());
        }
        // SAFETY: the mapping is `stride * height` bytes long and only used until the unmap.
        let data =
            unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), (stride * height) as usize) };
        for row in 0..height as usize {
            let line = &mut data[row * stride as usize..][..width as usize * 4];
            for (col, px) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let [r, g, b] = pattern_rgb(col, row, width as usize, height as usize);
                px.copy_from_slice(&[b, g, r, 0xff]);
            }
        }
        // SAFETY: `map_data` comes from the gbm_bo_map above.
        unsafe { gbm_sys::gbm_bo_unmap(self.bo, map_data) };
        Ok(())
    }

    /// A new DMA-BUF fd for the buffer (each call returns its own fd).
    fn export_fd(&self) -> Result<OwnedFd, Error> {
        // SAFETY: `bo` is valid; gbm_bo_get_fd returns a new fd or -1.
        let fd = unsafe { gbm_sys::gbm_bo_get_fd(self.bo) };
        if fd < 0 {
            return Err("gbm_bo_get_fd failed".into());
        }
        // SAFETY: the fd is new and owned by nobody else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

impl Drop for GbmBuffer {
    fn drop(&mut self) {
        // SAFETY: both were created in `allocate` and are not used after this.
        unsafe {
            gbm_sys::gbm_bo_destroy(self.bo);
            gbm_sys::gbm_device_destroy(self.device);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// VA-API: DRM PRIME import and the VPP
// ---------------------------------------------------------------------------------------------

/// An XRGB8888 linear DMA-BUF to import as a VA surface. The surface keeps it, so the fd stays
/// open as long as the surface exists.
pub struct DmaBufXrgb {
    fd: OwnedFd,
    width: u32,
    height: u32,
    stride: u32,
    modifier: u64,
}

impl ExternalBufferDescriptor for DmaBufXrgb {
    const MEMORY_TYPE: MemoryType = MemoryType::DrmPrime2;
    type DescriptorAttribute = VADRMPRIMESurfaceDescriptor;

    fn va_surface_attribute(&mut self) -> VADRMPRIMESurfaceDescriptor {
        let mut desc = VADRMPRIMESurfaceDescriptor {
            // VA's name for the same byte order as DRM XRGB8888: B, G, R, X
            fourcc: libva::VA_FOURCC_BGRX,
            width: self.width,
            height: self.height,
            num_objects: 1,
            num_layers: 1,
            ..Default::default()
        };
        desc.objects[0].fd = self.fd.as_raw_fd();
        desc.objects[0].size = self.stride * self.height;
        desc.objects[0].drm_format_modifier = self.modifier;
        desc.layers[0].drm_format = DRM_FORMAT_XRGB8888;
        desc.layers[0].num_planes = 1;
        desc.layers[0].object_index[0] = 0;
        desc.layers[0].offset[0] = 0;
        desc.layers[0].pitch[0] = self.stride;
        desc
    }
}

/// Allocates the GBM buffer, fills the pattern and imports it as a VA RGB32 surface.
fn import_pattern(
    display: &Rc<Display>,
    width: u32,
    height: u32,
) -> Result<(GbmBuffer, Surface<DmaBufXrgb>), Error> {
    let gbm = GbmBuffer::allocate(width, height)?;
    println!(
        "GBM: XRGB8888 {width}x{height}, stride {}, modifier 0x{:016x}{}",
        gbm.stride,
        gbm.modifier,
        if gbm.modifier == DRM_FORMAT_MOD_LINEAR {
            " (LINEAR)"
        } else {
            ""
        }
    );
    gbm.fill_pattern(width, height)?;
    let descriptor = DmaBufXrgb {
        fd: gbm.export_fd()?,
        width,
        height,
        stride: gbm.stride,
        modifier: gbm.modifier,
    };
    let mut surfaces = display
        .create_surfaces(
            libva::VA_RT_FORMAT_RGB32,
            Some(libva::VA_FOURCC_BGRX),
            width,
            height,
            Some(UsageHint::USAGE_HINT_VPP_READ),
            vec![descriptor],
        )
        .map_err(|e| format!("vaCreateSurfaces (DRM PRIME import of BGRX/XRGB8888): {e:?}"))?;
    let surface = surfaces
        .pop()
        .ok_or("vaCreateSurfaces returned no surface")?;
    println!("VA: imported as RGB32/BGRX surface {}", surface.id());
    Ok((gbm, surface))
}

/// The VideoProc pipeline converting RGB to NV12 BT.709 limited range.
///
/// Input and output surfaces must have the same size: the conversion maps the whole input onto
/// the whole output. Explicit regions do not work through cros-libva (at rev 59384456ac): its
/// Buffer keeps only the VA id and drops the parameter struct, so the region pointers inside
/// VAProcPipelineParameterBuffer dangle when Mesa reads them at vaRenderPicture; the result was
/// an empty output and no error.
pub struct Vpp {
    _config: libva::Config,
    context: Rc<libva::Context>,
}

impl Vpp {
    fn new(display: &Rc<Display>, width: u32, height: u32) -> Result<Self, Error> {
        let entrypoints = display
            .query_config_entrypoints(VAProfile::VAProfileNone)
            .map_err(|e| format!("vaQueryConfigEntrypoints(VAProfileNone): {e:?}"))?;
        if !entrypoints.contains(&VAEntrypoint::VAEntrypointVideoProc) {
            return Err("the driver has no VAEntrypointVideoProc".into());
        }
        let config = display
            .create_config(
                vec![],
                VAProfile::VAProfileNone,
                VAEntrypoint::VAEntrypointVideoProc,
            )
            .map_err(|e| format!("vaCreateConfig(VideoProc): {e:?}"))?;
        let context = display
            .create_context::<()>(&config, width, height, None, true)
            .map_err(|e| format!("vaCreateContext(VideoProc): {e:?}"))?;
        Ok(Self {
            _config: config,
            context,
        })
    }

    /// Converts `input` (RGB) into `output` (NV12) and waits for it.
    pub fn convert(
        &self,
        input: &Surface<DmaBufXrgb>,
        output: &Surface<()>,
        index: u32,
    ) -> Result<(), Error> {
        let params = ProcPipelineParameterBuffer::new(
            input.id(),
            None, // whole input surface (see the note on `Vpp`)
            // Explicit on both sides, with the same transfer: only the RGB to Y'CbCr matrix is
            // applied. With SRGB in and BT709 out, Mesa also converted gamma 2.2 to the BT.709
            // curve, which changed the mid-tones of the gray ramp.
            libva::_VAProcColorStandardType_VAProcColorStandardExplicit as u8,
            None, // whole output surface, the same size as the input
            0xff00_0000,
            libva::_VAProcColorStandardType_VAProcColorStandardExplicit as u8,
            0,
            0,
            None,
            None,
            None,
            0,
            None,
            0,
            None,
            0,
            0,
            // Input: full-range R'G'B', BT.709 primaries and transfer, matrix 0 (identity, RGB)
            ProcColorProperties::new(0, libva::VA_SOURCE_RANGE_FULL as u8, 1, 1, 0),
            // Output: BT.709 limited range (primaries 1, transfer 1, matrix 1)
            ProcColorProperties::new(0, libva::VA_SOURCE_RANGE_REDUCED as u8, 1, 1, 1),
            0,
            None,
        );
        let mut picture = Picture::new(u64::from(index), Rc::clone(&self.context), output);
        picture.add_buffer(
            self.context
                .create_buffer(BufferType::ProcPipelineParameter(params))
                .map_err(|e| format!("vaCreateBuffer(ProcPipelineParameter): {e:?}"))?,
        );
        let picture = picture
            .begin()
            .map_err(|e| format!("vaBeginPicture(VPP): {e:?}"))?
            .render()
            .map_err(|e| format!("vaRenderPicture(VPP): {e:?}"))?
            .end()
            .map_err(|e| format!("vaEndPicture(VPP): {e:?}"))?;
        picture
            .sync()
            .map_err(|(e, _)| format!("vaSyncSurface(VPP output): {e:?}"))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------------------------

/// Import + one VPP conversion, then reads the NV12 back on the CPU and checks every bar.
pub fn vpp_only(display: Rc<Display>, width: u32, height: u32) -> Result<(), Error> {
    let (_gbm, input) = import_pattern(&display, width, height)?;
    let mut outputs = display
        .create_surfaces(
            libva::VA_RT_FORMAT_YUV420,
            Some(libva::VA_FOURCC_NV12),
            width,
            height,
            Some(UsageHint::USAGE_HINT_VPP_WRITE),
            vec![()],
        )
        .map_err(|e| format!("vaCreateSurfaces(NV12 output): {e:?}"))?;
    let output = outputs.pop().ok_or("no NV12 surface")?;
    let vpp = Vpp::new(&display, width, height)?;

    let t = Instant::now();
    vpp.convert(&input, &output, 0)?;
    println!(
        "VPP: first conversion {:.2} ms",
        t.elapsed().as_secs_f64() * 1000.0
    );
    let mut times = Vec::new();
    for i in 1..=30 {
        let t = Instant::now();
        vpp.convert(&input, &output, i)?;
        times.push(t.elapsed());
    }
    times.sort();
    println!(
        "VPP: 30 more conversions, median {:.2} ms, worst {:.2} ms",
        times[15].as_secs_f64() * 1000.0,
        times[29].as_secs_f64() * 1000.0
    );

    let nv12 = display
        .query_image_formats()?
        .into_iter()
        .find(|f| f.fourcc == libva::VA_FOURCC_NV12)
        .ok_or("driver has no NV12 image format")?;
    let image = Image::create_from(&output, nv12, (width, height), (width, height))
        .map_err(|e| format!("reading the NV12 back (vaGetImage): {e:?}"))?;
    let va_image = *image.image();
    let (y_pitch, uv_pitch) = (va_image.pitches[0] as usize, va_image.pitches[1] as usize);
    let (y_off, uv_off) = (va_image.offsets[0] as usize, va_image.offsets[1] as usize);
    let data: &[u8] = image.as_ref();
    let (w, h) = (width as usize, height as usize);
    let luma_at = |col: usize, row: usize| data[y_off + row * y_pitch + col];
    let chroma_at = |col: usize, row: usize| {
        let i = uv_off + (row / 2) * uv_pitch + (col / 2) * 2;
        [data[i], data[i + 1]]
    };

    println!("\nbar        expected BT.709 limited     measured (Y, Cb, Cr)   closest model");
    let mut swapped = 0;
    let mut unexplained = 0;
    for (k, (name, rgb)) in BARS.iter().enumerate() {
        let (col, row) = (k * w / BARS.len() + w / BARS.len() / 2, h / 3);
        let measured = {
            let [cb, cr] = chroma_at(col, row);
            [f64::from(luma_at(col, row)), f64::from(cb), f64::from(cr)]
        };
        // Which interpretation explains the measurement best
        let mut models = Vec::new();
        for (matrix, mname) in [(Matrix::Bt709, "709"), (Matrix::Bt601, "601")] {
            for (limited, rname) in [(true, "limited"), (false, "full")] {
                for (swap, sname) in [(false, ""), (true, " R/B swapped")] {
                    let input = if swap { [rgb[2], rgb[1], rgb[0]] } else { *rgb };
                    let e = rgb_to_ycbcr(input, matrix, limited);
                    let err: f64 = (0..3).map(|i| (e[i] - measured[i]).abs()).sum();
                    models.push((err, format!("BT.{mname} {rname}{sname}")));
                }
            }
        }
        models.sort_by(|a, b| a.0.total_cmp(&b.0));
        if models[0].0 > 6.0 {
            unexplained += 1;
        } else if models[0].1.contains("swapped") && models[0].0 + 2.0 < models[1].0 {
            swapped += 1;
        }
        let e = rgb_to_ycbcr(*rgb, Matrix::Bt709, true).map(|v| v.round() as i32);
        let m = measured.map(|v| v as i32);
        println!(
            "{name:8}  ({:3}, {:3}, {:3})             ({:3}, {:3}, {:3})        {} (off by {:.0})",
            e[0], e[1], e[2], m[0], m[1], m[2], models[0].1, models[0].0
        );
    }

    // Whole-frame PSNR of the VPP output against the CPU-converted pattern
    let (luma, chroma) = expected_planes(w, h);
    // Mid-tones of the ramp show a transfer-curve conversion; the bars (0 and 255) cannot
    println!();
    for pct in [25, 50, 75] {
        let col = (w - 1) * pct / 100;
        let row = h * 5 / 6;
        println!(
            "ramp {pct:2}%: expected Y {:3}, measured {:3}",
            luma[row * w + col],
            luma_at(col, row)
        );
    }

    let (mut sum_y, mut sum_c) = (0.0, 0.0);
    let (mut sum_bars, mut sum_ramp) = (0.0, 0.0);
    for row in 0..h {
        for col in 0..w {
            let d = (f64::from(luma_at(col, row)) - f64::from(luma[row * w + col])).powi(2);
            sum_y += d;
            if row < h * 2 / 3 {
                sum_bars += d;
            } else {
                sum_ramp += d;
            }
        }
    }
    for row in 0..h / 2 {
        for col in 0..w / 2 {
            let got = chroma_at(2 * col, 2 * row);
            let want = chroma[row * (w / 2) + col];
            sum_c += (f64::from(got[0]) - f64::from(want[0])).powi(2)
                + (f64::from(got[1]) - f64::from(want[1])).powi(2);
        }
    }
    println!(
        "\nVPP output vs BT.709 limited (CPU): PSNR Y {:.2} dB (bars {:.2}, ramp {:.2}), CbCr {:.2} dB",
        psnr(sum_y, w * h),
        psnr(sum_bars, w * (h * 2 / 3)),
        psnr(sum_ramp, w * (h - h * 2 / 3)),
        psnr(sum_c, w * h / 2)
    );
    if unexplained > 0 {
        return Err(format!(
            "{unexplained} bar(s) match no color model: the VPP output is wrong or empty"
        )
        .into());
    }
    println!(
        "R/B check: {}",
        if swapped == 0 {
            "red and blue are where they should be"
        } else {
            "RED AND BLUE ARE SWAPPED"
        }
    );
    Ok(())
}

/// The IPPP encoder fed by the VPP: every frame converts the imported buffer into the encoder's
/// NV12 input surface (no CPU copy), with BT.709 limited declared in the SPS VUI.
pub fn encode_vpp(display: Rc<Display>, opts: &SeqOptions) -> Result<(), Error> {
    let (_gbm, input) = import_pattern(&display, opts.width, opts.height)?;
    // The encoder's NV12 input surface is width x height, the same size as the import
    let vpp = Vpp::new(&display, opts.width, opts.height)?;
    let mut convert = |index: u32, output: &Surface<()>| vpp.convert(&input, output, index);
    sequence::encode_sequence_with(
        Rc::clone(&display),
        opts,
        &mut convert,
        "VPP (RGB to NV12)",
        "out-vpp.h264",
    )
}

/// CPU only: PSNR of every frame of a decoded yuv420p file against the pattern in BT.709
/// limited range, and the worst bar's chroma error.
pub fn verify_bars_psnr(path: &str, width: u32, height: u32, frames: u32) -> Result<(), Error> {
    let (w, h) = (width as usize, height as usize);
    let (luma, chroma) = expected_planes(w, h);
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; w * h * 3 / 2];
    let (mut min_y, mut min_c, mut sum_y_db) = (f64::INFINITY, f64::INFINITY, 0.0);
    for index in 0..frames {
        file.read_exact(&mut buf)
            .map_err(|e| format!("frame {index}: {e}"))?;
        let sum_y: f64 = (0..w * h)
            .map(|i| (f64::from(buf[i]) - f64::from(luma[i])).powi(2))
            .sum();
        let (u, v) = (&buf[w * h..w * h * 5 / 4], &buf[w * h * 5 / 4..]);
        let sum_c: f64 = (0..w * h / 4)
            .map(|i| {
                (f64::from(u[i]) - f64::from(chroma[i][0])).powi(2)
                    + (f64::from(v[i]) - f64::from(chroma[i][1])).powi(2)
            })
            .sum();
        let (py, pc) = (psnr(sum_y, w * h), psnr(sum_c, w * h / 2));
        min_y = min_y.min(py);
        min_c = min_c.min(pc);
        sum_y_db += py;
        if index < 3 || index + 1 == frames {
            println!("frame {index:3}: PSNR Y {py:.2} dB, CbCr {pc:.2} dB");
        }
    }
    println!(
        "{frames} frames: luma min {min_y:.2} dB, mean {:.2} dB; chroma min {min_c:.2} dB",
        sum_y_db / f64::from(frames)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bt709_limited_reference_values() {
        // White, black, red and blue in BT.709 limited range (ITU-R BT.709 / common tables)
        let round = |v: [f64; 3]| v.map(|x| x.round() as i32);
        assert_eq!(
            round(rgb_to_ycbcr([255, 255, 255], Matrix::Bt709, true)),
            [235, 128, 128]
        );
        assert_eq!(
            round(rgb_to_ycbcr([0, 0, 0], Matrix::Bt709, true)),
            [16, 128, 128]
        );
        assert_eq!(
            round(rgb_to_ycbcr([255, 0, 0], Matrix::Bt709, true)),
            [63, 102, 240]
        );
        assert_eq!(
            round(rgb_to_ycbcr([0, 0, 255], Matrix::Bt709, true)),
            [32, 240, 118]
        );
    }

    #[test]
    fn test_pattern_layout() {
        assert_eq!(pattern_rgb(0, 0, 1920, 1080), [255, 255, 255]);
        assert_eq!(pattern_rgb(240 * 5 + 10, 100, 1920, 1080), [255, 0, 0]);
        assert_eq!(pattern_rgb(240 * 6 + 10, 100, 1920, 1080), [0, 0, 255]);
        assert_eq!(pattern_rgb(0, 1079, 1920, 1080), [0, 0, 0]);
        assert_eq!(pattern_rgb(1919, 1079, 1920, 1080), [255, 255, 255]);
    }
}
