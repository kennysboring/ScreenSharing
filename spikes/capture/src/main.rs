//! Capture spike: opens a ScreenCast session through the portal and reports what PipeWire
//! delivers (size, pixel format, buffer type, DMA-BUF modifier, fps). No pixels are read.

use ashpd::desktop::{
    PersistMode, Session,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use std::os::fd::OwnedFd;

mod capture;

type Error = Box<dyn std::error::Error + Send + Sync>;

/// What the portal hands over: the PipeWire node to read and a connection that can see it.
struct Portal {
    proxy: Screencast,
    session: Session<Screencast>,
    node_id: u32,
    size: Option<(i32, i32)>,
    fd: OwnedFd,
}

/// Asks the user to pick a screen. `start` shows the portal dialog and waits for the answer.
async fn open_portal() -> Result<Portal, Error> {
    let proxy = Screencast::new().await?;
    let session = proxy.create_session(Default::default()).await?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_sources(SourceType::MONITOR)
                // The cursor is drawn into the frames, as a screen share would need
                .set_cursor_mode(CursorMode::EMBEDDED)
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await?;

    let response = proxy
        .start(&session, None, Default::default())
        .await?
        .response()?;
    let stream = response
        .streams()
        .first()
        .ok_or("the portal returned no stream (was the dialog cancelled?)")?;
    let node_id = stream.pipe_wire_node_id();
    let size = stream.size();

    // Only this connection can see the node: the portal exposes just the chosen stream
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await?;

    Ok(Portal {
        proxy,
        session,
        node_id,
        size,
        fd,
    })
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let portal = open_portal().await?;
    println!(
        "portal: node {}, size reported by the portal {:?}",
        portal.node_id, portal.size
    );

    // The PipeWire loop blocks its thread, so it runs off the tokio workers. Only the node id
    // and the fd cross over; every PipeWire object is created on that thread.
    let node_id = portal.node_id;
    let fd = portal.fd;
    let captured = tokio::task::spawn_blocking(move || capture::run(node_id, fd)).await?;

    // The session does not close on drop; close it so the compositor stops the share
    drop(portal.proxy);
    portal.session.close().await?;
    captured?;
    Ok(())
}
