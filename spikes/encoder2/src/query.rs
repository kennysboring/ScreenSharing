//! Driver queries only (vaGetConfigAttributes): nothing is created and nothing reaches the VCE.

use libva::{Display, VAConfigAttrib, VAConfigAttribType, VAEntrypoint, VAProfile};

pub const PROFILE: VAProfile::Type = VAProfile::VAProfileH264ConstrainedBaseline;
pub const ENTRYPOINT: VAEntrypoint::Type = VAEntrypoint::VAEntrypointEncSlice;

/// What the driver supports for H.264 Constrained Baseline encode.
pub struct Caps {
    pub rate_control: u32,
    pub packed_headers: u32,
}

fn bit_names(value: u32, names: &[(u32, &str)]) -> String {
    let found: Vec<&str> = names
        .iter()
        .filter(|(bit, _)| value & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if found.is_empty() {
        "none".to_owned()
    } else {
        found.join(" | ")
    }
}

pub fn run(display: &Display) -> Result<Caps, Box<dyn std::error::Error>> {
    println!("vendor: {}", display.query_vendor_string()?);
    if !display.query_config_profiles()?.contains(&PROFILE) {
        return Err("H264ConstrainedBaseline is not offered".into());
    }
    if !display
        .query_config_entrypoints(PROFILE)?
        .contains(&ENTRYPOINT)
    {
        return Err("H264ConstrainedBaseline has no EncSlice entrypoint".into());
    }
    println!("H264ConstrainedBaseline + EncSlice: available");

    let kinds = [
        VAConfigAttribType::VAConfigAttribRTFormat,
        VAConfigAttribType::VAConfigAttribRateControl,
        VAConfigAttribType::VAConfigAttribEncPackedHeaders,
        VAConfigAttribType::VAConfigAttribEncMaxRefFrames,
        VAConfigAttribType::VAConfigAttribEncMaxSlices,
    ];
    let mut attrs: Vec<VAConfigAttrib> = kinds
        .iter()
        .map(|&type_| VAConfigAttrib { type_, value: 0 })
        .collect();
    display.get_config_attributes(PROFILE, ENTRYPOINT, &mut attrs)?;
    let value = |i: usize| attrs[i].value;
    let unsupported = |v: u32| v == libva::VA_ATTRIB_NOT_SUPPORTED;

    println!(
        "RTFormat:       0x{:08x} ({})",
        value(0),
        bit_names(value(0), &[(libva::VA_RT_FORMAT_YUV420, "YUV420")])
    );
    println!(
        "RateControl:    0x{:08x} ({})",
        value(1),
        bit_names(
            value(1),
            &[
                (libva::VA_RC_CBR, "CBR"),
                (libva::VA_RC_VBR, "VBR"),
                (libva::VA_RC_CQP, "CQP"),
                (libva::VA_RC_VBR_CONSTRAINED, "VBR_CONSTRAINED"),
                (libva::VA_RC_ICQ, "ICQ"),
                (libva::VA_RC_QVBR, "QVBR"),
            ]
        )
    );
    let packed = if unsupported(value(2)) { 0 } else { value(2) };
    println!(
        "PackedHeaders:  0x{:08x} ({})",
        value(2),
        bit_names(
            packed,
            &[
                (libva::VA_ENC_PACKED_HEADER_SEQUENCE, "SEQUENCE"),
                (libva::VA_ENC_PACKED_HEADER_PICTURE, "PICTURE"),
                (libva::VA_ENC_PACKED_HEADER_SLICE, "SLICE"),
                (libva::VA_ENC_PACKED_HEADER_MISC, "MISC"),
                (libva::VA_ENC_PACKED_HEADER_RAW_DATA, "RAW_DATA"),
            ]
        )
    );
    // MaxRefFrames: low 16 bits for list 0, high 16 bits for list 1
    println!(
        "MaxRefFrames:   0x{:08x} (L0 {}, L1 {})",
        value(3),
        value(3) & 0xffff,
        value(3) >> 16
    );
    println!("MaxSlices:      0x{:08x}", value(4));

    Ok(Caps {
        rate_control: value(1),
        packed_headers: packed,
    })
}
