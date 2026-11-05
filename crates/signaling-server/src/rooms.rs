//! State of all rooms and the rules that change it: expiring rooms, registering the host and
//! removing viewers.

use common::{ServerToHost, ServerToViewer, ViewerId};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    sync::mpsc::{Receiver, Sender, channel},
    time,
};
use uuid::Uuid;

pub const CAPACITY_CHANNEL: usize = 32;
pub const MAX_MESSAGE_SIZE: usize = 64 * 1024;

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const HOST_ABSENCE_GRACE: Duration = Duration::from_secs(180);

pub struct Host {
    pub id: Uuid,
    pub sender: Sender<ServerToHost>,
}
pub struct Room {
    pub viewers: HashMap<ViewerId, Sender<ServerToViewer>>,
    pub next_viewer_id: u32,
    pub host: Option<Host>,
    pub host_token: Uuid,
    pub host_absent_since: Option<Instant>,
}

pub struct AllRooms(pub HashMap<String, Room>);

pub type SharedState = Arc<Mutex<AllRooms>>;

/// Every minute, removes the rooms whose host has been away for longer than the grace period.
pub async fn sweep_expired_rooms(state: SharedState) {
    let mut timer = time::interval(SWEEP_INTERVAL);

    loop {
        timer.tick().await;
        {
            // .lock() in
            let mut guard = state.lock().unwrap();

            remove_expired_rooms(&mut guard.0, Instant::now(), HOST_ABSENCE_GRACE);
        } // .lock() out
    }
}

fn remove_expired_rooms(rooms: &mut HashMap<String, Room>, now: Instant, limit: Duration) {
    rooms.retain(|_id, room| match room.host_absent_since {
        Some(time) => now.saturating_duration_since(time) < limit,
        None => true,
    });
}

pub fn remove_viewer(state: &SharedState, room_id: &str, id: ViewerId) {
    {
        // .lock in
        let mut guard = state.lock().unwrap();

        if let Some(room) = guard.0.get_mut(room_id)
            && room.viewers.remove(&id).is_some()
            && let Some(host) = &room.host
        {
            let _ = host
                .sender
                .try_send(ServerToHost::ViewerLeft { viewer_id: id });
        }
    } // .lock out
}

pub fn register_host(room: &mut Room) -> (Receiver<ServerToHost>, Uuid) {
    let id = Uuid::new_v4();
    let (sender, receiver) = channel::<ServerToHost>(CAPACITY_CHANNEL);

    let mut viewers: Vec<ViewerId> = room.viewers.keys().copied().collect();
    viewers.sort_by_key(|v| v.0);

    let _ = sender.try_send(ServerToHost::ViewerList { viewers });

    room.host = Some(Host { id, sender });
    room.host_absent_since = None;
    (receiver, id)
}

/// Whether `id` is the host currently registered in the room. A host connection that was
/// replaced by a newer one is no longer the current host.
pub fn is_current_host(room: &Room, id: Uuid) -> bool {
    matches!(&room.host, Some(host) if host.id == id)
}

/// Marks the room as having no host, but only if `id_host` is still the current host, so an
/// old connection that ends does not remove the host that replaced it.
pub fn unregister_host(room: &mut Room, id_host: Uuid, now: Instant) {
    if !is_current_host(room, id_host) {
        return;
    }

    room.host = None;
    room.host_absent_since = Some(now);

    for viewer in room.viewers.values() {
        let _ = viewer.try_send(ServerToViewer::HostUnavailable);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room_with(host_absent_since: Option<Instant>) -> Room {
        Room {
            viewers: HashMap::new(),
            next_viewer_id: 0,
            host: None,
            host_token: Uuid::new_v4(),
            host_absent_since,
        }
    }

    #[test]
    fn test_remove_expired_rooms_absent_beyond_grace() {
        let mut rooms = HashMap::new();
        let since = Instant::now();
        let grace = HOST_ABSENCE_GRACE;

        rooms.insert("room1".to_string(), room_with(Some(since)));

        // Absent beyond the grace period (1 second more)
        let now = since + grace + Duration::from_secs(1);
        remove_expired_rooms(&mut rooms, now, grace);

        assert!(rooms.is_empty());
    }

    #[test]
    fn test_remove_expired_rooms_absent_within_grace() {
        let mut rooms = HashMap::new();
        let since = Instant::now();
        let grace = HOST_ABSENCE_GRACE;

        rooms.insert("room1".to_string(), room_with(Some(since)));

        // Absent for less than the grace period
        let now = since + Duration::from_secs(100);
        remove_expired_rooms(&mut rooms, now, grace);

        assert_eq!(rooms.len(), 1);
        assert!(rooms.contains_key("room1"));
    }

    #[test]
    fn test_remove_expired_rooms_host_present() {
        let mut rooms = HashMap::new();
        let since = Instant::now();
        let grace = HOST_ABSENCE_GRACE;

        // Host present (host_absent_since = None)
        rooms.insert("room1".to_string(), room_with(None));

        // Even with `now` far in the future (e.g. 100 hours later)
        let now = since + Duration::from_secs(360_000);
        remove_expired_rooms(&mut rooms, now, grace);

        assert_eq!(rooms.len(), 1);
        assert!(rooms.contains_key("room1"));
    }

    #[test]
    fn test_remove_expired_rooms_exact_boundary_and_transition() {
        let mut rooms = HashMap::new();
        let since = Instant::now();
        let grace = HOST_ABSENCE_GRACE;

        // 1. Instant right before the grace period ends (now = since + grace - 1 ns): must keep
        rooms.insert("room1".to_string(), room_with(Some(since)));
        let now_before = since + grace - Duration::from_nanos(1);
        remove_expired_rooms(&mut rooms, now_before, grace);
        assert_eq!(
            rooms.len(),
            1,
            "The room should be kept 1ns before reaching the exact grace period"
        );

        // 2. Exact boundary (now = since + grace): with '<', the delta equals the limit, giving false -> removed
        let now_exact = since + grace;
        remove_expired_rooms(&mut rooms, now_exact, grace);
        assert!(
            rooms.is_empty(),
            "The room should be removed at the exact end of the grace period (now == since + grace)"
        );
    }

    #[test]
    fn test_remove_expired_rooms_isolates_expired_entry() {
        let mut rooms = HashMap::new();
        let since = Instant::now();
        let grace = HOST_ABSENCE_GRACE;

        // Room 1: absent since `since` (expires at `now`)
        rooms.insert("expired_room".to_string(), room_with(Some(since)));

        // Room 2: absent more recently (will stay valid)
        let recent = since + Duration::from_secs(100);
        rooms.insert("valid_room".to_string(), room_with(Some(recent)));

        let now = since + grace;
        remove_expired_rooms(&mut rooms, now, grace);

        assert_eq!(rooms.len(), 1);
        assert!(rooms.contains_key("valid_room"));
        assert!(!rooms.contains_key("expired_room"));
    }

    #[test]
    fn test_register_host_with_viewers() {
        let mut room = room_with(None);

        // Insert five viewers (0 to 4). Since HashMap iteration order is not deterministic,
        // a sample of 5 elements lowers the chance of accidental ordering to 1 in 120 (1/5!).
        for id in [3, 0, 4, 1, 2] {
            let (tx, _rx) = channel(1);
            room.viewers.insert(ViewerId(id), tx);
        }

        let (mut rx, _id) = register_host(&mut room);

        let msg = rx.try_recv().expect("should receive ViewerList");
        assert_eq!(
            msg,
            ServerToHost::ViewerList {
                viewers: vec![
                    ViewerId(0),
                    ViewerId(1),
                    ViewerId(2),
                    ViewerId(3),
                    ViewerId(4),
                ]
            }
        );
    }

    #[test]
    fn test_register_host_empty_viewers() {
        let mut room = room_with(None);

        let (mut rx, _id) = register_host(&mut room);

        let msg = rx.try_recv().expect("should receive an empty ViewerList");
        assert_eq!(msg, ServerToHost::ViewerList { viewers: vec![] });
    }

    #[test]
    fn test_register_host_state() {
        // Start with Some(Instant::now()) to prove the function clears it
        let mut room = room_with(Some(Instant::now()));
        assert!(room.host.is_none());
        assert!(room.host_absent_since.is_some());

        let (_rx, id) = register_host(&mut room);

        assert!(is_current_host(&room, id));
        assert_eq!(room.host_absent_since, None);
    }

    #[test]
    fn test_register_host_replacement() {
        use tokio::sync::mpsc::error::TryRecvError;

        let mut room = room_with(None);

        let (mut rx1, id1) = register_host(&mut room);
        let (_rx2, id2) = register_host(&mut room);

        assert_ne!(id1, id2);
        assert!(is_current_host(&room, id2));

        // The first receiver still delivers the message that was queued
        let msg = rx1
            .try_recv()
            .expect("first receiver should deliver the queued message");
        assert_eq!(msg, ServerToHost::ViewerList { viewers: vec![] });

        // Only then does it return TryRecvError::Disconnected, because the old sender was dropped
        assert_eq!(rx1.try_recv(), Err(TryRecvError::Disconnected));
    }

    #[test]
    fn test_is_current_host() {
        let mut room = room_with(None);
        assert!(!is_current_host(&room, Uuid::new_v4()));

        let (_rx1, id1) = register_host(&mut room);
        assert!(is_current_host(&room, id1));
        assert!(!is_current_host(&room, Uuid::new_v4()));

        // After a replacement, only the newest connection is the current host
        let (_rx2, id2) = register_host(&mut room);
        assert!(!is_current_host(&room, id1));
        assert!(is_current_host(&room, id2));
    }

    #[test]
    fn test_unregister_host_current_host_leaves() {
        let mut room = room_with(None);
        let (viewer_tx, mut viewer_rx) = channel(4);
        room.viewers.insert(ViewerId(0), viewer_tx);
        let (_rx, id) = register_host(&mut room);

        let now = Instant::now();
        unregister_host(&mut room, id, now);

        assert!(room.host.is_none(), "the host that left must be removed");
        assert_eq!(room.host_absent_since, Some(now));
        assert_eq!(viewer_rx.try_recv(), Ok(ServerToViewer::HostUnavailable));
    }

    #[test]
    fn test_unregister_host_old_connection_keeps_new_host() {
        use tokio::sync::mpsc::error::TryRecvError;

        let mut room = room_with(None);
        let (viewer_tx, mut viewer_rx) = channel(4);
        room.viewers.insert(ViewerId(0), viewer_tx);
        let (_rx1, id1) = register_host(&mut room);
        let (mut rx2, id2) = register_host(&mut room);

        // The replaced connection ends after the new host registered
        unregister_host(&mut room, id1, Instant::now());

        assert!(is_current_host(&room, id2), "the new host must stay");
        assert_eq!(room.host_absent_since, None);
        assert!(
            viewer_rx.try_recv().is_err(),
            "viewers must not be told the host left"
        );

        // The new host channel is still open: its queued ViewerList, then Empty (not Disconnected)
        assert!(matches!(
            rx2.try_recv(),
            Ok(ServerToHost::ViewerList { .. })
        ));
        assert_eq!(rx2.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn test_unregister_host_lets_room_expire() {
        let mut rooms = HashMap::new();
        let mut room = room_with(None);
        let (_rx, id) = register_host(&mut room);

        let left_at = Instant::now();
        unregister_host(&mut room, id, left_at);
        rooms.insert("room1".to_string(), room);

        // Once the host left, the room expires after the grace period
        remove_expired_rooms(&mut rooms, left_at + HOST_ABSENCE_GRACE, HOST_ABSENCE_GRACE);
        assert!(rooms.is_empty());
    }
}
