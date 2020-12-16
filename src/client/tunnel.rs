use crate::client::channel::{ChannelReader, ChannelWriter};
use crate::client::{Channel, ChannelSender, Handler, OpenChannelMsg, Session};
use bytes::BytesMut;
use core::pin::Pin;
use futures::task::{Context, Poll};
use futures::Stream;
use std::fmt::Debug;
use std::future::Future;
use thrussh_keys::key;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn copy<R: AsyncReadExt + Unpin + Debug, W: AsyncWriteExt + Unpin>(
    mut r: R,
    mut w: W,
) -> Result<(), anyhow::Error> {
    let mut buf = BytesMut::with_capacity(2048);
    loop {
        buf.clear();
        trace!("==== reading from {:?}", &r);
        let n = r.read_buf(&mut buf).await?;
        trace!("==== read from {:?} {} bytes", &r, n);
        if n == 0 {
            debug!("==== read buf 0 end");
            return Ok(());
        }
        w.write_all(&buf[..n]).await?;
        trace!("=== written data to writer, reader {:?}", r);
    }
}

pub async fn handle_connect<F, Fut, C>(
    channel: Channel,
    conf: C,
    conn_init: F,
) -> Result<(), anyhow::Error>
where
    F: FnOnce(Channel, C) -> Fut,
    Fut: Future<Output = Result<(TcpStream, ChannelReader, ChannelWriter), anyhow::Error>>,
{
    debug!("=== handling channel");
    let (stream, ch_rh, ch_wh) = conn_init(channel, conf).await?;
    debug!("stream connected: {:?}", &stream);
    let (stream_rh, stream_wh) = stream.into_split();
    let cp1 = copy(ch_rh, stream_wh);
    let cp2 = copy(stream_rh, ch_wh);
    tokio::spawn(cp1);
    tokio::spawn(cp2);
    Ok(())
}

pub struct TunnelClient {}

impl TunnelClient {
    pub fn new() -> Self {
        TunnelClient {}
    }
}

impl Handler for TunnelClient {
    type FutureUnit = futures::future::Ready<Result<(Self, Session), anyhow::Error>>;
    type FutureBool = futures::future::Ready<Result<(Self, bool), anyhow::Error>>;

    fn finished_bool(self, b: bool) -> Self::FutureBool {
        futures::future::ready(Ok((self, b)))
    }
    fn finished(self, session: Session) -> Self::FutureUnit {
        futures::future::ready(Ok((self, session)))
    }
    fn check_server_key(self, server_public_key: &key::PublicKey) -> Self::FutureBool {
        println!("check_server_key: {:?}", server_public_key);
        self.finished_bool(true)
    }
}

pub struct RemoteForwardListener {
    channel: Channel,
}

impl Stream for RemoteForwardListener {
    type Item = Channel;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let me = unsafe { Pin::get_unchecked_mut(self) };
        match Stream::poll_next(Pin::new(&mut me.channel.receiver), cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(msg)) => match msg {
                OpenChannelMsg::Open {
                    id,
                    max_packet_size,
                    window_size,
                    receiver,
                } => Poll::Ready(Some(Channel {
                    sender: ChannelSender {
                        sender: me.channel.sender.sender.clone(),
                        id,
                    },
                    receiver: receiver.expect("receiver must exist"),
                    window_size,
                    max_packet_size,
                })),
                _ => Poll::Pending,
            },
            Poll::Ready(None) => Poll::Ready(None),
        }
    }
}

pub async fn upgrade_to_remote_forward_tcpip_listener<A: Into<String>>(
    mut channel: Channel,
    address: A,
    port: u32,
) -> Result<RemoteForwardListener, anyhow::Error> {
    channel.tcpip_forward(false, address, port).await?;
    Ok(RemoteForwardListener { channel })
}
