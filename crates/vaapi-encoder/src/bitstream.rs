//! H.264 bitstream pieces the encoder needs: SPS/PPS matching what Mesa's VCE backend writes,
//! the packed slice header for each frame, and splitting the coded output into NALs.
//!
//! The VCE firmware writes the real slice headers and Mesa writes its own SPS/PPS (Mesa 26.2
//! `radeon_vce.c`): `frame_num` always has 7 bits, and the PPS declares deblocking fields only
//! when the filter is disabled or has offsets. Our SPS/PPS must agree with those choices, both
//! to fill the VA parameter buffers consistently and to be a valid fallback when an IDR comes
//! back without headers. In spikes/encoder the driver's SPS/PPS were byte-identical to these.

use crate::Error;
use cros_codecs::codec::h264::{
    nalu_writer::NaluWriter,
    parser::{Level, Pps, PpsBuilder, Profile, Sps, SpsBuilder},
    synthesizer::Synthesizer,
};
use std::rc::Rc;

/// Fixed quantizer (CQP): middle of the 0..51 range.
pub const QP: u8 = 26;
/// frame_num width the VCE firmware uses (Mesa forces log2_max_frame_num_minus4 = 3).
pub const FRAME_NUM_BITS: usize = 7;
pub const MAX_FRAME_NUM: u32 = 1 << FRAME_NUM_BITS;

/// H.264 level for a frame size and rate: 4.0 covers 1080p30, 4.2 covers 1080p60.
pub fn level_for(width: u32, height: u32, fps: u32) -> Level {
    let macroblocks_per_second = width.div_ceil(16) * height.div_ceil(16) * fps;
    if macroblocks_per_second <= 245_760 {
        Level::L4
    } else {
        Level::L4_2
    }
}

/// SPS and PPS of the stream: Constrained Baseline, CAVLC, one reference frame, POC type 2.
pub fn build_headers(width: u32, height: u32, level: Level) -> Result<(Rc<Sps>, Rc<Pps>), Error> {
    // frame_mbs_only and chroma_format must be set before resolution(), which computes the crop
    let mut sps = SpsBuilder::new()
        .seq_parameter_set_id(0)
        .profile_idc(Profile::Baseline)
        .level_idc(level)
        .chroma_format_idc(1)
        .frame_mbs_only_flag(true)
        .direct_8x8_inference_flag(true)
        .max_num_ref_frames(1)
        .max_frame_num(MAX_FRAME_NUM)
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
        // With the default deblocking filter the VCE slice header has no deblocking fields
        .deblocking_filter_control_present_flag(false)
        .build();
    Ok((sps, pps))
}

/// SPS as an Annex B NAL (start code and emulation prevention included).
pub fn sps_bytes(sps: &Sps) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    Synthesizer::<Sps, _>::synthesize(3, sps, &mut bytes, true)
        .map_err(|e| format!("SPS: {e:?}"))?;
    Ok(bytes)
}

/// PPS as an Annex B NAL (start code and emulation prevention included).
pub fn pps_bytes(pps: &Pps) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    Synthesizer::<Pps, _>::synthesize(3, pps, &mut bytes, true)
        .map_err(|e| format!("PPS: {e:?}"))?;
    Ok(bytes)
}

/// Bits of an Exp-Golomb ue(v) code.
pub fn ue_bits(value: u32) -> u32 {
    2 * (value + 1).ilog2() + 1
}

/// Bits of an Exp-Golomb se(v) code.
pub fn se_bits(value: i32) -> u32 {
    let code = if value > 0 {
        2 * value as u32 - 1
    } else {
        2 * value.unsigned_abs()
    };
    ue_bits(code)
}

/// Packed slice header matching [`build_headers`] (CAVLC, POC type 2, 7-bit frame_num, no
/// deblocking fields, no reordering, no MMCO). Returns the Annex B bytes and their length in
/// bits, without the padding of the last byte.
///
/// Mesa parses it for slice parameters and, for an IDR, needs it to emit SPS/PPS; the bytes are
/// not copied into the output, since the firmware writes the real slice header.
pub fn slice_header(idr: bool, frame_num: u32, idr_pic_id: u32) -> Result<(Vec<u8>, u32), Error> {
    let err = |e| format!("slice header: {e:?}");
    // slice_type 7 / 5: I / P, with all slices of the picture of that type
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
        w.write_u(FRAME_NUM_BITS, frame_num % MAX_FRAME_NUM)
            .map_err(err)?;
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

/// Splits Annex B data into NAL payloads without their 3- or 4-byte start codes.
pub fn split_nals(data: &[u8]) -> Vec<&[u8]> {
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

/// NAL unit type of a NAL without start code.
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_nals_handles_3_and_4_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 0xaa, 0, 0, 1, 0x68, 0xbb, 0, 0, 0, 1, 0x65, 0xcc, 0xdd,
        ];
        assert_eq!(
            split_nals(&data),
            [&[0x67, 0xaa][..], &[0x68, 0xbb], &[0x65, 0xcc, 0xdd]]
        );
    }

    #[test]
    fn test_slice_header_bits_fit_the_written_bytes() {
        for (idr, frame_num, idr_pic_id) in
            [(true, 0, 0), (true, 0, 5), (false, 1, 0), (false, 59, 0)]
        {
            let (bytes, bits) = slice_header(idr, frame_num, idr_pic_id).unwrap();
            // The last byte is padded with zeros, so the bits round up to the byte count
            assert_eq!(bits.div_ceil(8) as usize, bytes.len());
            assert_eq!(&bytes[..4], &[0, 0, 0, 1]);
            assert_eq!(bytes[4] & 0x1f, if idr { 5 } else { 1 });
        }
    }

    #[test]
    fn test_headers_match_the_vce_spike_output() {
        // SPS/PPS that Mesa's VCE backend returned in spikes/encoder for 1920x1080 at level 4.0
        let (sps, pps) = build_headers(1920, 1080, Level::L4).unwrap();
        let mut headers = sps_bytes(&sps).unwrap();
        headers.extend(pps_bytes(&pps).unwrap());
        assert_eq!(
            headers,
            [
                0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x28, 0x91, 0xa0, 0x1e, 0x00, 0x89, 0xf9, 0x50, 0, 0,
                0, 1, 0x68, 0xce, 0x38, 0x80
            ]
        );
    }

    #[test]
    fn test_level_for() {
        assert_eq!(level_for(1920, 1080, 30) as u8, Level::L4 as u8);
        assert_eq!(level_for(1920, 1080, 60) as u8, Level::L4_2 as u8);
        assert_eq!(level_for(1280, 720, 60) as u8, Level::L4 as u8);
    }
}
