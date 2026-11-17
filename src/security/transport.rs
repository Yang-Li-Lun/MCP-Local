use std::time::Duration;

use rmcp::{
    RoleServer,
    model::{
        CallToolResult, ContentBlock, CustomNotification, CustomResult, ErrorData, JsonRpcMessage,
        ServerNotification, ServerResult,
    },
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};
use serde_json::Value;

use super::{Output, Runtime, SESSION_REVISION, SharedRuntime, guard::BLOCK_TEXT};

pub struct GuardedTransport<T> {
    inner: T,
    runtime: SharedRuntime,
    pulse: Option<tokio::task::JoinHandle<()>>,
}

impl<T> GuardedTransport<T> {
    #[cfg(test)]
    pub fn new(inner: T, runtime: Runtime) -> Self {
        Self::shared(inner, std::sync::Arc::new(std::sync::Mutex::new(runtime)))
    }

    pub fn shared(inner: T, runtime: SharedRuntime) -> Self {
        let managed = runtime.lock().unwrap().managed();
        let pulse = managed.then(|| {
            let runtime = runtime.clone();
            tokio::spawn(async move {
                let mut timer = tokio::time::interval(Duration::from_millis(100));
                timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    timer.tick().await;
                    let Ok(mut runtime) = runtime.lock() else {
                        break;
                    };
                    runtime.heartbeat();
                }
            })
        });
        Self {
            inner,
            runtime,
            pulse,
        }
    }
}

impl<T> Drop for GuardedTransport<T> {
    fn drop(&mut self) {
        if let Some(task) = self.pulse.take() {
            task.abort();
            self.runtime
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .close_controls();
        }
    }
}

fn protect(item: &mut TxJsonRpcMessage<RoleServer>, runtime: &mut Runtime) -> bool {
    match item {
        JsonRpcMessage::Response(message) => {
            let revision = if let ServerResult::CallToolResult(result) = &mut message.result
                && let Some(meta) = &mut result.meta
                && let Some(revision) = meta.0.remove(SESSION_REVISION)
            {
                if meta.0.is_empty() {
                    result.meta = None;
                }
                Some(revision)
            } else {
                None
            };
            message.result = match runtime.protect_response(
                revision.as_ref().map(|value| value.as_str().unwrap_or("")),
                || serde_json::to_value(&message.result),
            ) {
                Output::Unchanged => return true,
                Output::Payload(payload) => ServerResult::CustomResult(CustomResult::new(payload)),
                Output::Block => {
                    ServerResult::CallToolResult(CallToolResult::error(vec![ContentBlock::text(
                        BLOCK_TEXT,
                    )]))
                }
            };
        }
        JsonRpcMessage::Error(message) => {
            message.error = match runtime.protect(|| serde_json::to_value(&message.error)) {
                Output::Unchanged => return true,
                Output::Payload(payload) => match serde_json::from_value::<ErrorData>(payload) {
                    Ok(mut safe) => {
                        safe.code = message.error.code;
                        safe
                    }
                    Err(_) => ErrorData::new(message.error.code, BLOCK_TEXT, None),
                },
                Output::Block => ErrorData::new(message.error.code, BLOCK_TEXT, None),
            };
        }
        JsonRpcMessage::Notification(message) => {
            let mut wire = None;
            let output = runtime.protect(|| {
                let value = serde_json::to_value(&message.notification)?;
                let params = value
                    .get("params")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default()));
                wire = Some(value);
                Ok(params)
            });
            match output {
                Output::Unchanged => return true,
                Output::Block => return false,
                Output::Payload(params) => {
                    let Some(method) = wire
                        .as_ref()
                        .and_then(|value| value.get("method"))
                        .and_then(Value::as_str)
                    else {
                        return false;
                    };
                    message.notification = ServerNotification::CustomNotification(
                        CustomNotification::new(method, Some(params)),
                    );
                }
            }
        }
        _ => {}
    }
    true
}

impl<T: Transport<RoleServer>> Transport<RoleServer> for GuardedTransport<T> {
    type Error = T::Error;

    fn send(
        &mut self,
        mut item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let send = {
            let mut runtime = self.runtime.lock().unwrap_or_else(|error| {
                let mut runtime = error.into_inner();
                runtime.fail();
                runtime
            });
            protect(&mut item, &mut runtime)
        }
        .then(|| self.inner.send(item));
        async move {
            match send {
                Some(send) => send.await,
                None => Ok(()),
            }
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        self.inner.receive().await
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.inner.close().await
    }
}
