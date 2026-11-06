//! One H.264 Constrained Baseline IDR through VA-API: NV12 surfaces created by the driver, filled
//! through an Image, encoded with CQP, written as Annex B.

use crate::query::{ENTRYPOINT, PROFILE};
use cros_codecs::codec::h264::{
    nalu_writer::NaluWriter,
    parser::{Level, Pps, PpsBuilder, Profile, Sps, SpsBuilder},
    synthesizer::Synthesizer,
};
use libva::{
    BufferType, Display, EncCodedBuffer, EncPackedHeaderParameter, EncPackedHeaderType,
    EncPictureParameter, EncPictureParameterBufferH264, EncSequenceParameter,
    EncSequenceParameterBufferH264, EncSliceParameter, EncSliceParameterBufferH264,
    H264EncFrameCropOffsets, H264EncPicFields, H264EncSeqFields, Image, MappedCodedBuffer, Picture,
    PictureH264, UsageHint, VAConfigAttrib, VAConfigAttribType,
};
use std::{fs::File, io::Write, path::Path, rc::Rc};

type Error = Box<dyn std::error::Error>;

/// Fixed quantizer for the CQP test: middle of the 0..51 range.
pub(crate) const QP: u8 = 26;
/// H.264 slice_type values used by VA-API.
const SLICE_TYPE_I: u8 = 2;

pub struct Options {
    pub width: u32,
    pub height: u32,
    pub packed_headers: bool,
}

/// Picture placeholder for unused reference slots.
pub(crate) fn invalid_pic() -> PictureH264 {
    PictureH264::new(
        libva::VA_INVALID_ID,
        0,
        libva::VA_PICTURE_H264_INVALID,
        0,
        0,
    )
}

/// Fills an NV12 image: gradient luma with a white bar, flat-ish chroma.
fn fill_nv12(image: &mut Image, width: usize, height: usize) {
    let va_image = *image.image();
    let (y_pitch, uv_pitch) = (va_image.pitches[0] as usize, va_image.pitches[1] as usize);
    let (y_off, uv_off) = (va_image.offsets[0] as usize, va_image.offsets[1] as usize);
    let data = image.as_mut();
    for row in 0..height {
        let line = &mut data[y_off + row * y_pitch..][..width];
        for (col, px) in line.iter_mut().enumerate() {
            *px = if (64..128).contains(&col) {
                235
            } else {
                ((col + row) / 8 % 200 + 16) as u8
            };
        }
    }
    for row in 0..height / 2 {
        let line = &mut data[uv_off + row * uv_pitch..][..width];
        for (i, px) in line.iter_mut().enumerate() {
            *px = if i % 2 == 0 { 110 } else { 150 };
        }
    }
}

/// Lists NAL unit types in an Annex B stream.
pub fn nal_types(stream: &[u8]) -> Vec<u8> {
    stream
        .windows(4)
        .filter(|w| w[..3] == [0, 0, 1])
        .map(|w| w[3] & 0x1f)
        .collect()
}

/// The one SPS/PPS used everywhere: to fill the VA parameter buffers, as packed headers, and when
/// prepending headers to a driver-written slice. Sharing it keeps those from drifting apart.
pub fn build_headers(width: u32, height: u32) -> Result<(Rc<Sps>, Rc<Pps>), Error> {
    // frame_mbs_only and chroma_format must be set before resolution(), which computes the crop
    let mut sps = SpsBuilder::new()
        .seq_parameter_set_id(0)
        .profile_idc(Profile::Baseline)
        .level_idc(Level::L4)
        .chroma_format_idc(1)
        .frame_mbs_only_flag(true)
        .direct_8x8_inference_flag(true)
        .max_num_ref_frames(1)
        // The VCE firmware always writes frame_num with 7 bits: Mesa's radeon_vce.c write_sps()
        // forces log2_max_frame_num_minus4 = 3. Anything else here misaligns the slice header.
        .max_frame_num(128)
        .pic_order_cnt_type(2)
        .resolution(width, height)
        .build();
    // Constrained Baseline is Baseline (66) with constraint_set1; set0 too, as ffmpeg does
    let sps_mut = Rc::get_mut(&mut sps).ok_or("SPS is shared")?;
    sps_mut.constraint_set0_flag = true;
    sps_mut.constraint_set1_flag = true;
    let pps = PpsBuilder::new(Rc::clone(&sps))
        .pic_parameter_set_id(0)
        .pic_init_qp(QP)
        // Mesa's write_pps() sets this only when deblocking is disabled or has offsets; with the
        // default filter the VCE slice header has no deblocking fields
        .deblocking_filter_control_present_flag(false)
        .build();
    Ok((sps, pps))
}

/// SPS + PPS as Annex B bytes (start codes and emulation prevention included).
pub fn header_bytes(sps: &Sps, pps: &Pps) -> Result<Vec<u8>, Error> {
    let mut bytes = synthesize_sps(sps)?;
    bytes.extend(synthesize_pps(pps)?);
    Ok(bytes)
}

pub fn encode_idr(display: Rc<Display>, opts: &Options) -> Result<(), Error> {
    let (mb_w, mb_h) = (opts.width.div_ceil(16), opts.height.div_ceil(16));
    let (coded_w, coded_h) = (mb_w * 16, mb_h * 16);
    let (sps, pps) = build_headers(opts.width, opts.height)?;

    let packed_mask = if opts.packed_headers {
        libva::VA_ENC_PACKED_HEADER_SEQUENCE
            | libva::VA_ENC_PACKED_HEADER_PICTURE
            | libva::VA_ENC_PACKED_HEADER_SLICE
    } else {
        libva::VA_ENC_PACKED_HEADER_NONE
    };
    let config = display.create_config(
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
                value: packed_mask,
            },
        ],
        PROFILE,
        ENTRYPOINT,
    )?;

    // Input and reconstruction surfaces, allocated by the driver in its preferred layout
    let surfaces = display.create_surfaces(
        libva::VA_RT_FORMAT_YUV420,
        Some(libva::VA_FOURCC_NV12),
        coded_w,
        coded_h,
        Some(UsageHint::USAGE_HINT_ENCODER),
        vec![(), ()],
    )?;
    let (input, recon) = (&surfaces[0], &surfaces[1]);
    let context = display.create_context(&config, coded_w, coded_h, Some(&surfaces), true)?;

    let nv12 = display
        .query_image_formats()?
        .into_iter()
        .find(|f| f.fourcc == libva::VA_FOURCC_NV12)
        .ok_or("driver has no NV12 image format")?;
    {
        let mut image = Image::create_from(input, nv12, (coded_w, coded_h), (coded_w, coded_h))?;
        println!(
            "input image: derived {}, pitches {:?}, offsets {:?}",
            image.is_derived(),
            &image.image().pitches[..2],
            &image.image().offsets[..2]
        );
        fill_nv12(&mut image, coded_w as usize, coded_h as usize);
    } // dropping the Image writes it back to the surface when it was not derived
    input.sync()?;

    let coded: EncCodedBuffer = context.create_enc_coded((coded_w * coded_h * 3 / 2) as usize)?;

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
    let seq = EncSequenceParameterBufferH264::new(
        sps.seq_parameter_set_id,
        sps.level_idc as u8,
        120, // intra_period: one IDR per 120 frames (step 3)
        120, // intra_idr_period
        1,   // ip_period: no B-frames
        0,   // bits_per_second: unused with CQP
        u32::from(sps.max_num_ref_frames),
        mb_w as u16,
        mb_h as u16,
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
    );

    let pic_fields = H264EncPicFields::new(
        1, // idr_pic_flag
        1, // reference_pic_flag: the next P-frame will refer to it
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
        PictureH264::new(recon.id(), 0, 0, 0, 0),
        std::array::from_fn(|_| invalid_pic()),
        coded.id(),
        pps.pic_parameter_set_id,
        pps.seq_parameter_set_id,
        0, // last_picture
        0, // frame_num
        QP,
        pps.num_ref_idx_l0_default_active_minus1,
        pps.num_ref_idx_l1_default_active_minus1,
        pps.chroma_qp_index_offset,
        pps.second_chroma_qp_index_offset,
        &pic_fields,
    );

    let slice = EncSliceParameterBufferH264::new(
        0,
        mb_w * mb_h,
        libva::VA_INVALID_ID,
        SLICE_TYPE_I,
        pps.pic_parameter_set_id,
        0, // idr_pic_id
        0, // pic_order_cnt_lsb
        0,
        [0; 2],
        0,
        0,
        0,
        0,
        std::array::from_fn(|_| invalid_pic()),
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

    let mut picture = Picture::new(0, Rc::clone(&context), input);
    picture.add_buffer(context.create_buffer(BufferType::EncSequenceParameter(
        EncSequenceParameter::H264(seq),
    ))?);
    picture.add_buffer(context.create_buffer(BufferType::EncPictureParameter(
        EncPictureParameter::H264(pic),
    ))?);
    // The ffmpeg path. Mesa's VCE backend emits headers only when a packed *slice* header is also
    // present (SEQUENCE + PICTURE alone gave a byte-identical output). It then writes its own SPS
    // and PPS; the slice header bytes are parsed for parameters but not copied, since the firmware
    // writes the real slice header.
    if opts.packed_headers {
        let (slice_header, slice_header_bits) = synthesize_idr_slice_header()?;
        for (kind, bytes, bits) in [
            (EncPackedHeaderType::Sequence, synthesize_sps(&sps)?, None),
            (EncPackedHeaderType::Picture, synthesize_pps(&pps)?, None),
            (
                EncPackedHeaderType::Slice,
                slice_header,
                Some(slice_header_bits),
            ),
        ] {
            let bits = bits.unwrap_or(bytes.len() as u32 * 8);
            picture.add_buffer(context.create_buffer(BufferType::EncPackedHeaderParameter(
                EncPackedHeaderParameter::new(kind, bits, true),
            ))?);
            picture.add_buffer(context.create_buffer(BufferType::EncPackedHeaderData(bytes))?);
        }
    }
    picture.add_buffer(context.create_buffer(BufferType::EncSliceParameter(
        EncSliceParameter::H264(slice),
    ))?);

    let picture = picture.begin()?.render()?.end()?;
    let _picture = picture.sync().map_err(|(e, _)| e)?;

    let mapped = MappedCodedBuffer::new(&coded)?;
    let mut stream = Vec::new();
    for (i, segment) in mapped.segments().iter().enumerate() {
        println!(
            "coded segment {i}: {} bytes, status 0x{:08x}, bit_offset {}",
            segment.buf.len(),
            segment.status,
            segment.bit_offset
        );
        stream.extend_from_slice(segment.buf);
    }

    let name = if opts.packed_headers {
        "out-idr-packed.h264"
    } else {
        "out-idr.h264"
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    File::create(&path)?.write_all(&stream)?;
    let types = nal_types(&stream);
    println!(
        "wrote {} bytes to {}; NAL types: {:?}",
        stream.len(),
        path.display(),
        types
    );
    println!(
        "driver wrote SPS/PPS: {}",
        if types.starts_with(&[7, 8]) {
            "yes"
        } else {
            "no"
        }
    );
    Ok(())
}

/// Bits of an Exp-Golomb ue(v) code.
pub(crate) fn ue_bits(value: u32) -> u32 {
    2 * (value + 1).ilog2() + 1
}

/// Packed slice header for the IDR, matching `build_headers` (CAVLC, POC type 2, 7-bit frame_num,
/// no deblocking fields). Returns the bytes and their length in bits, without the padding of the
/// last byte.
fn synthesize_idr_slice_header() -> Result<(Vec<u8>, u32), Error> {
    const SLICE_TYPE_I_ALL: u32 = 7;
    const FRAME_NUM_BITS: usize = 7;
    let err = |e| format!("slice header: {e:?}");

    let mut bytes = Vec::new();
    {
        let mut w = NaluWriter::new(&mut bytes, true);
        w.write_header(3, 5).map_err(err)?; // nal_ref_idc 3, IDR slice
        w.write_ue(0u32).map_err(err)?; // first_mb_in_slice
        w.write_ue(SLICE_TYPE_I_ALL).map_err(err)?; // slice_type
        w.write_ue(0u32).map_err(err)?; // pic_parameter_set_id
        w.write_u(FRAME_NUM_BITS, 0u32).map_err(err)?; // frame_num
        w.write_ue(0u32).map_err(err)?; // idr_pic_id
        w.write_u(1, 0u32).map_err(err)?; // no_output_of_prior_pics_flag
        w.write_u(1, 0u32).map_err(err)?; // long_term_reference_flag
        w.write_se(0i32).map_err(err)?; // slice_qp_delta
    } // dropping the writer flushes the last, partial byte

    // 4-byte start code + 1-byte NAL header, then the fields above
    let bits = (4 + 1) * 8
        + ue_bits(0)
        + ue_bits(SLICE_TYPE_I_ALL)
        + ue_bits(0)
        + FRAME_NUM_BITS as u32
        + ue_bits(0)
        + 2
        + ue_bits(0); // se(0) is coded as ue(0)
    Ok((bytes, bits))
}

pub(crate) fn synthesize_sps(sps: &Sps) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    Synthesizer::<Sps, _>::synthesize(3, sps, &mut bytes, true)
        .map_err(|e| format!("SPS: {e:?}"))?;
    Ok(bytes)
}

pub(crate) fn synthesize_pps(pps: &Pps) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    Synthesizer::<Pps, _>::synthesize(3, pps, &mut bytes, true)
        .map_err(|e| format!("PPS: {e:?}"))?;
    Ok(bytes)
}
