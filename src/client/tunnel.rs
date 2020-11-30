use cryptovec::CryptoVec;
use thrussh_keys::key;
use tokio::net::TcpStream;
use tokio::sync::mpsc::{unbounded_channel, Sender, UnboundedReceiver, UnboundedSender};

use crate::client::proxy::Stream::Tcp;
use crate::client::{Channel, ChannelId, ChannelMsg, Handler, Msg, OpenChannelMsg, Session};
use crate::Error;
use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;

pub struct RemoteTunnel {
    addr: SocketAddr,
    sender: Sender<Msg>,
    receiver: UnboundedReceiver<OpenChannelMsg>,
    channel_id: ChannelId,
    window_size: u32,
    max_packet_size: u32,
}

impl RemoteTunnel {
    pub fn new(addr: SocketAddr, ch: Channel) -> Self {
        RemoteTunnel {
            addr,
            sender: ch.sender.sender,
            receiver: ch.receiver,
            channel_id: ch.sender.id,
            window_size: ch.window_size,
            max_packet_size: ch.max_packet_size,
        }
    }

    pub async fn run(self) -> Result<(), anyhow::Error> {
        let stream = TcpStream::connect(self.addr).await?;
        let (rh, wh) = stream.into_split();
        let sender = self.sender.clone();
        let (sender_tcp, receiver_tcp) = unbounded_channel();
        let (sender_ssh, receiver_ssh) = unbounded_channel();
        tokio::spawn(Self::receive_msg_loop(
            self.receiver,
            sender_tcp,
            sender_ssh,
        ));
        tokio::spawn(Self::send_to_ssh_loop(
            sender.clone(),
            self.channel_id.clone(),
            receiver_ssh,
            self.window_size,
            self.max_packet_size,
            rh,
        ));
        tokio::spawn(Self::send_to_tcp_loop(
            sender,
            self.channel_id.clone(),
            receiver_tcp,
            wh,
        ));
        Ok(())
    }

    async fn receive_msg_loop(
        mut msg_receiver: UnboundedReceiver<OpenChannelMsg>,
        sender_tcp: UnboundedSender<OpenChannelMsg>,
        sender_ssh: UnboundedSender<OpenChannelMsg>,
    ) -> Result<(), anyhow::Error> {
        loop {
            if let Some(msg) = msg_receiver.recv().await {
                match msg {
                    OpenChannelMsg::Msg(ch_msg) => match ch_msg {
                        ChannelMsg::Data { data } => {
                            sender_tcp
                                .send(OpenChannelMsg::Msg(ChannelMsg::Data { data }))
                                .map_err(|_| Error::SendError)?;
                        }
                        ChannelMsg::Eof => {
                            sender_tcp
                                .send(OpenChannelMsg::Msg(ChannelMsg::Eof))
                                .map_err(|_| Error::SendError)?;
                            sender_ssh
                                .send(OpenChannelMsg::Msg(ChannelMsg::Eof))
                                .map_err(|_| Error::SendError)?;
                        }
                        ChannelMsg::WindowAdjusted { new_size } => {
                            sender_ssh
                                .send(OpenChannelMsg::Msg(ChannelMsg::WindowAdjusted { new_size }))
                                .map_err(|_| Error::SendError)?;
                        }
                        ChannelMsg::FlushPendingAck { again } => {
                            sender_ssh
                                .send(OpenChannelMsg::Msg(ChannelMsg::FlushPendingAck { again }))
                                .map_err(|_| Error::SendError)?;
                        }
                        x => panic!("unexpected ChannelMsg received: {:?}", x),
                    },
                    x => panic!("unexpected OpenChannelMsg received: {:?}", x),
                }
            } else {
                return Ok(());
            }
        }
    }

    async fn send_to_tcp_loop(
        sender: Sender<Msg>,
        channel_id: ChannelId,
        mut msg_receiver: UnboundedReceiver<OpenChannelMsg>,
        mut writer: OwnedWriteHalf,
    ) -> Result<(), anyhow::Error> {
        debug!("send to tcp loop");
        loop {
            if let Some(msg) = msg_receiver.recv().await {
                match msg {
                    OpenChannelMsg::Msg(ch_msg) => match ch_msg {
                        ChannelMsg::Data { data } => {
                            writer.write_all(data.as_ref()).await?;
                        }
                        ChannelMsg::Eof => {
                            debug!("ssh channel write part eof");
                            break;
                        }
                        x => panic!("unexpected ChannelMsg: {:?}", x),
                    },
                    x => panic!("unexpected OpenChannelMsg: {:?}", x),
                }
            } else {
                break;
            }
        }
        debug!("exiting send_to_tcp_loop");
        Ok(())
    }

    async fn send_to_ssh_loop<R: tokio::io::AsyncReadExt + std::marker::Unpin>(
        sender: Sender<Msg>,
        channel_id: ChannelId,
        mut receiver: UnboundedReceiver<OpenChannelMsg>,
        mut window_size: u32,
        max_packet_size: u32,
        mut data: R,
    ) -> Result<(), anyhow::Error> {
        let orig_window = window_size;
        debug!("origin window size: {}", orig_window);
        loop {
            loop {
                debug!(
                    "sending data, window_size = {:?}, max_packet_size = {:?}",
                    window_size, max_packet_size
                );
                let sendable = window_size.min(max_packet_size - 64) as usize;
                //let page_len = 4096.min(sendable);
                //let mut c = CryptoVec::new_zeroed(page_len);
                let mut c = CryptoVec::new_zeroed(sendable);
                let n = data.read(&mut c[..]).await?;
                debug!("=== reading {} bytes from upstream", n);
                c.resize(n);
                window_size -= n as u32;
                if n == 0 {
                    debug!("=== read tcp socket end. window: {}", window_size);
                    sender
                        .send(Msg::FlushPending { id: channel_id })
                        .await
                        .map_err(|_| Error::SendError)?;
                    loop {
                        debug!("waiting on window size adjusting");
                        match receiver.recv().await {
                            Some(OpenChannelMsg::Msg(ChannelMsg::WindowAdjusted { new_size })) => {
                                debug!("ignoring window_size adjust: {}", window_size);
                                window_size = new_size;
                            }
                            Some(OpenChannelMsg::Msg(ChannelMsg::FlushPendingAck { again })) => {
                                if again || (window_size == 0) {
                                    sender
                                        .send(Msg::FlushPending { id: channel_id })
                                        .await
                                        .map_err(|_| Error::SendError)?;
                                } else {
                                    break;
                                }
                            }
                            Some(OpenChannelMsg::Msg(ChannelMsg::Eof)) => break,
                            Some(OpenChannelMsg::Msg(msg)) => {
                                panic!("unexpected channel msg: {:?}", msg);
                            }
                            Some(_) => panic!("unexpected channel msg"),
                            None => break,
                        }
                    }

                    sender
                        .send(Msg::Eof { id: channel_id })
                        .await
                        .map_err(|_| Error::SendError)?;
                    return Ok(());
                }
                sender
                    .send(Msg::Data {
                        id: channel_id,
                        data: c,
                    })
                    .await
                    .map_err(|_| Error::SendError)?;
                if window_size > 0 {
                    break;
                }
                // wait for the window to be restored.
                sender
                    .send(Msg::FlushPending { id: channel_id })
                    .await
                    .map_err(|_| Error::SendError)?;

                loop {
                    debug!("waiting on window size adjusting");
                    match receiver.recv().await {
                        Some(OpenChannelMsg::Msg(ChannelMsg::WindowAdjusted { new_size })) => {
                            debug!("ignoring window_size adjust: {}", window_size);
                            window_size = new_size;
                        }
                        Some(OpenChannelMsg::Msg(ChannelMsg::FlushPendingAck { again })) => {
                            if again || (window_size == 0) {
                                sender
                                    .send(Msg::FlushPending { id: channel_id })
                                    .await
                                    .map_err(|_| Error::SendError)?;
                            } else {
                                break;
                            }
                        }
                        Some(OpenChannelMsg::Msg(ChannelMsg::Eof)) => return Ok(()),
                        Some(OpenChannelMsg::Msg(msg)) => {
                            panic!("unexpected channel msg: {:?}", msg);
                        }
                        Some(_) => panic!("unexpected channel msg"),
                        None => break,
                    }
                }
            }
        }
    }
}

pub struct TunnelClient {
    sender: Option<Sender<Msg>>,
}

impl TunnelClient {
    pub fn new() -> Self {
        TunnelClient { sender: None }
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

    fn on_session_connected(mut self, sender: Sender<Msg>, session: Session) -> Self::FutureUnit {
        self.sender.replace(sender);
        self.finished(session)
    }
    fn channel_open_forwarded_tcpip_client(
        self,
        channel: ChannelId,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        window_size: u32,
        max_packet_size: u32,
        mut session: Session,
    ) -> Self::FutureUnit {
        debug!("client handling open forwarded tcpip request");
        match &self.sender {
            Some(ref sender) => {
                let ch = session
                    .create_forwarded_tcpip_channel(
                        channel.0,
                        sender.clone(),
                        window_size,
                        max_packet_size,
                    )
                    .unwrap();
                let addr: SocketAddr = "127.0.0.1:8081".parse().unwrap();
                let tunnel = RemoteTunnel::new(addr, ch);
                tokio::spawn(tunnel.run());
            }
            None => panic!("client should connect first"),
        }
        self.finished(session)
    }
}
