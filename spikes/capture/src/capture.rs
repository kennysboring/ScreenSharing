//! PipeWire side: connects to the portal's node, negotiates a video format (DMA-BUF if the
//! compositor agrees, shared memory otherwise) and reports each buffer without reading it.

use pipewire as pw;
use pw::{
    properties::properties,
    spa::{
        self,
        param::{
            ParamType,
            format::{FormatProperties, MediaSubtype, MediaType},
            format_utils,
            video::{VideoFlags, VideoFormat, VideoInfoRaw},
        },
        pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value, serialize::PodSerializer},
        utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle, SpaTypes},
    },
};
use std::{io::Cursor, os::fd::OwnedFd, time::Instant};

const FRAMES_TO_REPORT: u32 = 120;

/// Data is laid out row by row; modifier 0 means "no tiling".
const DRM_FORMAT_MOD_LINEAR: i64 = 0;
/// "Whatever the driver picks" (implicit modifier).
const DRM_FORMAT_MOD_INVALID: i64 = 0x00ff_ffff_ffff_ffff;

/// State shared by the stream callbacks.
struct State {
    format: VideoInfoRaw,
    frames: u32,
    first: Option<Instant>,
}

/// The format we offer. `modifier`:
/// - `None`: shared memory only (no modifier property).
/// - `Some(Err(list))`: DMA-BUF, letting the compositor pick from `list` (DONT_FIXATE).
/// - `Some(Ok(m))`: DMA-BUF with modifier `m` fixed (after fixation).
fn enum_format(modifier: Option<Result<i64, &[i64]>>) -> Vec<u8> {
    let mut object = spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 60, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );

    match modifier {
        None => {}
        Some(Err(list)) => object.properties.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE,
            value: Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: list[0],
                    alternatives: list.to_vec(),
                },
            ))),
        }),
        Some(Ok(fixed)) => object.properties.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY,
            value: Value::Long(fixed),
        }),
    }
    serialize(object)
}

/// Which buffer types we accept for the negotiated format.
fn buffers_param(dma_buf: bool) -> Vec<u8> {
    let data_type = if dma_buf {
        1 << spa::sys::SPA_DATA_DmaBuf
    } else {
        (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr)
    };
    serialize(Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![Property {
            key: spa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: PropertyFlags::empty(),
            value: Value::Int(data_type),
        }],
    })
}

fn serialize(object: Object) -> Vec<u8> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .expect("serializing a pod built here cannot fail")
        .0
        .into_inner()
}

fn pods(bytes: &[Vec<u8>]) -> Vec<&Pod> {
    bytes
        .iter()
        .map(|b| Pod::from_bytes(b).expect("bytes come from PodSerializer"))
        .collect()
}

fn describe_modifier(format: &VideoInfoRaw) -> String {
    if format.flags().contains(VideoFlags::MODIFIER) {
        format!("0x{:016x}", format.modifier())
    } else {
        "none (shared memory)".to_owned()
    }
}

/// Runs the PipeWire loop until `FRAMES_TO_REPORT` frames arrived or the stream fails.
pub fn run(node_id: u32, fd: OwnedFd) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    // The portal's fd is the only connection that can see the shared node
    let core = context.connect_fd_rc(fd, None)?;

    let stream = pw::stream::StreamRc::new(
        core,
        "capture-spike",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let state = State {
        format: VideoInfoRaw::default(),
        frames: 0,
        first: None,
    };

    let quit_on_error = mainloop.clone();
    let quit_when_done = mainloop.clone();
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(move |_, _, old, new| {
            println!("stream state: {old:?} -> {new:?}");
            if let pw::stream::StreamState::Error(_) = new {
                quit_on_error.quit();
            }
        })
        .param_changed(|stream, state, id, param| {
            let Some(param) = param else { return };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Ok((MediaType::Video, MediaSubtype::Raw)) = format_utils::parse_format(param)
            else {
                return;
            };
            if state.format.parse(param).is_err() {
                println!("could not parse the negotiated format");
                return;
            }
            let format = state.format;
            let size = format.size();
            let rate = format.framerate();
            let max_rate = format.max_framerate();
            println!(
                "negotiated: {}x{} {:?}, framerate {}/{} (max {}/{}), modifier {}",
                size.width,
                size.height,
                format.format(),
                rate.num,
                rate.denom,
                max_rate.num,
                max_rate.denom,
                describe_modifier(&format)
            );

            let dma_buf = format.flags().contains(VideoFlags::MODIFIER);
            let params = if format.flags().contains(VideoFlags::MODIFIER_FIXATION_REQUIRED) {
                // The modifier is still a list: pick the suggested one and negotiate again
                println!("fixating modifier 0x{:016x}", format.modifier());
                vec![
                    enum_format(Some(Ok(format.modifier() as i64))),
                    enum_format(None),
                ]
            } else {
                vec![buffers_param(dma_buf)]
            };
            if let Err(e) = stream.update_params(&mut pods(&params)) {
                println!("update_params failed: {e}");
            }
        })
        .process(move |stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first() else { return };

            let now = Instant::now();
            let first = *state.first.get_or_insert(now);
            state.frames += 1;
            if state.frames > FRAMES_TO_REPORT {
                return;
            }

            let size = state.format.size();
            let chunk = data.chunk();
            println!(
                "frame {:3}: {}x{} {:?} {:?} modifier {}, {} plane(s), chunk {} B stride {} flags {:?}",
                state.frames,
                size.width,
                size.height,
                state.format.format(),
                data.type_(),
                describe_modifier(&state.format),
                datas.len(),
                chunk.size(),
                chunk.stride(),
                chunk.flags()
            );

            if state.frames == FRAMES_TO_REPORT {
                let elapsed = now - first;
                let fps = f64::from(FRAMES_TO_REPORT - 1) / elapsed.as_secs_f64();
                println!(
                    "effective fps: {fps:.1} ({} frames in {:.2} s)",
                    FRAMES_TO_REPORT,
                    elapsed.as_secs_f64()
                );
                quit_when_done.quit();
            }
        })
        .register()?;

    // First choice DMA-BUF (compositor picks the modifier), then shared memory
    let offers = [
        enum_format(Some(Err(&[DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_MOD_INVALID]))),
        enum_format(None),
    ];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT,
        &mut pods(&offers),
    )?;

    mainloop.run();
    Ok(())
}
