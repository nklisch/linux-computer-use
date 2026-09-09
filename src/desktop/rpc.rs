//! Correlated local RPC. Readers remain live while actions or response writes wait.
use super::{Request, Response, WIRE_REVISION};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    future::Future,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{Mutex, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Packet {
    Call {
        id: u64,
        request: Request,
        /// Internal daemon forwarding metadata; first-hop readers ignore it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ingress: Option<u64>,
    },
    Cancel {
        id: u64,
    },
    Reply {
        id: u64,
        result: Box<std::result::Result<Response, String>>,
        image: Option<String>,
    },
}
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Response>>>>>;
struct Inner {
    send: mpsc::UnboundedSender<Packet>,
    pending: Pending,
    next: AtomicU64,
    dead: CancellationToken,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.dead.cancel();
    }
}
#[derive(Clone)]
pub struct Client(Arc<Inner>);
struct CancelCall {
    id: u64,
    send: mpsc::UnboundedSender<Packet>,
}
impl Drop for CancelCall {
    fn drop(&mut self) {
        let _ = self.send.send(Packet::Cancel { id: self.id });
    }
}
impl Client {
    pub async fn connect(path: &Path) -> Result<Self> {
        let socket = UnixStream::connect(path)
            .await
            .with_context(|| format!("Connect {}; start lcu daemon first", path.display()))?;
        let (read, mut write) = socket.into_split();
        let (send, mut receive) = mpsc::unbounded_channel();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let dead = CancellationToken::new();
        let d = dead.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _=d.cancelled()=>break,
                    packet=receive.recv()=>match packet {
                        Some(packet)=> { let Ok(mut bytes) = serde_json::to_vec(&packet) else { break }; bytes.push(b'\n');
                            if tokio::select! { _=d.cancelled()=>true, r=write.write_all(&bytes)=>r.is_err() } { break; }
                        }, None=>break,
                    }
                }
            }
            d.cancel();
        });
        let d = dead.clone();
        let p = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            loop {
                let line = tokio::select! { _=d.cancelled()=>break, l=lines.next_line()=>l };
                let Ok(Some(line)) = line else { break };
                let Ok(Packet::Reply { id, result, image }) = serde_json::from_str(&line) else {
                    break;
                };
                let result = (*result)
                    .map_err(anyhow::Error::msg)
                    .and_then(|mut response| {
                        if let (Response::Control(reply), Some(image)) = (&mut response, image) {
                            let png = base64::engine::general_purpose::STANDARD.decode(image)?;
                            match reply {
                                crate::types::Reply::Observation(o) => o.png = png,
                                crate::types::Reply::Action(a) => {
                                    if let Some(o) = &mut a.observation {
                                        o.png = png
                                    }
                                }
                                _ => {}
                            }
                        }
                        Ok(response)
                    });
                if let Some(tx) = p.lock().await.remove(&id) {
                    let _ = tx.send(result);
                }
            }
            d.cancel();
            for (_, tx) in p.lock().await.drain() {
                let _ = tx.send(Err(anyhow::anyhow!(
                    "Connection lost after submission; outcome unknown, do not replay input"
                )));
            }
        });
        Ok(Self(Arc::new(Inner {
            send,
            pending,
            next: AtomicU64::new(1),
            dead,
        })))
    }
    pub(crate) fn is_disconnected(&self) -> bool {
        self.0.dead.is_cancelled()
    }
    pub fn disconnect(&self) {
        self.0.dead.cancel();
    }
    pub async fn handshake(&self) -> Result<()> {
        let Response::Hello { wire_revision, .. } =
            tokio::time::timeout(std::time::Duration::from_secs(3), self.call(Request::Hello))
                .await
                .context("Worker handshake timed out; other desktops remain usable")??
        else {
            anyhow::bail!("Invalid worker handshake")
        };
        ensure!(
            wire_revision == WIRE_REVISION,
            "Unsupported worker wire revision {wire_revision}; use a matching client or explicitly recreate the desktop. Emergency stop remains available."
        );
        Ok(())
    }
    pub async fn call(&self, request: Request) -> Result<Response> {
        self.submit(request, None).await
    }
    pub(crate) async fn forward(&self, request: Request, ingress: u64) -> Result<Response> {
        self.submit(request, Some(ingress)).await
    }
    async fn submit(&self, request: Request, ingress: Option<u64>) -> Result<Response> {
        ensure!(
            !self.0.dead.is_cancelled(),
            "Connection closed; reconnect and claim explicitly"
        );
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.0.pending.lock().await.insert(id, tx);
        let _guard = CancelCall {
            id,
            send: self.0.send.clone(),
        };
        self.0
            .send
            .send(Packet::Call {
                id,
                request,
                ingress,
            })
            .context("RPC writer closed")?;
        tokio::select! { _=self.0.dead.cancelled()=>anyhow::bail!("Connection lost; operation outcome unknown, do not replay"), r=rx=>r.context("RPC reply lost; outcome unknown")? }
    }
}

pub async fn serve<F, Fut>(stream: UnixStream, life: CancellationToken, handler: F) -> Result<()>
where
    F: Fn(Request, u64, CancellationToken) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response>> + Send + 'static,
{
    serve_inner(stream, life, false, handler).await
}
pub(crate) async fn serve_worker<F, Fut>(
    stream: UnixStream,
    life: CancellationToken,
    handler: F,
) -> Result<()>
where
    F: Fn(Request, u64, CancellationToken) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response>> + Send + 'static,
{
    serve_inner(stream, life, true, handler).await
}
async fn serve_inner<F, Fut>(
    stream: UnixStream,
    life: CancellationToken,
    forwarded: bool,
    handler: F,
) -> Result<()>
where
    F: Fn(Request, u64, CancellationToken) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response>> + Send + 'static,
{
    let handler = Arc::new(handler);
    let (read, mut write) = stream.into_split();
    let (send, mut responses) = mpsc::unbounded_channel::<Packet>();
    let writer_life = life.clone();
    let writer = tokio::spawn(async move {
        while let Some(packet) =
            tokio::select! { _=writer_life.cancelled()=>None, p=responses.recv()=>p }
        {
            let Ok(mut bytes) = serde_json::to_vec(&packet) else {
                break;
            };
            bytes.push(b'\n');
            if tokio::select! { _=writer_life.cancelled()=>true, r=write.write_all(&bytes)=>r.is_err() }
            {
                break;
            }
        }
        writer_life.cancel();
    });
    let mut lines = BufReader::new(read).lines();
    let mut tasks = tokio::task::JoinSet::new();
    let mut calls = HashMap::<u64, CancellationToken>::new();
    let mut received = 0u64;
    loop {
        tokio::select! {
            biased;
            _=life.cancelled()=>break,
            done=tasks.join_next(), if !tasks.is_empty()=>{ if let Some(Ok(id))=done { calls.remove(&id); } },
            line=lines.next_line()=>{
                let Ok(Some(line))=line else { break };
                let Ok(packet)=serde_json::from_str::<Packet>(&line) else { break };
                match packet {
                    Packet::Call { id, request, ingress }=>{
                        if calls.contains_key(&id) { break; }
                        // Stamp decoded socket order, never correlation allocation or task scheduling.
                        received = received.checked_add(1).context("RPC ingress order exhausted")?;
                        let order = if forwarded { ingress.unwrap_or(received) } else { received };
                        let token=life.child_token(); calls.insert(id,token.clone());
                        let h=handler.clone(); let s=send.clone();
                        tasks.spawn(async move {
                            let result=h(request,order,token).await;
                            let image=match &result { Ok(Response::Control(r))=>r.observation().map(|o| base64::engine::general_purpose::STANDARD.encode(&o.png)), _=>None };
                            let _=s.send(Packet::Reply { id, result:Box::new(result.map_err(|e| format!("{e:#}"))), image }); id
                        });
                    },
                    Packet::Cancel { id }=>if let Some(token)=calls.get(&id) { token.cancel(); },
                    Packet::Reply { .. }=>break,
                }
            }
        }
    }
    life.cancel();
    while tasks.join_next().await.is_some() {}
    drop(send);
    let _ = writer.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    #[tokio::test]
    async fn first_reader_uses_wire_arrival_not_correlation_or_supplied_order() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let task = tokio::spawn(async move {
            serve(server, CancellationToken::new(), |_, order, _| async move {
                Ok(Response::Hello {
                    wire_revision: WIRE_REVISION,
                    version: order.to_string(),
                })
            })
            .await
            .unwrap();
        });
        for (id, ingress) in [(900, Some(80)), (2, Some(1))] {
            let mut bytes = serde_json::to_vec(&Packet::Call {
                id,
                request: Request::Hello,
                ingress,
            })
            .unwrap();
            bytes.push(b'\n');
            client.write_all(&bytes).await.unwrap();
        }
        let mut lines = BufReader::new(client).lines();
        let mut received = HashMap::new();
        for _ in 0..2 {
            let Packet::Reply { id, result, .. } =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap()
            else {
                panic!()
            };
            let Response::Hello { version, .. } = (*result).unwrap() else {
                panic!()
            };
            received.insert(id, version);
        }
        assert_eq!(received[&900], "1");
        assert_eq!(received[&2], "2");
        drop(lines);
        task.await.unwrap();
    }
    #[tokio::test]
    async fn image_bytes_survive_rpc_and_disconnect_cancels_inflight_work() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rpc.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let entered = Arc::new(std::sync::Mutex::new(Some(entered_tx)));
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            serve(
                socket,
                CancellationToken::new(),
                move |request, _order, cancel| {
                    let entered = entered.clone();
                    async move {
                        if matches!(request, Request::Hello) {
                            return Ok(Response::Control(Reply::Observation(Observation {
                                png: vec![0, 128, 255, 42],
                                info: ObservationInfo {
                                    frame_id: "owner-frame".into(),
                                    display: 0,
                                    captured_at_ms: 0,
                                    age_ms: 0,
                                    capture_sequence: 1,
                                    image_width: 1,
                                    image_height: 1,
                                    source_width: 1,
                                    source_height: 1,
                                    crop: Crop {
                                        x: 0,
                                        y: 0,
                                        width: 1,
                                        height: 1,
                                    },
                                    logical_size: None,
                                    new_frame_after_input: None,
                                    freshness_met: None,
                                    requested_after_sequence: None,
                                    wait_timed_out: false,
                                },
                            })));
                        }
                        if let Some(tx) = entered.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        cancel.cancelled().await;
                        anyhow::bail!("Cancelled by connection death")
                    }
                },
            )
            .await
            .unwrap();
        });
        let client = Client::connect(&path).await.unwrap();
        let Response::Control(Reply::Observation(o)) = client.call(Request::Hello).await.unwrap()
        else {
            panic!()
        };
        assert_eq!(o.png, vec![0, 128, 255, 42]);
        let other = client.clone();
        let call = tokio::spawn(async move {
            other
                .call(Request::Desktop(super::super::DesktopOperation::List))
                .await
        });
        entered_rx.await.unwrap();
        client.disconnect();
        assert!(call.await.unwrap().is_err());
        tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    }
}
