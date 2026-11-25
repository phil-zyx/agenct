//! True Server-Sent Events (SSE) bus scoped by user_id, replacing 7x3s frontend polling.
use crate::{auth::AuthUser, Hub, Result};
use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::stream::{self, Stream};
use serde_json::Value;
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::broadcast;

const PER_USER_CHANNEL_CAP: usize = 256;

#[derive(Clone, Debug)]
pub struct SseEvent {
    pub kind: String,
    pub data: String,
}

#[derive(Clone, Default)]
pub struct EventBus {
    channels: Arc<Mutex<HashMap<String, broadcast::Sender<SseEvent>>>>,
}

impl EventBus {
    pub fn emit(&self, user_id: impl Into<String>, kind: impl Into<String>, payload: Value) {
        let uid = user_id.into();
        let sender = {
            let mut map = self.channels.lock().expect("event bus mutex");
            match map.get(&uid) {
                Some(tx) if tx.receiver_count() > 0 => Some(tx.clone()),
                Some(_) => {
                    map.remove(&uid);
                    None
                }
                None => None,
            }
        };
        if let Some(tx) = sender {
            let _ = tx.send(SseEvent {
                kind: kind.into(),
                data: payload.to_string(),
            });
        }
    }

    pub fn subscribe(&self, user_id: &str) -> broadcast::Receiver<SseEvent> {
        let mut map = self.channels.lock().expect("event bus mutex");
        if map.len() > 64 {
            map.retain(|_, tx| tx.receiver_count() > 0);
        }
        let tx = map
            .entry(user_id.to_owned())
            .or_insert_with(|| broadcast::channel(PER_USER_CHANNEL_CAP).0);
        tx.subscribe()
    }
}

pub async fn stream(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
) -> Result<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>> {
    let rx = hub.events.subscribe(&user.id);

    let initial = stream::once(async move {
        Ok::<Event, Infallible>(
            Event::default()
                .event("connected")
                .data(serde_json::json!({"ok": true}).to_string()),
        )
    });

    let updates = stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(ev) => {
                let event = Event::default().event(ev.kind).data(ev.data);
                Some((Ok::<Event, Infallible>(event), rx))
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let event = Event::default()
                    .event("resync")
                    .data(serde_json::json!({"reason": "lagged"}).to_string());
                Some((Ok::<Event, Infallible>(event), rx))
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    });

    use futures_util::StreamExt;
    Ok(Sse::new(initial.chain(updates)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(20))
            .text("ping"),
    ))
}

#[cfg(test)]
mod tests {
    use super::EventBus;
    use serde_json::json;

    #[test]
    fn per_user_channels_isolate_events_and_prune_idle_entries() {
        let bus = EventBus::default();
        let mut rx_a = bus.subscribe("u-a");
        let mut rx_b = bus.subscribe("u-b");

        bus.emit("u-a", "message", json!({"n": 1}));
        bus.emit("u-b", "task", json!({"n": 2}));

        let ev_a = rx_a.try_recv().expect("u-a receives own event");
        assert_eq!(ev_a.kind, "message");
        assert!(rx_a.try_recv().is_err(), "u-a must not receive u-b events");

        let ev_b = rx_b.try_recv().expect("u-b receives own event");
        assert_eq!(ev_b.kind, "task");
        assert!(rx_b.try_recv().is_err(), "u-b must not receive u-a events");

        drop(rx_a);
        bus.emit("u-a", "message", json!({"n": 3}));
        assert_eq!(bus.channels.lock().unwrap().len(), 1);
    }
}
