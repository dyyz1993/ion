use crate::types::{StreamEvent, AssistantMessage, StopReason};
use tokio::sync::{mpsc, oneshot};

/// A channel-based event stream.
pub struct EventStream {
    rx: mpsc::Receiver<StreamEvent>,
    result_rx: Option<oneshot::Receiver<AssistantMessage>>,
}

impl EventStream {
    pub fn new() -> (Self, EventSender) {
        let (tx, rx) = mpsc::channel(256);
        let (result_tx, result_rx) = oneshot::channel();

        let stream = Self {
            rx,
            result_rx: Some(result_rx),
        };
        let sender = EventSender { tx, result_tx: Some(result_tx) };

        (stream, sender)
    }

    pub async fn recv(&mut self) -> Option<StreamEvent> {
        self.rx.recv().await
    }

    pub async fn result(mut self) -> crate::ProviderResult<AssistantMessage> {
        while self.rx.recv().await.is_some() {}
        self.result_rx
            .take()
            .unwrap()
            .await
            .map_err(|_| crate::ProviderError::Stream("stream ended without result".into()))
    }

    /// Forward events from `inner` to a new EventStream, tapping the final Done/Error message.
    /// `on_done` is called once with the final AssistantMessage BEFORE the stream completes.
    /// Correctly completes the result oneshot via end()/error() — do NOT drop the returned stream early.
    pub fn forward_with_done_tap<F>(
        mut inner: EventStream,
        on_done: F,
    ) -> EventStream
    where
        F: FnOnce(&AssistantMessage) + Send + 'static,
    {
        let (tap_stream, tap_sender) = EventStream::new();
        tokio::spawn(async move {
            let mut final_msg: Option<AssistantMessage> = None;
            while let Some(ev) = inner.recv().await {
                match &ev {
                    StreamEvent::Done { message, .. } => { final_msg = Some(message.clone()); }
                    StreamEvent::Error { message, .. } => { final_msg = Some(message.clone()); }
                    _ => {}
                }
                tap_sender.push(ev);
            }
            // inner ended; call on_done BEFORE consuming msg, then complete the tap's oneshot
            match final_msg {
                Some(msg) => {
                    on_done(&msg);
                    if matches!(msg.stop_reason, StopReason::Error | StopReason::Aborted) {
                        tap_sender.error(msg.stop_reason.clone(), msg);
                    } else {
                        tap_sender.end(msg);
                    }
                }
                None => {
                    // inner ended without Done/Error — tap_sender drops, result() errors "stream ended without result"
                }
            }
        });
        tap_stream
    }
}

pub struct EventSender {
    tx: mpsc::Sender<StreamEvent>,
    result_tx: Option<oneshot::Sender<AssistantMessage>>,
}

impl EventSender {
    pub fn push(&self, event: StreamEvent) {
        let _ = self.tx.try_send(event);
    }

    pub async fn send(&self, event: StreamEvent) {
        let _ = self.tx.send(event).await;
    }

    pub fn end(mut self, message: AssistantMessage) {
        let _ = self.tx.try_send(StreamEvent::Done {
            reason: message.stop_reason.clone(),
            message: message.clone(),
        });
        if let Some(tx) = self.result_tx.take() {
            let _ = tx.send(message);
        }
    }

    pub fn error(mut self, reason: StopReason, message: AssistantMessage) {
        let _ = self.tx.try_send(StreamEvent::Error {
            reason,
            message: message.clone(),
        });
        if let Some(tx) = self.result_tx.take() {
            let _ = tx.send(message);
        }
    }
}
