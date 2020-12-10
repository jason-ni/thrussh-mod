use crate::client::{Channel, ChannelSender, Msg, OpenChannelMsg};
use crate::pty::Pty;
use crate::ChannelMsg;
use anyhow::Error;
use anyhow::{Chain, Context};
use bitflags::_core::pin::Pin;
use cryptovec::CryptoVec;
use futures::future::BoxFuture;
use futures::task::Poll;
use futures::{FutureExt, Stream};
use std::io::Write;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc::{unbounded_channel, Sender, UnboundedReceiver, UnboundedSender};

pub trait ShellChannel {
    fn split(self) -> Result<(ShellReader, ShellWriter), anyhow::Error>;
}

pub async fn upgrade_to_shell(mut channel: Channel) -> Result<Channel, anyhow::Error> {
    let tty_modes = [
        (Pty::VERASE, 127),
        (Pty::IUTF8, 1),
        (Pty::ECHO, 1),
        (Pty::VQUIT, 28),
        (Pty::TTY_OP_ISPEED, 36000),
        (Pty::TTY_OP_OSPEED, 36000),
    ];
    channel
        .request_pty(true, "xterm", 126, 24, 640, 480, &tty_modes)
        .await?;
    debug!("requested pty");
    match channel.wait().await {
        Some(m) => match m {
            ChannelMsg::Success => (),
            other => anyhow::bail!(
                "unexcepted msg while waiting for Success on requesting pty: {:?}",
                other
            ),
        },
        None => anyhow::bail!("received empty msg while waiting for Success on requesting pty"),
    };
    channel.request_shell(true).await.unwrap();
    debug!("requested shell");
    match channel.wait().await {
        Some(m) => match m {
            ChannelMsg::WindowAdjusted { new_size } => (),
            other => anyhow::bail!(
                "unexcepted msg while waiting for WindowAdjusted on requesting shell: {:?}",
                other
            ),
        },
        None => anyhow::bail!("received empty msg while waiting for Success on requesting shell"),
    };
    match channel.wait().await {
        Some(m) => match m {
            ChannelMsg::Success => (),
            other => anyhow::bail!(
                "unexcepted msg while waiting for Success on requesting shell: {:?}",
                other
            ),
        },
        None => anyhow::bail!("received empty msg while waiting for Success on requesting shell"),
    };
    Ok(channel)
}

async fn relay_msg_loop(
    reader_sender: UnboundedSender<ChannelMsg>,
    writer_sender: UnboundedSender<ChannelMsg>,
    mut channel: Channel,
) -> Result<(), anyhow::Error> {
    while let Some(omsg) = channel.receiver.recv().await {
        let msg = match omsg {
            OpenChannelMsg::Msg(m) => m,
            _ => panic!("should never receive Open msg here"),
        };
        match msg {
            ChannelMsg::Data { data } => reader_sender
                .send(ChannelMsg::Data { data })
                .context("relay data to reader failed")?,
            ChannelMsg::Eof => {
                reader_sender
                    .send(ChannelMsg::Eof)
                    .context("relay eof to reader failed")?;
                writer_sender
                    .send(ChannelMsg::Eof)
                    .context("relay eof to writer failed")?;
            }
            ChannelMsg::WindowAdjusted { new_size } => writer_sender
                .send(ChannelMsg::WindowAdjusted { new_size })
                .context("relay window adjusted msg to writer failed")?,
            ChannelMsg::FlushPendingAck { again } => writer_sender
                .send(ChannelMsg::FlushPendingAck { again })
                .context("relay flush pending ack to writer failed")?,
            ChannelMsg::ExitStatus { exit_status } => {
                reader_sender
                    .send(ChannelMsg::ExitStatus { exit_status })
                    .context("relay exit status to reader failed")?;
                writer_sender
                    .send(ChannelMsg::ExitStatus { exit_status })
                    .context("relay exit status to writer failed")?;
            }
            other => panic!(format!("unexpected OpenChannelMsg: {:?}", other)),
        }
    }
    Ok(())
}

impl ShellChannel for Channel {
    fn split(mut self) -> Result<(ShellReader, ShellWriter), Error> {
        let (reader_sender, reader_receiver) = unbounded_channel();
        let (writer_sender, writer_receiver) = unbounded_channel();
        let reader = ShellReader {
            receiver: reader_receiver,
            pending_buf: None,
            last_read_size: 0,
        };
        let writer = ShellWriter {
            channel_sender: self.sender.clone(),
            receiver: writer_receiver,
            max_packet_size: self.max_packet_size,
            window_size: self.window_size,
            pending_send: false,
            pending_msg: None,
            fut: None,
            flush_pending: false,
            shutdown_flushed: false,
        };
        tokio::spawn(relay_msg_loop(reader_sender, writer_sender, self));
        Ok((reader, writer))
    }
}

pub struct ShellReader {
    receiver: UnboundedReceiver<ChannelMsg>,
    pending_buf: Option<CryptoVec>,
    last_read_size: usize,
}

impl AsyncRead for ShellReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut core::task::Context,
        buf: &mut ReadBuf,
    ) -> Poll<Result<(), tokio::io::Error>> {
        let me = unsafe { &mut Pin::get_unchecked_mut(self) };
        if let Some(pending_buf) = me.pending_buf.take() {
            let sendable = (pending_buf.len() - me.last_read_size).min(buf.remaining());
            buf.put_slice(&pending_buf[me.last_read_size..(me.last_read_size + sendable)]);
            me.last_read_size += sendable;
            if pending_buf.len() > me.last_read_size {
                me.pending_buf = Some(pending_buf);
            }
            return Poll::Ready(Ok(()));
        }
        match Stream::poll_next(Pin::new(&mut me.receiver), cx) {
            Poll::Ready(m) => match m {
                Some(ChannelMsg::Data { data }) => {
                    let end = data.len().min(buf.remaining());
                    buf.put_slice(&data[..end]);
                    if end < data.len() {
                        me.pending_buf.replace(data);
                        me.last_read_size = end;
                    }
                    Poll::Ready(Ok(()))
                }
                Some(ChannelMsg::Eof) => Poll::Ready(Ok(())),
                Some(ChannelMsg::ExitStatus { .. }) => Poll::Ready(Ok(())),
                Some(other) => Poll::Pending,
                None => Poll::Ready(Ok(())),
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

use core::future::Future;
use tokio::sync::mpsc::error::SendError;
pub struct ShellWriter {
    channel_sender: ChannelSender,
    receiver: UnboundedReceiver<ChannelMsg>,
    max_packet_size: u32,
    window_size: u32,
    pending_send: bool,
    pending_msg: Option<Msg>,
    fut: Option<Pin<Box<dyn Future<Output = Result<(), SendError<Msg>>>>>>,
    flush_pending: bool,
    shutdown_flushed: bool,
}

impl ShellWriter {
    pub async fn window_change(
        &mut self,
        col_width: u32,
        row_height: u32,
        pix_width: u32,
        pix_height: u32,
    ) -> Result<(), anyhow::Error> {
        self.channel_sender
            .sender
            .send(Msg::WindowChange {
                id: self.channel_sender.id,
                col_width,
                row_height,
                pix_width,
                pix_height,
            })
            .await
            .map_err(|_| crate::Error::SendError)?;
        Ok(())
    }

    pub fn get_sender(&self) -> Sender<Msg> {
        self.channel_sender.sender.clone()
    }
}

unsafe impl Send for ShellWriter {}

async fn send_msg(sender: Sender<Msg>, msg: Msg) -> Result<(), SendError<Msg>> {
    sender.send(msg).await
}

fn do_poll_flush(
    me: &mut &mut ShellWriter,
    cx: &mut core::task::Context<'_>,
) -> Poll<Result<(), tokio::io::Error>> {
    if me.flush_pending {
        let msg = Msg::FlushPending {
            id: me.channel_sender.id,
        };
        let sender = me.channel_sender.sender.clone();
        let fut = send_msg(sender, msg).boxed();
        me.fut = Some(fut);
    };
    match &mut me.fut {
        Some(fut) => match fut.poll_unpin(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Ok(())) => (),
            Poll::Ready(Err(e)) => {
                return Poll::Ready(Err(tokio::io::Error::new(
                    tokio::io::ErrorKind::UnexpectedEof,
                    e,
                )))
            }
        },
        None => (),
    };
    if me.fut.is_some() {
        me.fut = None;
    }
    match Stream::poll_next(Pin::new(&mut me.receiver), cx) {
        Poll::Ready(Some(msg)) => match msg {
            ChannelMsg::WindowAdjusted { new_size } => {
                me.window_size = new_size;
                Poll::Pending
            }
            ChannelMsg::Eof => Poll::Ready(Ok(())), //TODO: what we should return if we find channel eof
            ChannelMsg::FlushPendingAck { again } => {
                me.flush_pending = false;
                if again {
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            ChannelMsg::ExitStatus { .. } => Poll::Ready(Ok(())),
            other => Poll::Pending,
        },
        Poll::Ready(None) => Poll::Ready(Ok(())),
        Poll::Pending => Poll::Pending,
    }
}

impl AsyncWrite for ShellWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, tokio::io::Error>> {
        let me = unsafe { &mut Pin::get_unchecked_mut(self) };
        match &mut me.fut {
            Some(fut) => match fut.poll_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(())) => (),
                Poll::Ready(Err(e)) => {
                    return Poll::Ready(Err(tokio::io::Error::new(
                        tokio::io::ErrorKind::UnexpectedEof,
                        e,
                    )))
                }
            },
            None => (),
        };
        if me.window_size == 0 {
            loop {
                match Stream::poll_next(Pin::new(&mut me.receiver), cx) {
                    Poll::Ready(Some(msg)) => match msg {
                        ChannelMsg::WindowAdjusted { new_size } => {
                            me.window_size = new_size;
                            if new_size > 0 {
                                break;
                            }
                        }
                        ChannelMsg::Eof => return Poll::Ready(Ok(0)), //TODO: what we should return if we find channel eof
                        ChannelMsg::ExitStatus { .. } => return Poll::Ready(Ok(0)),
                        _other => (),
                    },
                    Poll::Ready(None) => return Poll::Ready(Ok(0)),
                    Poll::Pending => return Poll::Pending,
                }
            }
        }
        assert!(me.window_size > 0, true);
        let sendable = buf
            .len()
            .min((me.max_packet_size - 64).min(me.window_size) as usize);
        let mut c = CryptoVec::new_zeroed(0);
        match c.write(&buf[..sendable]) {
            Ok(_) => (),
            Err(e) => return Poll::Ready(Err(e)),
        };
        let msg = Msg::Data {
            id: me.channel_sender.id,
            data: c,
        };
        let sender = me.channel_sender.sender.clone();
        let mut fut = send_msg(sender, msg).boxed();
        match fut.poll_unpin(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(sendable)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(tokio::io::Error::new(
                tokio::io::ErrorKind::UnexpectedEof,
                e,
            ))),
            Poll::Pending => {
                me.fut = Some(fut);
                Poll::Pending
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<Result<(), tokio::io::Error>> {
        let me = unsafe { &mut Pin::get_unchecked_mut(self) };
        do_poll_flush(me, cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<Result<(), tokio::io::Error>> {
        let me = unsafe { &mut Pin::get_unchecked_mut(self) };
        if !me.shutdown_flushed {
            match do_poll_flush(me, cx) {
                Poll::Ready(Ok(())) => (),
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
            me.shutdown_flushed = true
        }

        if me.fut.is_none() {
            let msg = Msg::Eof {
                id: me.channel_sender.id,
            };
            let sender = me.channel_sender.sender.clone();
            let fut = send_msg(sender, msg).boxed();
            me.fut = Some(fut);
        }

        let poll_res = match &mut me.fut {
            Some(fut) => match fut.poll_unpin(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
                Poll::Ready(Err(e)) => Poll::Ready(Err(tokio::io::Error::new(
                    tokio::io::ErrorKind::UnexpectedEof,
                    e,
                ))),
            },
            None => Poll::Ready(Ok(())),
        };
        me.shutdown_flushed = false;
        poll_res
    }
}
