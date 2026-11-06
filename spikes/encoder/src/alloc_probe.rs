//! `--alloc-only`: tries to allocate NV12 buffers through GBM alone, with several sizes and usage
//! flags, and prints what the driver says. No VA-API display, no encoder, nothing reaches the VCE.
//!
//! cros-codecs allocates NV12 with `GBM_BO_USE_HW_VIDEO_ENCODER` (1 << 14), a flag that exists in
//! ChromeOS minigbm but not in Mesa's gbm.h (which stops at 1 << 6). This shows which flags, if
//! any, Mesa's GBM accepts for NV12 on this GPU.

use gbm_sys as gbm;
use std::{ffi::CStr, fs::OpenOptions, os::fd::AsRawFd};

const NV12: u32 = u32::from_le_bytes(*b"NV12");
const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

const FLAGS: &[(&str, u32)] = &[
    ("none", 0),
    ("SCANOUT", 1 << 0),
    ("RENDERING", 1 << 2),
    ("LINEAR", 1 << 4),
    ("LINEAR|RENDERING", (1 << 4) | (1 << 2)),
    ("HW_VIDEO_DECODER (minigbm only)", 1 << 13),
    (
        "HW_VIDEO_ENCODER (minigbm only, what cros-codecs passes)",
        1 << 14,
    ),
];

const SIZES: &[(u32, u32)] = &[(320, 240), (640, 480), (1920, 1080), (1920, 1088)];

/// Describes a buffer object and destroys it.
///
/// # Safety
/// `bo` must be a non-null buffer object returned by `gbm_bo_create*`, not used afterwards.
unsafe fn describe_and_destroy(bo: *mut gbm::gbm_bo) -> String {
    // SAFETY: the caller guarantees `bo` is a valid buffer object.
    unsafe {
        let planes = gbm::gbm_bo_get_plane_count(bo);
        let modifier = gbm::gbm_bo_get_modifier(bo);
        let layout: Vec<String> = (0..planes)
            .map(|p| {
                format!(
                    "plane {p}: stride {} offset {}",
                    gbm::gbm_bo_get_stride_for_plane(bo, p),
                    gbm::gbm_bo_get_offset(bo, p)
                )
            })
            .collect();
        gbm::gbm_bo_destroy(bo);
        format!(
            "ok, {planes} plane(s), modifier 0x{modifier:016x}, {}",
            layout.join(", ")
        )
    }
}

pub fn run(render_node: &str) -> Result<(), Box<dyn std::error::Error>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(render_node)?;
    // SAFETY: `file` stays open until after `gbm_device_destroy` at the end of this function.
    let device = unsafe { gbm::gbm_create_device(file.as_raw_fd()) };
    if device.is_null() {
        return Err(format!("gbm_create_device failed on {render_node}").into());
    }

    // SAFETY: `device` is valid; the returned string is owned by GBM and lives with the device.
    let backend = unsafe { CStr::from_ptr(gbm::gbm_device_get_backend_name(device)) };
    println!("GBM backend: {}", backend.to_string_lossy());

    println!("\n== gbm_device_is_format_supported(NV12, flags)");
    for (name, flags) in FLAGS {
        // SAFETY: `device` is valid.
        let supported = unsafe { gbm::gbm_device_is_format_supported(device, NV12, *flags) };
        println!(
            "  {name:58} -> {}",
            if supported != 0 { "yes" } else { "no" }
        );
    }

    println!("\n== control: gbm_bo_create(XRGB8888, 1920x1080, RENDERING)");
    // SAFETY: `device` is valid; a null result is handled.
    let bo = unsafe { gbm::gbm_bo_create(device, 1920, 1080, XRGB8888, 1 << 2) };
    if bo.is_null() {
        println!("  failed");
    } else {
        // SAFETY: `bo` is non-null and not used after this call.
        println!("  {}", unsafe { describe_and_destroy(bo) });
    }

    for &(width, height) in SIZES {
        println!("\n== NV12 {width}x{height}");
        for (name, flags) in FLAGS {
            // SAFETY: `device` is valid; a null result is handled.
            let bo = unsafe { gbm::gbm_bo_create(device, width, height, NV12, *flags) };
            let result = if bo.is_null() {
                "failed".to_owned()
            } else {
                // SAFETY: `bo` is non-null and not used after this call.
                unsafe { describe_and_destroy(bo) }
            };
            println!("  gbm_bo_create, {name:58} -> {result}");
        }

        let modifiers = [DRM_FORMAT_MOD_LINEAR];
        // SAFETY: `device` is valid and `modifiers` outlives the call; a null result is handled.
        let bo = unsafe {
            gbm::gbm_bo_create_with_modifiers(
                device,
                width,
                height,
                NV12,
                modifiers.as_ptr(),
                modifiers.len() as u32,
            )
        };
        let result = if bo.is_null() {
            "failed".to_owned()
        } else {
            // SAFETY: `bo` is non-null and not used after this call.
            unsafe { describe_and_destroy(bo) }
        };
        println!(
            "  gbm_bo_create_with_modifiers, [LINEAR] {:41} -> {result}",
            ""
        );
    }

    // SAFETY: every buffer object was destroyed above and `device` is not used afterwards.
    unsafe { gbm::gbm_device_destroy(device) };
    drop(file);
    Ok(())
}
