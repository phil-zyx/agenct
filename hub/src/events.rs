//! True Server-Sent Events (SSE) bus scoped by user_id, replacing 7x3s frontend polling.
use crate::{Hub, Result};
use axum::{
    extract::State,
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::stream::{self, Stream};
use serde_json::Value;
use std::{convert::Infallible, time::Duration};
use tokio::sync::broadcast;

#[derive(Clone, Debug)]
pub struct UserEvent {
    pub user_id: String,
    pub kind: String,
    pub payload: Value,
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<UserEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(512);
        Self { tx }
    }
}

impl EventBus {
    pub fn emit(&self, user_id: impl Into<String>, kind: impl Into<String>, payload: Value) {
        let _ = self.tx.send(UserEvent {
            user_id: user_id.into(),
            kind: kind.into(),
            payload,
        });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<UserEvent> {
        self.tx.subscribe()
    }
}

pub async fn stream(
    State(hub): State<Hub>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>> {
    let user = hub.require_user(&headers).await?;
    let rx = hub.events.subscribe();
    let user_id = user.id;

    let initial = stream::once(async move {
        Ok::<Event, Infallible>(
            Event::default()
                .event("connected")
                .data(serde_json::json!({"ok": true}).to_string()),
        )
    });

    let updates = stream::unfold((rx, user_id), |(mut rx, uid)| async move {
        loop {
            match rx.recv().await {
                Ok(ev) if ev.user_id == uid => {
                    let event = Event::default().event(ev.kind).data(ev.payload.to_string());
                    return Some((Ok::<Event, Infallible>(event), (rx, uid)));
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let event = Event::default()
                        .event("resync")
                        .data(serde_json::json!({"reason": "lagged"}).to_string());
                    return Some((Ok::<Event, Infallible>(event), (rx, uid)));
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    use futures_util::StreamExt;
    Ok(Sse::new(initial.chain(updates)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(20))
            .text("ping"),
    ))
}
