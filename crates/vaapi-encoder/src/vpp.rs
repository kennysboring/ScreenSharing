//! Capture frames into the encoder: an XRGB8888 DMA-BUF imported as a VA surface (DRM PRIME 2)
//! and the VideoProc (VPP) converting it to NV12 BT.709 limited range, straight into
//! [`crate::vaapi::Encoder::input_surface`] with no CPU copy. As validated in spikes/encoder
//! (`--vpp-only`, `--encode-vpp`) on radeonsi. The VPP runs on the GPU's graphics/compute
//! queues, not on the VCE.

use crate::Error;
use libva::{
    BufferType, Config, Context, Display, ExternalBufferDescriptor, MemoryType, Picture,
    ProcColorProperties, ProcPipelineParameterBuffer, Surface, UsageHint,
    VADRMPRIMESurfaceDescriptor, VAEntrypoint, VAProfile,
};
use std::{
    os::fd::{AsRawFd, OwnedFd},
    rc::Rc,
};

/// DRM_FORMAT_XRGB8888 ('XR24'): 32-bit pixels, bytes B, G, R, X in memory. PipeWire calls it
/// BGRx, VA calls it BGRX.
pub const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Layout of a single-plane XRGB8888 DMA-BUF, as the capture describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaBufLayout {
    pub width: u32,
    pub height: u32,
    /// Bytes per row
    pub stride: u32,
    /// Where the first row starts in the buffer
    pub offset: u32,
    /// Size of the whole buffer
    pub size: u32,
    /// Only LINEAR: the only one tested, and what the capture negotiates
    pub modifier: u64,
}

impl DmaBufLayout {
    fn validate(&self) -> Result<(), Error> {
        if self.width == 0 || self.height == 0 {
            return Err(format!("DMA-BUF of {}x{}", self.width, self.height).into());
        }
        if self.modifier != DRM_FORMAT_MOD_LINEAR {
            return Err(format!(
                "DMA-BUF modifier 0x{:016x}: only LINEAR is supported",
                self.modifier
            )
            .into());
        }
        if u64::from(self.stride) < 4 * u64::from(self.width) {
            return Err(format!(
                "DMA-BUF stride {} is too small for {} XRGB8888 pixels",
                self.stride, self.width
            )
            .into());
        }
        let end = u64::from(self.offset) + u64::from(self.stride) * u64::from(self.height);
        if end > u64::from(self.size) {
            return Err(format!(
                "DMA-BUF of {} bytes is too small: {}x{} rows of {} from offset {} need {end}",
                self.size, self.width, self.height, self.stride, self.offset
            )
            .into());
        }
        Ok(())
    }
}

/// An XRGB8888 DMA-BUF to import. The surface made from it owns the fd, so it stays open as
/// long as the surface exists.
pub struct DmaBufXrgb {
    fd: OwnedFd,
    layout: DmaBufLayout,
}

impl DmaBufXrgb {
    /// `fd`: a descriptor of its own (for a PipeWire buffer, a dup of the buffer's fd).
    pub fn new(fd: OwnedFd, layout: DmaBufLayout) -> Result<Self, Error> {
        layout.validate()?;
        Ok(Self { fd, layout })
    }
}

impl ExternalBufferDescriptor for DmaBufXrgb {
    const MEMORY_TYPE: MemoryType = MemoryType::DrmPrime2;
    type DescriptorAttribute = VADRMPRIMESurfaceDescriptor;

    fn va_surface_attribute(&mut self) -> VADRMPRIMESurfaceDescriptor {
        let l = self.layout;
        let mut desc = VADRMPRIMESurfaceDescriptor {
            fourcc: libva::VA_FOURCC_BGRX,
            width: l.width,
            height: l.height,
            num_objects: 1,
            num_layers: 1,
            ..Default::default()
        };
        desc.objects[0].fd = self.fd.as_raw_fd();
        desc.objects[0].size = l.size;
        desc.objects[0].drm_format_modifier = l.modifier;
        desc.layers[0].drm_format = DRM_FORMAT_XRGB8888;
        desc.layers[0].num_planes = 1;
        desc.layers[0].object_index[0] = 0;
        desc.layers[0].offset[0] = l.offset;
        desc.layers[0].pitch[0] = l.stride;
        desc
    }
}

/// Imports `buffer` as a VA RGB32 surface the VPP can read. Done once per capture buffer: the
/// surface follows the buffer's content, so later frames in it need no new import.
pub fn import(display: &Rc<Display>, buffer: DmaBufXrgb) -> Result<Surface<DmaBufXrgb>, Error> {
    let (width, height) = (buffer.layout.width, buffer.layout.height);
    display
        .create_surfaces(
            libva::VA_RT_FORMAT_RGB32,
            Some(libva::VA_FOURCC_BGRX),
            width,
            height,
            Some(UsageHint::USAGE_HINT_VPP_READ),
            vec![buffer],
        )
        .map_err(|e| format!("importing the XRGB8888 DMA-BUF (vaCreateSurfaces): {e:?}"))?
        .pop()
        .ok_or_else(|| "vaCreateSurfaces returned no surface".into())
}

/// The VideoProc pipeline converting RGB to NV12 BT.709 limited range.
///
/// Input and output must have the same size: the whole input maps onto the whole output.
/// Explicit regions do not work through cros-libva (rev 59384456ac): its Buffer keeps only the
/// VA id and drops the parameter struct, so the region pointers inside
/// VAProcPipelineParameterBuffer dangle when Mesa reads them at vaRenderPicture, and the output
/// comes out empty with no error.
pub struct Vpp {
    _config: Config,
    context: Rc<Context>,
    pictures: u64,
}

impl Vpp {
    pub fn new(display: &Rc<Display>, width: u32, height: u32) -> Result<Self, Error> {
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
            pictures: 0,
        })
    }

    /// Converts `input` (XRGB8888) into `output` (NV12) and waits until it is written.
    pub fn convert(
        &mut self,
        input: &Surface<DmaBufXrgb>,
        output: &Surface<()>,
    ) -> Result<(), Error> {
        if input.size() != output.size() {
            return Err(format!(
                "VPP input {:?} and output {:?} differ in size (regions are not supported)",
                input.size(),
                output.size()
            )
            .into());
        }
        let params = ProcPipelineParameterBuffer::new(
            input.id(),
            None, // whole input surface (see the note on `Vpp`)
            // Explicit on both sides with the same transfer, so only the RGB to Y'CbCr matrix
            // is applied: with SRGB in and BT709 out, Mesa also converted gamma 2.2 to the
            // BT.709 curve and the mid-tones changed
            libva::_VAProcColorStandardType_VAProcColorStandardExplicit as u8,
            None, // whole output surface
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
            // Input: full-range R'G'B', BT.709 primaries and transfer, matrix 0 (RGB)
            ProcColorProperties::new(0, libva::VA_SOURCE_RANGE_FULL as u8, 1, 1, 0),
            // Output: BT.709 limited range, as the SPS VUI declares (bt709_vui)
            ProcColorProperties::new(0, libva::VA_SOURCE_RANGE_REDUCED as u8, 1, 1, 1),
            0,
            None,
        );
        let mut picture = Picture::new(self.pictures, Rc::clone(&self.context), output);
        self.pictures += 1;
        picture.add_buffer(
            self.context
                .create_buffer(BufferType::ProcPipelineParameter(params))
                .map_err(|e| format!("vaCreateBuffer(ProcPipelineParameter): {e:?}"))?,
        );
        picture
            .begin()
            .map_err(|e| format!("vaBeginPicture(VPP): {e:?}"))?
            .render()
            .map_err(|e| format!("vaRenderPicture(VPP): {e:?}"))?
            .end()
            .map_err(|e| format!("vaEndPicture(VPP): {e:?}"))?
            .sync()
            .map_err(|(e, _)| format!("vaSyncSurface(VPP output): {e:?}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn layout() -> DmaBufLayout {
        DmaBufLayout {
            width: 1920,
            height: 1080,
            stride: 7680,
            offset: 0,
            size: 7680 * 1080,
            modifier: DRM_FORMAT_MOD_LINEAR,
        }
    }

    fn any_fd() -> OwnedFd {
        File::open("/dev/null").unwrap().into()
    }

    #[test]
    fn test_layout_validation() {
        assert!(layout().validate().is_ok());
        let bad = [
            DmaBufLayout {
                width: 0,
                ..layout()
            },
            DmaBufLayout {
                modifier: 0x0200_0000_0000_0001, // an AMD tiled modifier
                ..layout()
            },
            DmaBufLayout {
                stride: 4 * 1920 - 4,
                ..layout()
            },
            DmaBufLayout {
                offset: 4096, // the last row would run past the end
                ..layout()
            },
            DmaBufLayout {
                // stride * height overflows u32: must not wrap into a "valid" size
                stride: u32::MAX,
                ..layout()
            },
        ];
        for l in bad {
            assert!(l.validate().is_err(), "{l:?}");
        }
    }

    #[test]
    fn test_descriptor_describes_one_xrgb_plane() {
        let fd = any_fd();
        let raw = fd.as_raw_fd();
        let l = DmaBufLayout {
            stride: 8192,
            offset: 256,
            size: 256 + 8192 * 1080,
            ..layout()
        };
        let mut buffer = DmaBufXrgb::new(fd, l).unwrap();
        let desc = buffer.va_surface_attribute();
        assert_eq!(desc.fourcc, libva::VA_FOURCC_BGRX);
        assert_eq!((desc.width, desc.height), (1920, 1080));
        assert_eq!((desc.num_objects, desc.num_layers), (1, 1));
        assert_eq!(desc.objects[0].fd, raw);
        assert_eq!(desc.objects[0].size, l.size);
        assert_eq!(desc.objects[0].drm_format_modifier, DRM_FORMAT_MOD_LINEAR);
        let layer = &desc.layers[0];
        assert_eq!(layer.drm_format, DRM_FORMAT_XRGB8888);
        assert_eq!(layer.num_planes, 1);
        assert_eq!(
            (layer.object_index[0], layer.offset[0], layer.pitch[0]),
            (0, 256, 8192)
        );
    }

    #[test]
    fn test_invalid_layout_is_rejected_before_import() {
        let l = DmaBufLayout {
            size: 100,
            ..layout()
        };
        assert!(DmaBufXrgb::new(any_fd(), l).is_err());
    }
}
