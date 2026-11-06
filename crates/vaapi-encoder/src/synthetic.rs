//! Synthetic NV12 test frames: a luma gradient sliding 2 px per frame and a 64 px white bar
//! moving 8 px per frame, flat chroma. The motion gives P-frames real work.

pub const U_VALUE: u8 = 110;
pub const V_VALUE: u8 = 150;
const BAR_WIDTH: usize = 64;

/// Luma of pixel (`col`, `row`) in frame `index` of a picture `width` pixels wide.
pub fn luma(col: usize, row: usize, index: usize, width: usize) -> u8 {
    let bar = (index * 8) % width;
    if col >= bar && col < bar + BAR_WIDTH {
        235
    } else {
        ((col + row + 2 * index) / 8 % 200 + 16) as u8
    }
}

/// Writes frame `index` into an NV12 buffer with the given plane pitches and offsets (as a
/// mapped VA image reports them). Bytes outside the `width` x `height` picture are untouched.
pub fn fill_nv12(
    data: &mut [u8],
    pitches: [usize; 2],
    offsets: [usize; 2],
    width: usize,
    height: usize,
    index: usize,
) {
    for row in 0..height {
        let line = &mut data[offsets[0] + row * pitches[0]..][..width];
        for (col, px) in line.iter_mut().enumerate() {
            *px = luma(col, row, index, width);
        }
    }
    for row in 0..height / 2 {
        let line = &mut data[offsets[1] + row * pitches[1]..][..width];
        for (i, px) in line.iter_mut().enumerate() {
            // Interleaved chroma: even bytes U, odd bytes V
            *px = if i % 2 == 0 { U_VALUE } else { V_VALUE };
        }
    }
}

/// Y'CbCr in BT.709 limited range (Y 16-235, Cb/Cr 16-240) of a full-range R'G'B' color: only
/// the matrix, no transfer curve conversion, which is what the VPP is set up to do.
pub fn rgb_to_bt709_limited(rgb: [u8; 3]) -> [f64; 3] {
    let (kr, kb) = (0.2126, 0.0722);
    let [r, g, b] = rgb.map(|c| f64::from(c) / 255.0);
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    let cb = (b - y) / (2.0 * (1.0 - kb));
    let cr = (r - y) / (2.0 * (1.0 - kr));
    [16.0 + 219.0 * y, 128.0 + 224.0 * cb, 128.0 + 224.0 * cr]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fill_nv12_respects_pitch_and_offsets() {
        // 4x2 picture, luma pitch 6 (2 padding bytes per row), chroma plane at offset 16
        const PAD: u8 = 0xee;
        let mut data = vec![PAD; 16 + 6];
        fill_nv12(&mut data, [6, 6], [0, 16], 4, 2, 0);

        for row in 0..2 {
            for col in 0..4 {
                assert_eq!(data[row * 6 + col], luma(col, row, 0, 4));
            }
            assert_eq!(&data[row * 6 + 4..row * 6 + 6], &[PAD, PAD], "luma padding");
        }
        assert_eq!(&data[16..20], &[U_VALUE, V_VALUE, U_VALUE, V_VALUE]);
        assert_eq!(&data[20..22], &[PAD, PAD], "chroma padding");
    }

    #[test]
    fn test_bt709_limited_reference_values() {
        let round = |v: [f64; 3]| v.map(|x| x.round() as i32);
        assert_eq!(
            round(rgb_to_bt709_limited([255, 255, 255])),
            [235, 128, 128]
        );
        assert_eq!(round(rgb_to_bt709_limited([0, 0, 0])), [16, 128, 128]);
        assert_eq!(round(rgb_to_bt709_limited([255, 0, 0])), [63, 102, 240]);
        assert_eq!(round(rgb_to_bt709_limited([0, 0, 255])), [32, 240, 118]);
    }

    #[test]
    fn test_bar_moves_between_frames() {
        let width = 1920;
        assert_eq!(luma(0, 0, 0, width), 235);
        assert_ne!(luma(0, 0, 10, width), 235);
        assert_eq!(luma(80, 0, 10, width), 235);
    }
}
