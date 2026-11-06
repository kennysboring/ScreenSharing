//! IPPP sequence: IDR every `gop` frames (plus an optional forced IDR), each P referring to the
//! previous frame's reconstruction, packed headers per frame, output split into NALs without
//! start codes (as the streamer-client's EncodedFrame expects), per-frame encode timing.
//!
//! `verify_psnr` (CPU only) compares a decoded yuv420p file with the same synthetic frames.

use crate::encode::{
    QP, build_headers, header_bytes, invalid_pic, synthesize_pps, synthesize_sps, ue_bits,
};
use crate::query::{ENTRYPOINT, PROFILE};
use cros_codecs::codec::h264::{nalu_writer::NaluWriter, parser::Sps};
use libva::{
    BufferType, Display, EncPackedHeaderParameter, EncPackedHeaderType, EncPictureParameter,
    EncPictureParameterBufferH264, EncSequenceParameter, EncSequenceParameterBufferH264,
    EncSliceParameter, EncSliceParameterBufferH264, H264EncFrameCropOffsets, H264EncPicFields,
    H264EncSeqFields, Image, MappedCodedBuffer, Picture, PictureH264, UsageHint, VAConfigAttrib,
    VAConfigAttribType,
};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

type Error = Box<dyn std::error::Error>;

/// H.264 slice_type values used in the VA slice parameter buffer.
const SLICE_TYPE_P: u8 = 0;
const SLICE_TYPE_I: u8 = 2;
/// frame_num is 7 bits on the VCE (see build_headers).
const FRAME_NUM_BITS: usize = 7;
const MAX_FRAME_NUM: u32 = 1 << FRAME_NUM_BITS;

pub struct SeqOptions {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub gop: u32,
    pub force_idr_at: Option<u32>,
}

impl SeqOptions {
    pub fn is_idr(&self, index: u32) -> bool {
        index.is_multiple_of(self.gop) || self.force_idr_at == Some(index)
    }
}

/// One NAL without start code, shaped like the streamer-client's EncodedFrame: SPS/PPS are not
/// timed, slices are.
pub struct Nal {
    pub data: Vec<u8>,
    pub timed: bool,
}

/// Synthetic frame `index`: luma gradient sliding 2 px per frame and a 64 px white bar moving
/// 8 px per frame; flat chroma. Used both to fill the encoder input and to check the output.
fn luma(col: usize, row: usize, index: usize, width: usize) -> u8 {
    let bar = (index * 8) % width;
    if col >= bar && col < bar + 64 {
        235
    } else {
        ((col + row + 2 * index) / 8 % 200 + 16) as u8
    }
}
const U_VALUE: u8 = 110;
const V_VALUE: u8 = 150;

fn fill_frame(image: &mut Image, width: usize, height: usize, index: usize) {
    let va_image = *image.image();
    let (y_pitch, uv_pitch) = (va_image.pitches[0] as usize, va_image.pitches[1] as usize);
    let (y_off, uv_off) = (va_image.offsets[0] as usize, va_image.offsets[1] as usize);
    let data = image.as_mut();
    for row in 0..height {
        let line = &mut data[y_off + row * y_pitch..][..width];
        for (col, px) in line.iter_mut().enumerate() {
            *px = luma(col, row, index, width);
        }
    }
    for row in 0..height / 2 {
        let line = &mut data[uv_off + row * uv_pitch..][..width];
        for (i, px) in line.iter_mut().enumerate() {
            *px = if i % 2 == 0 { U_VALUE } else { V_VALUE };
        }
    }
}

/// Splits Annex B data into NAL payloads without their 3- or 4-byte start codes.
fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &start)| {
            let mut end = starts.get(k + 1).map_or(data.len(), |&next| next - 3);
            // A 4-byte start code leaves a zero byte before the next 00 00 01
            if k + 1 < starts.len() && end > start && data[end - 1] == 0 {
                end -= 1;
            }
            &data[start..end]
        })
        .collect()
}

fn se_bits(value: i32) -> u32 {
    let code = if value > 0 {
        2 * value as u32 - 1
    } else {
        2 * value.unsigned_abs()
    };
    ue_bits(code)
}

/// Packed slice header matching build_headers (CAVLC, POC type 2, 7-bit frame_num, no
/// deblocking fields). Returns the bytes and their length in bits.
fn slice_header(idr: bool, frame_num: u32, idr_pic_id: u32) -> Result<(Vec<u8>, u32), Error> {
    let err = |e| format!("slice header: {e:?}");
    // slice_type 7 / 5 = I / P, all slices of the picture of that type
    let slice_type: u32 = if idr { 7 } else { 5 };
    let (nal_ref_idc, nal_type) = if idr { (3, 5) } else { (2, 1) };

    let mut bytes = Vec::new();
    let mut bits = (4 + 1) * 8; // start code + NAL header
    {
        let mut w = NaluWriter::new(&mut bytes, true);
        w.write_header(nal_ref_idc, nal_type).map_err(err)?;
        for value in [0, slice_type, 0] {
            // first_mb_in_slice, slice_type, pic_parameter_set_id
            w.write_ue(value).map_err(err)?;
            bits += ue_bits(value);
        }
        w.write_u(FRAME_NUM_BITS, frame_num).map_err(err)?;
        bits += FRAME_NUM_BITS as u32;
        if idr {
            w.write_ue(idr_pic_id).map_err(err)?;
            bits += ue_bits(idr_pic_id);
        } else {
            w.write_u(1, 0u32).map_err(err)?; // num_ref_idx_active_override_flag
            w.write_u(1, 0u32).map_err(err)?; // ref_pic_list_modification_flag_l0
            bits += 2;
        }
        // dec_ref_pic_marking: IDR no_output_of_prior_pics + long_term_reference; P adaptive = 0
        let marking_bits = if idr { 2 } else { 1 };
        w.write_u(marking_bits, 0u32).map_err(err)?;
        bits += marking_bits as u32;
        w.write_se(0i32).map_err(err)?; // slice_qp_delta
        bits += se_bits(0);
    } // dropping the writer flushes the last, partial byte
    Ok((bytes, bits))
}

fn seq_param(sps: &Sps, mb_w: u32, mb_h: u32, gop: u32) -> EncSequenceParameterBufferH264 {
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
        gop, // intra_period
        gop, // intra_idr_period
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
    )
}

struct Stats {
    durations: Vec<Duration>,
}

impl Stats {
    fn report(&self) {
        let mut sorted = self.durations.clone();
        sorted.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let pick = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
        let mean = sorted.iter().sum::<Duration>() / sorted.len() as u32;
        println!(
            "encode time per frame ({} frames): median {:.2} ms, p99 {:.2} ms, worst {:.2} ms, \
             mean {:.2} ms",
            sorted.len(),
            ms(pick(0.5)),
            ms(pick(0.99)),
            ms(sorted[sorted.len() - 1]),
            ms(mean)
        );
        let worst = self
            .durations
            .iter()
            .enumerate()
            .max_by_key(|(_, d)| **d)
            .map(|(i, _)| i)
            .unwrap_or(0);
        println!("worst frame index: {worst}");
    }
}

pub fn encode_sequence(display: Rc<Display>, opts: &SeqOptions) -> Result<(), Error> {
    let (mb_w, mb_h) = (opts.width.div_ceil(16), opts.height.div_ceil(16));
    let (coded_w, coded_h) = (mb_w * 16, mb_h * 16);
    let (sps, pps) = build_headers(opts.width, opts.height)?;

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
                value: libva::VA_ENC_PACKED_HEADER_SEQUENCE
                    | libva::VA_ENC_PACKED_HEADER_PICTURE
                    | libva::VA_ENC_PACKED_HEADER_SLICE,
            },
        ],
        PROFILE,
        ENTRYPOINT,
    )?;
    // [0] input, [1] and [2] reconstructions that alternate: current and reference
    let surfaces = display.create_surfaces(
        libva::VA_RT_FORMAT_YUV420,
        Some(libva::VA_FOURCC_NV12),
        coded_w,
        coded_h,
        Some(UsageHint::USAGE_HINT_ENCODER),
        vec![(), (), ()],
    )?;
    let context = display.create_context(&config, coded_w, coded_h, Some(&surfaces), true)?;
    let coded = context.create_enc_coded((coded_w * coded_h * 3 / 2) as usize)?;
    let nv12 = display
        .query_image_formats()?
        .into_iter()
        .find(|f| f.fourcc == libva::VA_FOURCC_NV12)
        .ok_or("driver has no NV12 image format")?;

    // Our SPS/PPS as NALs, only used if the driver ever returns an IDR without them
    let fallback_headers: Vec<Vec<u8>> = split_nals(&header_bytes(&sps, &pps)?)
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect();

    let input = &surfaces[0];
    let mut recon_index = 1; // reconstruction surface the current frame writes to
    let mut frame_num = 0u32;
    let mut idr_pic_id = 0u32;
    let mut prepended = 0u32;
    let mut stats = Stats {
        durations: Vec::with_capacity(opts.frames as usize),
    };
    let mut prepare = Duration::ZERO;
    let mut out_nals: Vec<Nal> = Vec::new();

    for index in 0..opts.frames {
        let idr = opts.is_idr(index);
        if idr {
            frame_num = 0;
            if index > 0 {
                idr_pic_id = (idr_pic_id + 1) % 65536;
            }
        } else {
            frame_num = (frame_num + 1) % MAX_FRAME_NUM;
        }
        let reference_index = 3 - recon_index; // the other reconstruction surface
        let (recon, reference) = (&surfaces[recon_index], &surfaces[reference_index]);

        // Not timed: synthetic content
        let t = Instant::now();
        {
            let mut image =
                Image::create_from(input, nv12, (coded_w, coded_h), (coded_w, coded_h))?;
            fill_frame(
                &mut image,
                coded_w as usize,
                coded_h as usize,
                index as usize,
            );
        }
        input.sync()?;
        prepare += t.elapsed();

        // Timed: buffers, encode, sync and reading the coded buffer
        let t = Instant::now();
        let poc = (2 * frame_num) as i32;
        let reference_frame_num = (frame_num + MAX_FRAME_NUM - 1) % MAX_FRAME_NUM;
        let reference_pic = PictureH264::new(
            reference.id(),
            reference_frame_num,
            libva::VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            2 * reference_frame_num as i32,
            2 * reference_frame_num as i32,
        );

        let mut reference_frames: [PictureH264; 16] = std::array::from_fn(|_| invalid_pic());
        let mut ref_list_0: [PictureH264; 32] = std::array::from_fn(|_| invalid_pic());
        if !idr {
            reference_frames[0] = reference_pic;
            ref_list_0[0] = PictureH264::new(
                reference.id(),
                reference_frame_num,
                libva::VA_PICTURE_H264_SHORT_TERM_REFERENCE,
                2 * reference_frame_num as i32,
                2 * reference_frame_num as i32,
            );
        }

        let pic_fields = H264EncPicFields::new(
            u32::from(idr),
            1, // every frame is a reference for the next one
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
            PictureH264::new(recon.id(), frame_num, 0, poc, poc),
            reference_frames,
            coded.id(),
            pps.pic_parameter_set_id,
            pps.seq_parameter_set_id,
            u8::from(index + 1 == opts.frames),
            frame_num as u16,
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
            if idr { SLICE_TYPE_I } else { SLICE_TYPE_P },
            pps.pic_parameter_set_id,
            idr_pic_id as u16,
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

        let mut picture = Picture::new(u64::from(index), Rc::clone(&context), input);
        if idr {
            picture.add_buffer(context.create_buffer(BufferType::EncSequenceParameter(
                EncSequenceParameter::H264(seq_param(&sps, mb_w, mb_h, opts.gop)),
            ))?);
        }
        picture.add_buffer(context.create_buffer(BufferType::EncPictureParameter(
            EncPictureParameter::H264(pic),
        ))?);
        let mut packed = Vec::new();
        if idr {
            packed.push((EncPackedHeaderType::Sequence, synthesize_sps(&sps)?, None));
            packed.push((EncPackedHeaderType::Picture, synthesize_pps(&pps)?, None));
        }
        let (header, header_bits) = slice_header(idr, frame_num, idr_pic_id)?;
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

        let mut frame_nals: Vec<Nal> = Vec::new();
        {
            let mapped = MappedCodedBuffer::new(&coded)?;
            for segment in mapped.segments() {
                for nal in split_nals(segment.buf) {
                    let nal_type = nal.first().map(|b| b & 0x1f);
                    frame_nals.push(Nal {
                        data: nal.to_vec(),
                        timed: matches!(nal_type, Some(1) | Some(5)),
                    });
                }
            }
        } // unmaps the coded buffer
        drop(picture);
        stats.durations.push(t.elapsed());

        // Runtime check: an IDR must start with SPS and PPS; if not, add ours and warn
        let types: Vec<u8> = frame_nals.iter().map(|n| n.data[0] & 0x1f).collect();
        if idr && !types.starts_with(&[7, 8]) {
            prepended += 1;
            println!(
                "warning: frame {index}: IDR came without SPS/PPS ({types:?}); prepending ours"
            );
            let mut with_headers: Vec<Nal> = fallback_headers
                .iter()
                .map(|h| Nal {
                    data: h.clone(),
                    timed: false,
                })
                .collect();
            with_headers.append(&mut frame_nals);
            frame_nals = with_headers;
        }
        if idr || index < 2 {
            let shown: Vec<u8> = frame_nals.iter().map(|n| n.data[0] & 0x1f).collect();
            println!(
                "frame {index:3}: {} frame_num {frame_num:3}, NAL types {shown:?}, {} bytes",
                if idr { "IDR" } else { "P  " },
                frame_nals.iter().map(|n| n.data.len()).sum::<usize>()
            );
        }
        out_nals.append(&mut frame_nals);
        recon_index = reference_index;
    }

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("out-ippp.h264");
    let mut file = File::create(&path)?;
    let mut total = 0usize;
    for nal in &out_nals {
        file.write_all(&[0, 0, 0, 1])?;
        file.write_all(&nal.data)?;
        total += nal.data.len() + 4;
    }
    let timed = out_nals.iter().filter(|n| n.timed).count();
    println!(
        "{} NALs ({timed} timed slices, {} SPS/PPS), {total} bytes -> {}",
        out_nals.len(),
        out_nals.len() - timed,
        path.display()
    );
    println!(
        "IDRs without driver SPS/PPS (prepended): {prepended}; prepare (not timed) {:.2} ms/frame",
        prepare.as_secs_f64() * 1000.0 / f64::from(opts.frames)
    );
    stats.report();
    Ok(())
}

/// CPU only: PSNR of each frame of a decoded yuv420p file against the synthetic frames.
pub fn verify_psnr(path: &str, opts: &SeqOptions) -> Result<(), Error> {
    let (w, h) = (opts.width as usize, opts.height as usize);
    let frame_size = w * h * 3 / 2;
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; frame_size];
    let psnr = |sum: u64, n: usize| {
        if sum == 0 {
            f64::INFINITY
        } else {
            10.0 * (255.0 * 255.0 / (sum as f64 / n as f64)).log10()
        }
    };

    let mut luma_psnr = Vec::new();
    let mut worst_chroma = f64::INFINITY;
    for index in 0..opts.frames as usize {
        file.read_exact(&mut buf)
            .map_err(|e| format!("frame {index}: {e}"))?;
        let mut sum_y = 0u64;
        for row in 0..h {
            for col in 0..w {
                let d = i64::from(buf[row * w + col]) - i64::from(luma(col, row, index, w));
                sum_y += (d * d) as u64;
            }
        }
        let quarter = w * h / 4;
        let chroma = |plane: &[u8], expected: u8| {
            plane
                .iter()
                .map(|&b| (i64::from(b) - i64::from(expected)).pow(2) as u64)
                .sum::<u64>()
        };
        let sum_u = chroma(&buf[w * h..w * h + quarter], U_VALUE);
        let sum_v = chroma(&buf[w * h + quarter..], V_VALUE);
        luma_psnr.push(psnr(sum_y, w * h));
        worst_chroma = worst_chroma
            .min(psnr(sum_u, quarter))
            .min(psnr(sum_v, quarter));
    }

    println!("per-frame luma PSNR (dB), I = IDR:");
    for (row, chunk) in luma_psnr.chunks(10).enumerate() {
        let line: Vec<String> = chunk
            .iter()
            .enumerate()
            .map(|(k, p)| {
                let index = (row * 10 + k) as u32;
                format!("{}{p:5.2}", if opts.is_idr(index) { "I" } else { " " })
            })
            .collect();
        println!(
            "  {:3}-{:3}: {}",
            row * 10,
            row * 10 + chunk.len() - 1,
            line.join(" ")
        );
    }
    let (min_index, min) = luma_psnr
        .iter()
        .copied()
        .enumerate()
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .ok_or("no frames")?;
    let mean = luma_psnr.iter().sum::<f64>() / luma_psnr.len() as f64;
    println!("luma: min {min:.2} dB (frame {min_index}), mean {mean:.2} dB");
    println!("chroma: worst plane PSNR {worst_chroma:.2} dB");

    // Average by distance from the last IDR: a gradual drift would show as a falling curve
    let mut by_position: Vec<(f64, u32)> = Vec::new();
    let mut since_idr = 0usize;
    for (index, p) in luma_psnr.iter().enumerate() {
        if opts.is_idr(index as u32) {
            since_idr = 0;
        }
        if by_position.len() <= since_idr {
            by_position.push((0.0, 0));
        }
        by_position[since_idr].0 += p;
        by_position[since_idr].1 += 1;
        since_idr += 1;
    }
    let curve: Vec<String> = by_position
        .iter()
        .enumerate()
        .filter(|(pos, _)| pos % 5 == 0 || *pos + 1 == by_position.len())
        .map(|(pos, (sum, n))| format!("{pos}:{:.2}", sum / f64::from(*n)))
        .collect();
    println!("mean luma PSNR by frames since IDR: {}", curve.join("  "));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_nals_handles_3_and_4_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 0xaa, 0, 0, 1, 0x68, 0xbb, 0, 0, 0, 1, 0x65, 0xcc, 0xdd,
        ];
        let nals = split_nals(&data);
        assert_eq!(
            nals,
            [&[0x67, 0xaa][..], &[0x68, 0xbb], &[0x65, 0xcc, 0xdd]]
        );
    }

    #[test]
    fn test_slice_header_bits_fit_the_written_bytes() {
        for (idr, frame_num, idr_pic_id) in
            [(true, 0, 0), (true, 0, 5), (false, 1, 0), (false, 29, 0)]
        {
            let (bytes, bits) = slice_header(idr, frame_num, idr_pic_id).unwrap();
            // The last byte is padded with zeros, so bits round up to exactly the byte count
            assert_eq!(
                bits.div_ceil(8) as usize,
                bytes.len(),
                "idr {idr} frame_num {frame_num}"
            );
            assert_eq!(&bytes[..4], &[0, 0, 0, 1]);
            assert_eq!(bytes[4] & 0x1f, if idr { 5 } else { 1 });
        }
    }
}
