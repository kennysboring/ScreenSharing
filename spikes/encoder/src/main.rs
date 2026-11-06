//! Encoder spike: H.264 Constrained Baseline through cros-libva directly (no cros-codecs
//! encoder; GBM only allocates the RGB input of the VPP modes).
//!
//! - no flags: only queries the driver (safe on any kernel);
//! - `--prepend [--size WxH]`: CPU only, writes SPS + PPS + `out-idr.h264` to
//!   `out-idr-prepended.h264` (no VA-API, no GPU);
//! - `--psnr <decoded.yuv> [--frames N] [--gop N] [--force-idr-at N] [--size WxH]`: CPU only,
//!   per-frame PSNR of a decoded yuv420p file against the synthetic IPPP frames;
//! - `--encode-idr [--no-packed-headers] [--size WxH]`: encodes one IDR frame. By default it sends
//!   packed SPS, PPS and slice headers, so the driver writes SPS/PPS itself (the ffmpeg path);
//!   `--no-packed-headers` gets only the slice, to which `--prepend` adds them;
//! - `--encode-ippp [--frames N] [--gop N] [--force-idr-at N] [--size WxH]`: encodes an IPPP
//!   sequence to `out-ippp.h264`.
//!
//! - `--vpp-only [--size WxH]`: imports a GBM XRGB8888 buffer, converts it to NV12 with the VPP
//!   and checks the colors on the CPU (VideoProc only, nothing reaches the VCE);
//! - `--encode-vpp [--frames N] [--gop N] [--size WxH]`: the VPP output feeds the IPPP encoder,
//!   written to `out-vpp.h264` with BT.709 limited range in the SPS VUI;
//! - `--psnr-bars <decoded.yuv> [--frames N] [--size WxH]`: CPU only, PSNR against the pattern.
//!
//! Every mode that touches the GPU refuses to run unless the kernel is 6.18 LTS: on 7.2.2+
//! H.264 encode hangs the RX 580's VCE.

mod encode;
mod query;
mod sequence;
mod vpp;

use libva::Display;

type Error = Box<dyn std::error::Error>;

/// Only kernel line where H.264 encode was confirmed not to hang the VCE on this machine.
const SAFE_KERNEL_PREFIX: &str = "6.18.";

fn main() -> Result<(), Error> {
    let mut encode_idr = false;
    let mut encode_ippp = false;
    let mut vpp_only = false;
    let mut encode_vpp = false;
    let mut psnr_bars: Option<String> = None;
    let mut prepend = false;
    let mut psnr_file: Option<String> = None;
    let mut opts = encode::Options {
        width: 1920,
        height: 1080,
        packed_headers: true,
    };
    let mut seq = sequence::SeqOptions {
        width: 1920,
        height: 1080,
        frames: 120,
        gop: 30,
        force_idr_at: None,
        bt709_vui: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--encode-idr" => encode_idr = true,
            "--encode-ippp" => encode_ippp = true,
            "--vpp-only" => vpp_only = true,
            "--encode-vpp" => encode_vpp = true,
            "--psnr-bars" => psnr_bars = Some(args.next().ok_or("--psnr-bars needs a file")?),
            "--prepend" => prepend = true,
            "--psnr" => psnr_file = Some(args.next().ok_or("--psnr needs a file")?),
            "--no-packed-headers" => opts.packed_headers = false,
            "--frames" => seq.frames = args.next().ok_or("--frames needs N")?.parse()?,
            "--gop" => seq.gop = args.next().ok_or("--gop needs N")?.parse()?,
            "--force-idr-at" => {
                seq.force_idr_at = Some(args.next().ok_or("--force-idr-at needs N")?.parse()?)
            }
            "--size" => {
                let value = args.next().ok_or("--size needs WxH")?;
                let (w, h) = value.split_once('x').ok_or("--size needs WxH")?;
                (opts.width, opts.height) = (w.parse()?, h.parse()?);
                (seq.width, seq.height) = (opts.width, opts.height);
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    if seq.frames == 0 || seq.gop == 0 {
        return Err("--frames and --gop must be > 0".into());
    }

    // CPU-only modes, checked before VA-API is opened
    if let Some(path) = psnr_file {
        return sequence::verify_psnr(&path, &seq);
    }
    if let Some(path) = psnr_bars {
        return vpp::verify_bars_psnr(&path, seq.width, seq.height, seq.frames);
    }
    if prepend {
        // SPS + PPS (same values the encoder sent to VA) followed by the slice the driver wrote
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let slice = std::fs::read(dir.join("out-idr.h264"))?;
        let (sps, pps) = encode::build_headers(opts.width, opts.height)?;
        let mut stream = encode::header_bytes(&sps, &pps)?;
        stream.extend_from_slice(&slice);
        let out = dir.join("out-idr-prepended.h264");
        std::fs::write(&out, &stream)?;
        println!(
            "wrote {} bytes to {}; NAL types: {:?}",
            stream.len(),
            out.display(),
            encode::nal_types(&stream)
        );
        return Ok(());
    }

    let display = Display::open().ok_or("could not open a VA-API display")?;
    let caps = query::run(&display)?;
    if caps.rate_control & libva::VA_RC_CQP == 0 {
        return Err("the driver does not offer CQP rate control".into());
    }
    let needed = libva::VA_ENC_PACKED_HEADER_SEQUENCE
        | libva::VA_ENC_PACKED_HEADER_PICTURE
        | libva::VA_ENC_PACKED_HEADER_SLICE;
    if (encode_ippp || opts.packed_headers) && caps.packed_headers & needed != needed {
        return Err("the driver does not accept packed SPS/PPS/slice headers".into());
    }
    if !encode_idr && !encode_ippp && !vpp_only && !encode_vpp {
        println!("query only; pass --encode-idr, --encode-ippp, --vpp-only or --encode-vpp");
        return Ok(());
    }

    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")?;
    let release = release.trim();
    if !release.starts_with(SAFE_KERNEL_PREFIX) {
        return Err(format!(
            "kernel {release}: encoding is only allowed on {SAFE_KERNEL_PREFIX}x (LTS); \
             H.264 encode hangs the VCE on 7.2.2+"
        )
        .into());
    }
    if vpp_only {
        println!(
            "VPP only (no encoder), {}x{} on kernel {release}",
            seq.width, seq.height
        );
        return vpp::vpp_only(display, seq.width, seq.height);
    }
    if encode_vpp {
        seq.bt709_vui = true;
        println!(
            "encoding {} frames IPPP from the VPP, {}x{}, IDR every {} on kernel {release}",
            seq.frames, seq.width, seq.height, seq.gop
        );
        return vpp::encode_vpp(display, &seq);
    }
    if encode_ippp {
        println!(
            "encoding {} frames IPPP {}x{}, IDR every {}{} on kernel {release}",
            seq.frames,
            seq.width,
            seq.height,
            seq.gop,
            seq.force_idr_at
                .map_or(String::new(), |n| format!(" + forced at {n}"))
        );
        return sequence::encode_sequence(display, &seq);
    }
    println!(
        "encoding one IDR {}x{} on kernel {release}, packed headers: {}",
        opts.width, opts.height, opts.packed_headers
    );
    encode::encode_idr(display, &opts)
}
