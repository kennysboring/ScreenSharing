//! The VA-API encoder (feature `vaapi`): H.264 Constrained Baseline, IPPP with one reference,
//! CQP, packed SPS/PPS/slice headers so the driver writes SPS/PPS in front of every IDR.
//!
//! Only runs on kernel 6.18 (see [`crate::kernel`]): on 7.2.2+ H.264 encode hangs the RX 580's
//! VCE.

use crate::{
    Error, Nal,
    bitstream::{
        MAX_FRAME_NUM, QP, build_headers, level_for, nal_type, pps_bytes, slice_header, split_nals,
        sps_bytes,
    },
    kernel,
    schedule::IdrScheduler,
    synthetic,
};
use cros_codecs::codec::h264::parser::{Pps, Sps};
use libva::{
    BufferType, Config as VaConfig, Context, Display, EncCodedBuffer, EncPackedHeaderParameter,
    EncPackedHeaderType, EncPictureParameter, EncPictureParameterBufferH264, EncSequenceParameter,
    EncSequenceParameterBufferH264, EncSliceParameter, EncSliceParameterBufferH264,
    H264EncFrameCropOffsets, H264EncPicFields, H264EncSeqFields, Image, MappedCodedBuffer, Picture,
    PictureH264, Surface, UsageHint, VAConfigAttrib, VAConfigAttribType, VAEntrypoint,
    VAImageFormat, VAProfile,
};
use std::{
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const PROFILE: VAProfile::Type = VAProfile::VAProfileH264ConstrainedBaseline;
const ENTRYPOINT: VAEntrypoint::Type = VAEntrypoint::VAEntrypointEncSlice;
const PACKED_HEADERS: u32 = libva::VA_ENC_PACKED_HEADER_SEQUENCE
    | libva::VA_ENC_PACKED_HEADER_PICTURE
    | libva::VA_ENC_PACKED_HEADER_SLICE;
const SLICE_TYPE_P: u8 = 0;
const SLICE_TYPE_I: u8 = 2;

#[derive(Debug, Clone)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Periodic IDR every this many frames; at most [`MAX_FRAME_NUM`] so frame_num never wraps
    /// inside a GOP (that case was not tested on the VCE).
    pub gop_frames: u32,
    /// Requested IDRs closer than this to the previous IDR wait until it has passed.
    pub min_forced_idr_interval: Duration,
}

impl Config {
    fn validate(&self) -> Result<(), Error> {
        if self.width == 0 || self.height == 0 || self.fps == 0 {
            return Err("width, height and fps must be > 0".into());
        }
        if !(1..=MAX_FRAME_NUM).contains(&self.gop_frames) {
            return Err(format!("gop_frames must be 1..={MAX_FRAME_NUM}").into());
        }
        Ok(())
    }
}

fn invalid_pic() -> PictureH264 {
    PictureH264::new(
        libva::VA_INVALID_ID,
        0,
        libva::VA_PICTURE_H264_INVALID,
        0,
        0,
    )
}

fn reference_pic(surface: &Surface<()>, frame_num: u32) -> PictureH264 {
    let poc = 2 * frame_num as i32; // POC type 2: derived from frame_num
    PictureH264::new(
        surface.id(),
        frame_num,
        libva::VA_PICTURE_H264_SHORT_TERM_REFERENCE,
        poc,
        poc,
    )
}

/// An open VA-API H.264 encoder. Not `Send` (libva handles are reference counted with `Rc`):
/// create and use it on one thread.
pub struct Encoder {
    config: Config,
    _va_config: VaConfig,
    context: Rc<Context>,
    /// [0] input, [1] and [2] reconstructions that alternate: current and reference
    surfaces: Vec<Surface<()>>,
    coded: EncCodedBuffer,
    nv12: VAImageFormat,
    sps: Rc<Sps>,
    pps: Rc<Pps>,
    mb_w: u32,
    mb_h: u32,
    recon_index: usize,
    frame_num: u32,
    idr_pic_id: u32,
    encoded_idrs: u32,
    /// Our SPS/PPS as NALs, used only if the driver returns an IDR without them
    fallback_headers: Vec<Vec<u8>>,
}

impl Encoder {
    pub fn open(config: &Config) -> Result<Self, Error> {
        config.validate()?;
        let release = kernel::running_release()?;
        if !kernel::encode_allowed(&release) {
            return Err(format!(
                "kernel {release}: VA-API encode is only allowed on {}x (H.264 encode hangs \
                 the VCE on 7.2.2+)",
                kernel::SAFE_KERNEL_PREFIX
            )
            .into());
        }

        let display = Display::open().ok_or("could not open a VA-API display")?;
        if !display.query_config_profiles()?.contains(&PROFILE)
            || !display
                .query_config_entrypoints(PROFILE)?
                .contains(&ENTRYPOINT)
        {
            return Err("the driver has no H.264 Constrained Baseline encoder".into());
        }
        let mut attrs = [
            VAConfigAttrib {
                type_: VAConfigAttribType::VAConfigAttribRateControl,
                value: 0,
            },
            VAConfigAttrib {
                type_: VAConfigAttribType::VAConfigAttribEncPackedHeaders,
                value: 0,
            },
        ];
        display.get_config_attributes(PROFILE, ENTRYPOINT, &mut attrs)?;
        if attrs[0].value & libva::VA_RC_CQP == 0 {
            return Err("the driver does not offer CQP rate control".into());
        }
        if attrs[1].value == libva::VA_ATTRIB_NOT_SUPPORTED
            || attrs[1].value & PACKED_HEADERS != PACKED_HEADERS
        {
            return Err("the driver does not accept packed SPS/PPS/slice headers".into());
        }

        let (mb_w, mb_h) = (config.width.div_ceil(16), config.height.div_ceil(16));
        let (coded_w, coded_h) = (mb_w * 16, mb_h * 16);
        let level = level_for(config.width, config.height, config.fps);
        let (sps, pps) = build_headers(config.width, config.height, level)?;

        let va_config = display.create_config(
            vec![
                VAConfigAttrib {
                    type_: VAConfigAttribType::VAConfigAttribRTFormat,
                    value: libva::VA_RT_FORMAT_YUV420,
                },
                VAConfigAttrib {
                    type_: VAConfigAttribType::VAConfigAttribRateControl,
                    value: libva::VA_RC_CQP,
                },
                VAConfigAttrib {
                    type_: VAConfigAttribType::VAConfigAttribEncPackedHeaders,
                    value: PACKED_HEADERS,
                },
            ],
            PROFILE,
            ENTRYPOINT,
        )?;
        let surfaces = display.create_surfaces(
            libva::VA_RT_FORMAT_YUV420,
            Some(libva::VA_FOURCC_NV12),
            coded_w,
            coded_h,
            Some(UsageHint::USAGE_HINT_ENCODER),
            vec![(), (), ()],
        )?;
        let context =
            display.create_context(&va_config, coded_w, coded_h, Some(&surfaces), true)?;
        let coded = context.create_enc_coded((coded_w * coded_h * 3 / 2) as usize)?;
        let nv12 = display
            .query_image_formats()?
            .into_iter()
            .find(|f| f.fourcc == libva::VA_FOURCC_NV12)
            .ok_or("the driver has no NV12 image format")?;

        let mut headers = sps_bytes(&sps)?;
        headers.extend(pps_bytes(&pps)?);
        let fallback_headers = split_nals(&headers)
            .into_iter()
            .map(<[u8]>::to_vec)
            .collect();

        Ok(Self {
            config: config.clone(),
            _va_config: va_config,
            context,
            surfaces,
            coded,
            nv12,
            sps,
            pps,
            mb_w,
            mb_h,
            recon_index: 1,
            frame_num: 0,
            idr_pic_id: 0,
            encoded_idrs: 0,
            fallback_headers,
        })
    }

    fn coded_size(&self) -> (u32, u32) {
        (self.mb_w * 16, self.mb_h * 16)
    }

    /// Writes synthetic frame `index` into the input surface.
    pub fn upload_synthetic(&mut self, index: u32) -> Result<(), Error> {
        let (coded_w, coded_h) = self.coded_size();
        let input = &self.surfaces[0];
        {
            let mut image =
                Image::create_from(input, self.nv12, (coded_w, coded_h), (coded_w, coded_h))?;
            let va_image = *image.image();
            synthetic::fill_nv12(
                image.as_mut(),
                [va_image.pitches[0] as usize, va_image.pitches[1] as usize],
                [va_image.offsets[0] as usize, va_image.offsets[1] as usize],
                coded_w as usize,
                coded_h as usize,
                index as usize,
            );
        } // dropping the Image writes it back when it was not derived
        input.sync()?;
        Ok(())
    }

    fn seq_param(&self) -> EncSequenceParameterBufferH264 {
        let sps = &self.sps;
        let seq_fields = H264EncSeqFields::new(
            u32::from(sps.chroma_format_idc),
            u32::from(sps.frame_mbs_only_flag),
            0,
            0,
            u32::from(sps.direct_8x8_inference_flag),
            u32::from(sps.log2_max_frame_num_minus4),
            u32::from(sps.pic_order_cnt_type),
            u32::from(sps.log2_max_pic_order_cnt_lsb_minus4),
            0,
        );
        let crop = sps.frame_cropping_flag.then(|| {
            H264EncFrameCropOffsets::new(
                sps.frame_crop_left_offset,
                sps.frame_crop_right_offset,
                sps.frame_crop_top_offset,
                sps.frame_crop_bottom_offset,
            )
        });
        EncSequenceParameterBufferH264::new(
            sps.seq_parameter_set_id,
            sps.level_idc as u8,
            self.config.gop_frames, // intra_period
            self.config.gop_frames, // intra_idr_period
            1,                      // ip_period: no B-frames
            0,                      // bits_per_second: unused with CQP
            u32::from(sps.max_num_ref_frames),
            self.mb_w as u16,
            self.mb_h as u16,
            &seq_fields,
            0,
            0,
            0,
            0,
            0,
            [0; 256],
            crop,
            None,
            0,
            0,
            0,
            0,
            0,
        )
    }

    /// Encodes the frame in the input surface. P-frames refer to the previous frame's
    /// reconstruction. Returns its NALs without start codes, SPS and PPS first for an IDR.
    pub fn encode(&mut self, index: u32, idr: bool) -> Result<Vec<Nal>, Error> {
        if idr {
            self.frame_num = 0;
            if self.encoded_idrs > 0 {
                self.idr_pic_id = (self.idr_pic_id + 1) % 65536;
            }
            self.encoded_idrs += 1;
        } else {
            self.frame_num = (self.frame_num + 1) % MAX_FRAME_NUM;
        }
        let reference_index = 3 - self.recon_index;
        let (recon, reference) = (
            &self.surfaces[self.recon_index],
            &self.surfaces[reference_index],
        );
        let (sps, pps) = (&self.sps, &self.pps);
        let poc = 2 * self.frame_num as i32;
        let reference_frame_num = (self.frame_num + MAX_FRAME_NUM - 1) % MAX_FRAME_NUM;

        let mut reference_frames: [PictureH264; 16] = std::array::from_fn(|_| invalid_pic());
        let mut ref_list_0: [PictureH264; 32] = std::array::from_fn(|_| invalid_pic());
        if !idr {
            reference_frames[0] = reference_pic(reference, reference_frame_num);
            ref_list_0[0] = reference_pic(reference, reference_frame_num);
        }
        let pic_fields = H264EncPicFields::new(
            u32::from(idr),
            1, // every frame is the reference of the next one
            u32::from(pps.entropy_coding_mode_flag),
            0,
            0,
            0,
            0,
            u32::from(pps.deblocking_filter_control_present_flag),
            0,
            0,
            0,
        );
        let pic = EncPictureParameterBufferH264::new(
            PictureH264::new(recon.id(), self.frame_num, 0, poc, poc),
            reference_frames,
            self.coded.id(),
            pps.pic_parameter_set_id,
            pps.seq_parameter_set_id,
            0, // last_picture: the stream does not end
            self.frame_num as u16,
            QP,
            pps.num_ref_idx_l0_default_active_minus1,
            pps.num_ref_idx_l1_default_active_minus1,
            pps.chroma_qp_index_offset,
            pps.second_chroma_qp_index_offset,
            &pic_fields,
        );
        let slice = EncSliceParameterBufferH264::new(
            0,
            self.mb_w * self.mb_h,
            libva::VA_INVALID_ID,
            if idr { SLICE_TYPE_I } else { SLICE_TYPE_P },
            pps.pic_parameter_set_id,
            self.idr_pic_id as u16,
            0,
            0,
            [0; 2],
            0,
            0,
            0,
            0,
            ref_list_0,
            std::array::from_fn(|_| invalid_pic()),
            0,
            0,
            0,
            [0; 32],
            [0; 32],
            0,
            [[0; 2]; 32],
            [[0; 2]; 32],
            0,
            [0; 32],
            [0; 32],
            0,
            [[0; 2]; 32],
            [[0; 2]; 32],
            0, // cabac_init_idc
            0, // slice_qp_delta
            0, // disable_deblocking_filter_idc
            0,
            0,
        );

        let context = &self.context;
        let mut picture = Picture::new(u64::from(index), Rc::clone(context), &self.surfaces[0]);
        if idr {
            picture.add_buffer(context.create_buffer(BufferType::EncSequenceParameter(
                EncSequenceParameter::H264(self.seq_param()),
            ))?);
        }
        picture.add_buffer(context.create_buffer(BufferType::EncPictureParameter(
            EncPictureParameter::H264(pic),
        ))?);
        let mut packed = Vec::new();
        if idr {
            packed.push((EncPackedHeaderType::Sequence, sps_bytes(sps)?, None));
            packed.push((EncPackedHeaderType::Picture, pps_bytes(pps)?, None));
        }
        let (header, header_bits) = slice_header(idr, self.frame_num, self.idr_pic_id)?;
        packed.push((EncPackedHeaderType::Slice, header, Some(header_bits)));
        for (kind, bytes, bits) in packed {
            let bits = bits.unwrap_or(bytes.len() as u32 * 8);
            picture.add_buffer(context.create_buffer(BufferType::EncPackedHeaderParameter(
                EncPackedHeaderParameter::new(kind, bits, true),
            ))?);
            picture.add_buffer(context.create_buffer(BufferType::EncPackedHeaderData(bytes))?);
        }
        picture.add_buffer(context.create_buffer(BufferType::EncSliceParameter(
            EncSliceParameter::H264(slice),
        ))?);

        let picture = picture.begin()?.render()?.end()?;
        let picture = picture.sync().map_err(|(e, _)| e)?;

        let mut nals: Vec<Nal> = Vec::new();
        {
            let mapped = MappedCodedBuffer::new(&self.coded)?;
            for segment in mapped.segments() {
                for nal in split_nals(segment.buf) {
                    nals.push(Nal {
                        data: nal.to_vec(),
                        // One slice per picture: the slice ends it
                        last_in_frame: matches!(nal_type(nal), Some(1) | Some(5)),
                    });
                }
            }
        } // unmaps the coded buffer
        drop(picture);
        self.recon_index = reference_index;

        // An IDR must start with SPS and PPS; if the driver ever stops writing them, add ours
        if idr
            && !nals
                .iter()
                .map(|n| nal_type(&n.data))
                .eq([Some(7), Some(8), Some(5)])
        {
            eprintln!("vaapi: frame {index}: IDR came without SPS/PPS, prepending ours");
            let mut with_headers: Vec<Nal> = self
                .fallback_headers
                .iter()
                .map(|h| Nal {
                    data: h.clone(),
                    last_in_frame: false,
                })
                .collect();
            with_headers.extend(
                nals.into_iter()
                    .filter(|n| !matches!(nal_type(&n.data), Some(7) | Some(8))),
            );
            nals = with_headers;
        }
        Ok(nals)
    }
}

/// Starts a thread that encodes synthetic frames at `config.fps` and calls `publish` with each
/// NAL and the pts of its picture (time since the source started, taken when the frame is
/// produced, so a frame delayed by a stall gets the time it really has).
/// A message on `keyframe_requests` asks for an IDR, rate limited by
/// `config.min_forced_idr_interval`. Returns once the encoder is open, or with the error that
/// prevented opening it.
pub fn spawn_test_source(
    config: Config,
    keyframe_requests: mpsc::Receiver<()>,
    mut publish: impl FnMut(Nal, Duration) + Send + 'static,
) -> Result<thread::JoinHandle<()>, Error> {
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let handle = thread::Builder::new()
        .name("vaapi-encoder".to_owned())
        .spawn(move || {
            let mut encoder = match Encoder::open(&config) {
                Ok(encoder) => {
                    let _ = ready_tx.send(Ok(()));
                    encoder
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            let mut scheduler =
                IdrScheduler::new(config.gop_frames, config.min_forced_idr_interval);
            let period = Duration::from_secs(1) / config.fps;
            let start = Instant::now();
            let mut next = start;
            for index in 0u32.. {
                // Requests from sessions that just subscribed (later also PLIs)
                while keyframe_requests.try_recv().is_ok() {
                    scheduler.request();
                }
                let now = Instant::now();
                let pts = now - start;
                let idr = scheduler.next_is_idr(now);
                let result = encoder
                    .upload_synthetic(index)
                    .and_then(|()| encoder.encode(index, idr));
                match result {
                    Ok(nals) => nals.into_iter().for_each(|nal| publish(nal, pts)),
                    Err(e) => {
                        eprintln!("vaapi: encoding frame {index} failed: {e}; stopping");
                        return;
                    }
                }

                // Fixed rate from the start time; after a stall, resynchronize instead of
                // sending a burst of frames to catch up
                next += period;
                let now = Instant::now();
                if next > now {
                    thread::sleep(next - now);
                } else if now - next > period {
                    next = now;
                }
            }
        })?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err("the encoder thread ended before opening the encoder".into()),
    }
}
