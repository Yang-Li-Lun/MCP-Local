use std::{
    collections::{HashMap, HashSet},
    os::windows::io::OwnedHandle,
    sync::{Arc, Mutex},
    time::Instant,
};

use rmcp::{
    RoleServer,
    model::{ClientNotification, ClientRequest, JsonRpcMessage, RequestId, ServerResult},
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};
use serde_json::Value;

use super::{Bus, Mode, Record, Status};

const MAX_PENDING: usize = 32;
const MAX_AMBIGUOUS: usize = 128;

struct Captured {
    record: Record,
    started: Instant,
    epoch: u64,
}

impl Captured {
    fn finish(mut self, bus: &Bus, status: Status, response: Option<Value>, note: Option<&str>) {
        self.record.finish(status, self.started, response, note);
        bus.publish(self.epoch, &self.record);
    }
}

struct Pending {
    captured: Option<Captured>,
    sequence: u64,
    sending: bool,
}

#[derive(Default)]
struct Captures {
    pending: HashMap<RequestId, Pending>,
    ambiguous: HashSet<RequestId>,
    sequence: u64,
    saturated: bool,
    mode: Mode,
}

impl Captures {
    fn request(
        &mut self,
        bus: &Bus,
        id: &RequestId,
        tool: Option<&str>,
        request: &impl serde::Serialize,
    ) {
        if matches!(id, RequestId::String(id) if id.len() > 1024) {
            return;
        }
        let tracked = tool.and_then(|tool| {
            let epoch = bus.epoch()?;
            let started = Instant::now();
            let record = Record::begin(serde_json::to_value(request).ok()?, tool, self.mode)?;
            Some(Captured {
                record,
                epoch,
                started,
            })
        });
        if let Some(previous) = self.pending.remove(id) {
            if self.ambiguous.len() < MAX_AMBIGUOUS {
                self.ambiguous.insert(id.clone());
            } else {
                self.saturated = true;
            }
            if let Some(previous) = previous.captured {
                previous.finish(
                    bus,
                    Status::Ambiguous,
                    None,
                    Some("有另一筆未完成請求使用同一 ID。"),
                );
            }
        }
        if self.ambiguous.contains(id) {
            if let Some(tracked) = tracked {
                tracked.finish(
                    bus,
                    Status::Ambiguous,
                    None,
                    Some("請求 ID 重複，無法對應回應。"),
                );
            }
        } else if self.saturated || self.pending.len() >= MAX_PENDING || self.sequence == u64::MAX {
            self.saturated = true;
            if let Some(tracked) = tracked {
                tracked.finish(bus, Status::Incomplete, None, Some("請求監控對應表已滿。"));
            }
        } else {
            if let Some(tracked) = &tracked {
                bus.publish(tracked.epoch, &tracked.record);
            }
            self.sequence += 1;
            self.pending.insert(
                id.clone(),
                Pending {
                    captured: tracked,
                    sequence: self.sequence,
                    sending: false,
                },
            );
        }
    }

    fn claim(&mut self, id: &RequestId) -> Option<(u64, bool)> {
        let tracked = self.pending.get_mut(id)?;
        if tracked.sending {
            return None;
        }
        tracked.sending = true;
        Some((tracked.sequence, tracked.captured.is_some()))
    }

    fn take(&mut self, id: &RequestId, sequence: u64) -> Option<Captured> {
        if self.pending.get(id)?.sequence == sequence {
            self.pending.remove(id)?.captured
        } else {
            None
        }
    }
}

struct Sending {
    bus: Arc<Bus>,
    captures: Arc<Mutex<Captures>>,
    id: RequestId,
    sequence: u64,
}

impl Sending {
    fn finish(&self, status: Status, response: Option<Value>, note: Option<&str>) {
        let tracked = self
            .captures
            .lock()
            .ok()
            .and_then(|mut state| state.take(&self.id, self.sequence));
        if let Some(tracked) = tracked {
            tracked.finish(&self.bus, status, response, note);
        }
    }
}

impl Drop for Sending {
    fn drop(&mut self) {
        self.finish(Status::Incomplete, None, Some("回應送出未確認。"));
    }
}

pub struct ObservedTransport<T> {
    inner: T,
    bus: Arc<Bus>,
    captures: Arc<Mutex<Captures>>,
    attachment: Option<OwnedHandle>,
}

impl<T> ObservedTransport<T> {
    pub fn new(inner: T, bus: Arc<Bus>, mode: Mode) -> Self {
        let attachment = bus.mark_attached();
        Self {
            inner,
            bus,
            captures: Arc::new(Mutex::new(Captures {
                mode,
                ..Captures::default()
            })),
            attachment,
        }
    }
}

impl<T: Transport<RoleServer>> Transport<RoleServer> for ObservedTransport<T> {
    type Error = T::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let id_status = match &item {
            JsonRpcMessage::Response(response) => Some((
                &response.id,
                match &response.result {
                    ServerResult::CallToolResult(result) if result.is_error == Some(true) => {
                        Status::Error
                    }
                    ServerResult::CallToolResult(_) => Status::Success,
                    ServerResult::CustomResult(result) if result.0["isError"] == true => {
                        Status::Error
                    }
                    ServerResult::CustomResult(result) if result.0["content"].is_array() => {
                        Status::Success
                    }
                    _ => Status::Incomplete,
                },
            )),
            JsonRpcMessage::Error(error) => error.id.as_ref().map(|id| (id, Status::Error)),
            _ => None,
        };
        // The stdio writer yields only after flushing the frame. UI status follows
        // that result, while the original SDK frame continues through unchanged.
        let tracked = id_status.and_then(|(id, status)| {
            let (sequence, capture) = self.captures.lock().ok()?.claim(id)?;
            let sending = Sending {
                bus: self.bus.clone(),
                captures: self.captures.clone(),
                id: id.clone(),
                sequence,
            };
            Some((
                sending,
                status,
                capture.then(|| serde_json::to_value(&item).ok()).flatten(),
            ))
        });
        let send = self.inner.send(item);
        async move {
            let result = send.await;
            if let Some((sending, status, response)) = tracked {
                if result.is_err() {
                    sending.finish(Status::TransportError, None, Some("STDIO 回應送出失敗。"));
                } else if response.is_none() {
                    sending.finish(Status::Incomplete, None, Some("回應已送出，擷取失敗。"));
                } else {
                    sending.finish(status, response, None);
                }
            }
            result
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        let item = self.inner.receive().await?;
        match &item {
            JsonRpcMessage::Request(request) => {
                let tool = match &request.request {
                    ClientRequest::CallToolRequest(call) => Some(call.params.name.as_ref()),
                    _ => None,
                };
                if let Ok(mut captures) = self.captures.lock() {
                    // Other MCP methods share the request ID namespace. A duplicate
                    // ID must never attach a different method's response to a tool.
                    captures.request(&self.bus, &request.id, tool, &item);
                }
            }
            JsonRpcMessage::Notification(notification) => {
                if let ClientNotification::CancelledNotification(cancel) =
                    &notification.notification
                {
                    if let Some(id) = &cancel.params.request_id {
                        let tracked = self.captures.lock().ok().and_then(|mut captures| {
                            // Retain the ID until the matching frame/close, even
                            // after an explicit cancellation retires the UI row.
                            captures.pending.get_mut(id)?.captured.take()
                        });
                        if let Some(tracked) = tracked {
                            tracked.finish(
                                &self.bus,
                                Status::Cancelled,
                                None,
                                Some("收到用戶端的取消通知。"),
                            );
                        }
                    }
                }
            }
            _ => {}
        }
        Some(item)
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        let result = self.inner.close().await;
        let pending = self
            .captures
            .lock()
            .ok()
            .map(|mut captures| std::mem::take(&mut captures.pending));
        for tracked in pending
            .into_iter()
            .flatten()
            .filter_map(|(_, pending)| pending.captured)
        {
            tracked.finish(
                &self.bus,
                Status::Disconnected,
                None,
                Some("STDIO 已關閉，回應未確認。"),
            );
        }
        self.attachment.take();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::transport::async_rw::AsyncRwTransport;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex, split};

    #[tokio::test]
    async fn captures_actual_stdio_frames_and_failures_without_changing_wire() {
        let bus = Arc::new(Bus::create().unwrap());
        assert!(!bus.attached().unwrap());
        let (client, server) = duplex(64 * 1024);
        let (read, write) = split(server);
        let mut transport = ObservedTransport::new(
            AsyncRwTransport::new(read, write),
            bus.clone(),
            Mode::DeveloperControl,
        );
        assert!(bus.attached().unwrap());
        let mut peer = BufReader::new(client);
        let request = |id| {
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\"params\":{{\"name\":\"read_file\",\"arguments\":{{\"path\":\"sample.txt\"}}}}}}\n"
            )
        };
        let response = |id| {
            serde_json::from_value(serde_json::json!({
            "jsonrpc":"2.0","id":id, "result":{"isError":false,"content":[{"type":"text","text":"fixture"}]}
        })).unwrap()
        };
        let mut cursor = 0;

        peer.write_all(request(1).as_bytes()).await.unwrap();
        assert!(matches!(
            transport.receive().await,
            Some(JsonRpcMessage::Request(_))
        ));
        let opening = bus.poll(&mut cursor).1.pop().unwrap().2;
        assert_eq!(opening.status, Status::Running);
        assert!(opening.response.is_none());
        transport.send(response(1)).await.unwrap();
        let mut wire = String::new();
        peer.read_line(&mut wire).await.unwrap();
        let final_row = bus.poll(&mut cursor).1.pop().unwrap().2;
        assert_eq!(
            super::super::presentation::export(&final_row, 0, 0)["record"]["access_mode"],
            "developer_control"
        );
        assert_eq!(final_row.id, opening.id);
        assert_eq!(final_row.status, Status::Success);
        assert_eq!(
            final_row.response.unwrap(),
            serde_json::from_str::<Value>(&wire).unwrap()
        );

        peer.write_all(request(2).as_bytes()).await.unwrap();
        transport.receive().await.unwrap();
        let previous = bus.poll(&mut cursor).1.pop().unwrap().2;
        cursor = bus.change(None, true).unwrap();
        transport.send(response(2)).await.unwrap();
        wire.clear();
        peer.read_line(&mut wire).await.unwrap();
        assert!(bus.poll(&mut cursor).1.is_empty());
        assert_eq!(previous.status, Status::Running);

        for _ in 0..2 {
            peer.write_all(request(3).as_bytes()).await.unwrap();
            transport.receive().await.unwrap();
        }
        let records = bus.poll(&mut cursor).1;
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].2.id, records[1].2.id);
        assert!(
            records[1..]
                .iter()
                .all(|(_, _, row)| row.status == Status::Ambiguous)
        );
        transport.send(response(3)).await.unwrap();
        assert!(bus.poll(&mut cursor).1.is_empty());

        peer.write_all(request(5).as_bytes()).await.unwrap();
        transport.receive().await.unwrap();
        let opening = bus.poll(&mut cursor).1.pop().unwrap().2;
        peer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"ping\"}\n")
            .await
            .unwrap();
        transport.receive().await.unwrap();
        transport.send(response(5)).await.unwrap();
        let ambiguous = bus.poll(&mut cursor).1.pop().unwrap().2;
        assert_eq!(ambiguous.id, opening.id);
        assert_eq!(ambiguous.status, Status::Ambiguous);
        assert!(ambiguous.response.is_none());

        peer.write_all(request(4).as_bytes()).await.unwrap();
        transport.receive().await.unwrap();
        let opening = bus.poll(&mut cursor).1.pop().unwrap().2;
        drop(peer);
        assert!(transport.send(response(4)).await.is_err());
        let failed = bus.poll(&mut cursor).1.pop().unwrap().2;
        assert_eq!(failed.id, opening.id);
        assert_eq!(failed.status, Status::TransportError);
        assert!(failed.response.is_none());
        let _ = transport.close().await;
        assert!(!bus.attached().unwrap());
    }
}
