//! Encoder spike: checks that VA-API offers H.264 Constrained Baseline encode and, only when asked
//! with `--encode`, encodes synthetic NV12 frames to `out.h264` and reports the time per frame.
//!
//! Without `--encode` nothing is submitted to the GPU's encoder: it only queries the driver, like
//! `vainfo`. H.264 encode hung the RX 580's VCE on kernel 7.2.11 (reported as a regression since
//! 7.2.2, fixed in 7.3-rc2), so `--encode` refuses to run on 7.2.2 to 7.2.x unless
//! `--ignore-kernel-check` is given.
//!
//! `--alloc-only` only tries NV12 allocations through GBM (see `alloc_probe`); it does not open
//! VA-API at all.

use cros_codecs::{
    BlockingMode, Fourcc, Resolution,
    codec::h264::parser::{Level, Profile},
    encoder::{
        FrameMetadata, RateControl, Tunings, VideoEncoder, h264::EncoderConfig, stateless::h264,
    },
    libva::{Display, VAEntrypoint, VAProfile},
    video_frame::{
        UV_PLANE, VideoFrame, Y_PLANE,
        gbm_video_frame::{GbmDevice, GbmUsage},
        generic_dma_video_frame::GenericDmaVideoFrame,
    },
};
use std::{
    fs::File,
    io::Write,
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

mod alloc_probe;

type Error = Box<dyn std::error::Error>;

const RENDER_NODE: &str = "/dev/dri/renderD128";
const BITRATE: u64 = 6_000_000;
const FRAMERATE: u32 = 30;

struct Args {
    alloc_only: bool,
    encode: bool,
    ignore_kernel_check: bool,
    size: Resolution,
    frames: u32,
}

fn parse_args() -> Result<Args, Error> {
    let mut args = Args {
        alloc_only: false,
        encode: false,
        ignore_kernel_check: false,
        size: Resolution {
            width: 1920,
            height: 1080,
        },
        frames: 120,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--alloc-only" => args.alloc_only = true,
            "--encode" => args.encode = true,
            "--ignore-kernel-check" => args.ignore_kernel_check = true,
            "--size" => {
                let value = it.next().ok_or("--size needs WxH")?;
                let (w, h) = value.split_once('x').ok_or("--size needs WxH")?;
                args.size = Resolution {
                    width: w.parse()?,
                    height: h.parse()?,
                };
            }
            "--frames" => args.frames = it.next().ok_or("--frames needs N")?.parse()?,
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    if !args.size.width.is_multiple_of(2) || !args.size.height.is_multiple_of(2) || args.frames == 0
    {
        return Err("size must be even and frames > 0".into());
    }
    Ok(args)
}

fn entrypoint_name(entrypoint: u32) -> &'static str {
    match entrypoint {
        VAEntrypoint::VAEntrypointVLD => "VLD (decode)",
        VAEntrypoint::VAEntrypointEncSlice => "EncSlice (encode)",
        VAEntrypoint::VAEntrypointEncSliceLP => "EncSliceLP (low-power encode)",
        VAEntrypoint::VAEntrypointEncPicture => "EncPicture",
        VAEntrypoint::VAEntrypointVideoProc => "VideoProc",
        _ => "other",
    }
}

/// Opens the VA display and checks the H.264 Constrained Baseline encode entrypoint. Query only:
/// no config, context or surface is created.
fn query(display: &Display) -> Result<(), Error> {
    println!("vendor: {}", display.query_vendor_string()?);

    let profile = VAProfile::VAProfileH264ConstrainedBaseline;
    if !display.query_config_profiles()?.contains(&profile) {
        return Err("H264ConstrainedBaseline is not offered by this driver".into());
    }
    let entrypoints = display.query_config_entrypoints(profile)?;
    for entrypoint in &entrypoints {
        println!(
            "H264ConstrainedBaseline: {} ({entrypoint})",
            entrypoint_name(*entrypoint)
        );
    }
    if !entrypoints.contains(&VAEntrypoint::VAEntrypointEncSlice) {
        return Err("H264ConstrainedBaseline has no EncSlice entrypoint".into());
    }
    println!("ok: H264ConstrainedBaseline + VAEntrypointEncSlice is available");
    Ok(())
}

/// Whether the running kernel is in the range where H.264 encode hangs the VCE (7.2.2 to 7.2.x).
fn kernel_affected() -> Result<(bool, String), Error> {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")?
        .trim()
        .to_owned();
    // "7.2.11-arch1-1" -> [7, 2, 11]
    let numbers: Vec<u32> = release
        .split(|c: char| !c.is_ascii_digit())
        .take(3)
        .map(|n| n.parse().unwrap_or(0))
        .collect();
    let affected = matches!(numbers.as_slice(), [7, 2, patch] if *patch >= 2);
    Ok((affected, release))
}

/// Fills NV12 planes with a gradient and a vertical bar that moves 8 px per frame, so every
/// frame differs and the encoder has real work. The padding up to `coded` is filled too.
fn fill_frame(y: &mut [u8], uv: &mut [u8], pitches: &[usize], coded: &Resolution, index: u32) {
    let (width, height) = (coded.width as usize, coded.height as usize);
    let bar = (index as usize * 8) % width;
    for (row, line) in y.chunks_mut(pitches[Y_PLANE]).take(height).enumerate() {
        for (col, px) in line.iter_mut().take(width).enumerate() {
            let in_bar = col >= bar && col < bar + 64;
            *px = if in_bar {
                235
            } else {
                ((col + row) / 8 % 200 + 16) as u8
            };
        }
    }
    for (row, line) in uv
        .chunks_mut(pitches[UV_PLANE])
        .take(height / 2)
        .enumerate()
    {
        for (i, px) in line.iter_mut().take(width).enumerate() {
            // Even bytes are U, odd bytes are V
            *px = if i % 2 == 0 {
                (96 + row % 64) as u8
            } else {
                (160 - (i / 2) % 64) as u8
            };
        }
    }
}

/// Lists the NAL unit types found in an Annex B stream (start codes 00 00 01).
fn nal_types(stream: &[u8], max: usize) -> Vec<u8> {
    stream
        .windows(4)
        .filter(|w| w[..3] == [0, 0, 1])
        .map(|w| w[3] & 0x1f)
        .take(max)
        .collect()
}

fn encode(display: Rc<Display>, args: &Args) -> Result<(), Error> {
    let visible = args.size;
    // VA-API surfaces are 16-aligned; 1080 becomes 1088
    let coded = Resolution {
        width: visible.width.div_ceil(16) * 16,
        height: visible.height.div_ceil(16) * 16,
    };
    let gbm = GbmDevice::open(RENDER_NODE)?;

    let config = EncoderConfig {
        resolution: visible,
        // Baseline maps to VAProfileH264ConstrainedBaseline
        profile: Profile::Baseline,
        // 4.0 covers 1080p30
        level: Level::L4,
        initial_tunings: Tunings {
            rate_control: RateControl::ConstantBitrate(BITRATE),
            framerate: FRAMERATE,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut encoder = h264::StatelessEncoder::<GenericDmaVideoFrame, _>::new_vaapi(
        display,
        config,
        Fourcc::from(b"NV12"),
        visible,
        false,
        BlockingMode::Blocking,
    )
    .map_err(|e| format!("creating the encoder: {e:?}"))?;

    let out_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("out.h264");
    let mut out = File::create(&out_path)?;
    let mut bitstream_len = 0usize;
    let mut first_bytes = Vec::new();
    let mut coded_frames = 0u32;
    let mut write_coded = |data: &[u8]| -> Result<(), Error> {
        out.write_all(data)?;
        bitstream_len += data.len();
        if first_bytes.len() < 4096 {
            first_bytes.extend_from_slice(&data[..data.len().min(4096)]);
        }
        coded_frames += 1;
        Ok(())
    };

    let mut prepare_time = Duration::ZERO;
    let mut encode_time = Duration::ZERO;
    let started = Instant::now();
    for index in 0..args.frames {
        let t = Instant::now();
        let mut frame = gbm
            .clone()
            .new_frame(Fourcc::from(b"NV12"), visible, coded, GbmUsage::Encode)?
            .to_generic_dma_video_frame()?;
        let pitches = frame.get_plane_pitch();
        {
            let mapping = frame.map_mut()?;
            let planes = mapping.get();
            fill_frame(
                &mut planes[Y_PLANE].borrow_mut(),
                &mut planes[UV_PLANE].borrow_mut(),
                &pitches,
                &coded,
                index,
            );
        }
        prepare_time += t.elapsed();

        let t = Instant::now();
        let meta = FrameMetadata {
            timestamp: u64::from(index),
            layout: Default::default(),
            force_keyframe: false,
        };
        encoder
            .encode(meta, frame)
            .map_err(|e| format!("encode frame {index}: {e:?}"))?;
        while let Some(coded_buffer) = encoder.poll().map_err(|e| format!("poll: {e:?}"))? {
            write_coded(&coded_buffer.bitstream)?;
        }
        encode_time += t.elapsed();
    }

    let t = Instant::now();
    encoder.drain().map_err(|e| format!("drain: {e:?}"))?;
    while let Some(coded_buffer) = encoder.poll().map_err(|e| format!("poll: {e:?}"))? {
        write_coded(&coded_buffer.bitstream)?;
    }
    encode_time += t.elapsed();
    let total = started.elapsed();

    let frames = f64::from(args.frames);
    println!("first NAL types: {:?}", nal_types(&first_bytes, 8));
    println!(
        "{} frames in, {coded_frames} coded buffers out, {} bytes -> {}",
        args.frames,
        bitstream_len,
        out_path.display()
    );
    println!(
        "average per frame: prepare (alloc + fill) {:.2} ms, encode (encode + poll) {:.2} ms",
        prepare_time.as_secs_f64() * 1000.0 / frames,
        encode_time.as_secs_f64() * 1000.0 / frames
    );
    println!(
        "total {:.2} s, {:.1} fps, {:.0} kbit/s at {FRAMERATE} fps",
        total.as_secs_f64(),
        frames / total.as_secs_f64(),
        bitstream_len as f64 * 8.0 * f64::from(FRAMERATE) / frames / 1000.0
    );
    Ok(())
}

fn main() -> Result<(), Error> {
    let args = parse_args()?;
    // Checked before VA-API is opened: this mode only talks to GBM
    if args.alloc_only {
        return alloc_probe::run(RENDER_NODE);
    }
    let display = Display::open().ok_or("could not open a VA-API display")?;
    query(&display)?;

    if !args.encode {
        println!("query only; pass --encode to submit frames to the encoder");
        return Ok(());
    }

    let (affected, release) = kernel_affected()?;
    if affected && !args.ignore_kernel_check {
        return Err(format!(
            "kernel {release} is in the range where H.264 encode hangs the VCE (7.2.2+ before \
             7.3); boot a kernel without the regression, or pass --ignore-kernel-check"
        )
        .into());
    }
    println!(
        "encoding {} frames at {}x{} on kernel {release}",
        args.frames, args.size.width, args.size.height
    );
    encode(display, &args)
}
