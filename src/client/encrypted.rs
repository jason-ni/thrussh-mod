// Copyright 2016 Pierre-Étienne Meunier
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
use super::{Msg, Reply};
use crate::auth;
use crate::client::OpenChannelMsg;
use crate::key::PubKey;
use crate::msg;
use crate::negotiation;
use crate::negotiation::Named;
use crate::negotiation::Select;
use crate::session::*;
use crate::{ChannelId, ChannelOpenFailure, Error, Sig};
use cryptovec::CryptoVec;
use std::cell::RefCell;
use std::net::{IpAddr, SocketAddr};
use thrussh_keys::encoding::{Encoding, Reader};
use tokio::sync::mpsc::{unbounded_channel, Sender, UnboundedSender};

thread_local! {
    static SIGNATURE_BUFFER: RefCell<CryptoVec> = RefCell::new(CryptoVec::new());
}

impl super::Session {
    pub(crate) async fn client_read_encrypted<C: super::Handler>(
        mut self,
        client: &mut Option<C>,
        buf: &[u8],
    ) -> Result<Self, anyhow::Error> {
        trace!(
            "client_read_encrypted, buf = {}",
            pretty_hex::pretty_hex(&buf[..buf.len().min(100)].as_ref()),
        );
        // Either this packet is a KEXINIT, in which case we start a key re-exchange.
        if buf[0] == msg::KEXINIT {
            // Now, if we're encrypted:
            if let Some(ref mut enc) = self.common.encrypted {
                // If we're not currently rekeying, but buf is a rekey request
                if let Some(Kex::KexInit(kexinit)) = enc.rekey.take() {
                    enc.rekey = Some(Kex::KexDhDone(kexinit.client_parse(
                        self.common.config.as_ref(),
                        &self.common.cipher,
                        buf,
                        &mut self.common.write_buffer,
                    )?));
                    self.flush()?;
                } else if let Some(exchange) = std::mem::replace(&mut enc.exchange, None) {
                    let kexinit = KexInit::received_rekey(
                        exchange,
                        negotiation::Client::read_kex(buf, &self.common.config.as_ref().preferred)?,
                        &enc.session_id,
                    );
                    debug!("shall we send a KexInit?");
                    enc.rekey = Some(Kex::KexDhDone(kexinit.client_parse(
                        self.common.config.as_ref(),
                        &mut self.common.cipher,
                        buf,
                        &mut self.common.write_buffer,
                    )?));
                }
            } else {
                unreachable!()
            }
            return Ok(self);
        }

        if let Some(ref mut enc) = self.common.encrypted {
            match enc.rekey.take() {
                Some(Kex::KexDhDone(mut kexdhdone)) => {
                    if kexdhdone.names.ignore_guessed {
                        kexdhdone.names.ignore_guessed = false;
                        enc.rekey = Some(Kex::KexDhDone(kexdhdone));
                        return Ok(self);
                    } else if buf[0] == msg::KEX_ECDH_REPLY {
                        // We've sent ECDH_INIT, waiting for ECDH_REPLY
                        enc.rekey = Some(kexdhdone.server_key_check(true, client, buf).await?);
                        self.common
                            .cipher
                            .write(&[msg::NEWKEYS], &mut self.common.write_buffer);
                        self.flush()?;
                        return Ok(self);
                    } else {
                        error!("Wrong packet received");
                        return Err(Error::Inconsistent.into());
                    }
                }
                Some(Kex::NewKeys(newkeys)) => {
                    if buf[0] != msg::NEWKEYS {
                        return Err(Error::Kex.into());
                    }
                    self.common.write_buffer.bytes = 0;
                    enc.last_rekey = std::time::Instant::now();

                    // Ok, NEWKEYS received, now encrypted.
                    self.common.newkeys(newkeys);
                    return Ok(self);
                }
                rek => enc.rekey = rek,
            }
        }

        // If we've successfully read a packet.
        debug!("buf = {:?} bytes", buf.len());
        trace!("buf = {:?}", buf);
        let mut is_authenticated = false;
        if let Some(ref mut enc) = self.common.encrypted {
            match enc.state {
                EncryptedState::WaitingServiceRequest {
                    ref mut accepted, ..
                } => {
                    debug!(
                        "waiting service request, {:?} {:?}",
                        buf[0],
                        msg::SERVICE_ACCEPT
                    );
                    if buf[0] == msg::SERVICE_ACCEPT {
                        let mut r = buf.reader(1);
                        if r.read_string()? == b"ssh-userauth" {
                            *accepted = true;
                            if let Some(ref meth) = self.common.auth_method {
                                let auth_request = auth::AuthRequest {
                                    methods: auth::MethodSet::all(),
                                    partial_success: false,
                                    current: None,
                                    rejection_count: 0,
                                };
                                let len = enc.write.len();
                                if enc.write_auth_request(&self.common.auth_user, meth) {
                                    debug!("enc: {:?}", &enc.write[len..]);
                                    enc.state = EncryptedState::WaitingAuthRequest(auth_request)
                                }
                            } else {
                                debug!("no auth method")
                            }
                        }
                    } else {
                        debug!("unknown message: {:?}", buf);
                        return Err(Error::Inconsistent.into());
                    }
                }
                EncryptedState::WaitingAuthRequest(ref mut auth_request) => {
                    if buf[0] == msg::USERAUTH_SUCCESS {
                        debug!("userauth_success");
                        self.sender
                            .send(Reply::AuthSuccess)
                            .map_err(|_| Error::SendError)?;
                        enc.state = EncryptedState::InitCompression;
                        enc.server_compression.init_decompress(&mut enc.decompress);
                        return Ok(self);
                    } else if buf[0] == msg::USERAUTH_BANNER {
                        let mut r = buf.reader(1);
                        let banner = r.read_string()?;
                        if let Ok(banner) = std::str::from_utf8(banner) {
                            let c = client.take().unwrap();
                            let (c, s) = c.auth_banner(banner, self).await?;
                            *client = Some(c);
                            return Ok(s);
                        } else {
                            return Ok(self);
                        }
                    } else if buf[0] == msg::USERAUTH_FAILURE {
                        debug!("userauth_failure");

                        let mut r = buf.reader(1);
                        let remaining_methods = r.read_string()?;
                        debug!(
                            "remaining methods {:?}",
                            std::str::from_utf8(remaining_methods)
                        );
                        auth_request.methods = auth::MethodSet::empty();
                        for method in remaining_methods.split(|&c| c == b',') {
                            if let Some(m) = auth::MethodSet::from_bytes(method) {
                                auth_request.methods |= m
                            }
                        }
                        let no_more_methods = auth_request.methods.is_empty();
                        self.common.auth_method = None;
                        self.sender
                            .send(Reply::AuthFailure)
                            .map_err(|_| Error::SendError)?;

                        // If no other authentication method is allowed by the server, give up.
                        if no_more_methods {
                            return Err(Error::NoAuthMethod.into());
                        }
                    } else if buf[0] == msg::USERAUTH_PK_OK {
                        debug!("userauth_pk_ok");
                        if let Some(auth::CurrentRequest::PublicKey {
                            ref mut sent_pk_ok, ..
                        }) = auth_request.current
                        {
                            *sent_pk_ok = true;
                        }

                        match self.common.auth_method.take() {
                            Some(auth_method @ auth::Method::PublicKey { .. }) => {
                                self.common.buffer.clear();
                                enc.client_send_signature(
                                    &self.common.auth_user,
                                    &auth_method,
                                    &mut self.common.buffer,
                                )?
                            }
                            Some(auth::Method::FuturePublicKey { key }) => {
                                debug!("public key");
                                self.common.buffer.clear();
                                let i = enc.client_make_to_sign(
                                    &self.common.auth_user,
                                    &key,
                                    &mut self.common.buffer,
                                );
                                let len = self.common.buffer.len();
                                let buf =
                                    std::mem::replace(&mut self.common.buffer, CryptoVec::new());

                                self.sender
                                    .send(Reply::SignRequest { key, data: buf })
                                    .map_err(|_| Error::SendError)?;
                                self.common.buffer = loop {
                                    match self.receiver.recv().await {
                                        Some(Msg::Signed { data }) => break data,
                                        _ => {}
                                    }
                                };
                                if self.common.buffer.len() != len {
                                    // The buffer was modified.
                                    push_packet!(enc.write, {
                                        enc.write.extend(&self.common.buffer[i..]);
                                    })
                                }
                            }
                            _ => {}
                        }
                    } else {
                        debug!("unknown message: {:?}", buf);
                        return Err(Error::Inconsistent.into());
                    }
                }
                EncryptedState::InitCompression => unreachable!(),
                EncryptedState::Authenticated => is_authenticated = true,
            }
        }
        if is_authenticated {
            self.client_read_authenticated(client, buf).await
        } else {
            Ok(self)
        }
    }

    async fn client_read_authenticated<C: super::Handler>(
        mut self,
        client: &mut Option<C>,
        buf: &[u8],
    ) -> Result<Self, anyhow::Error> {
        match buf[0] {
            msg::CHANNEL_OPEN_CONFIRMATION => {
                debug!("channel_open_confirmation");
                let mut reader = buf.reader(1);
                let id_send = ChannelId(reader.read_u32()?);
                let id_recv = reader.read_u32()?;
                let window = reader.read_u32()?;
                let max_packet = reader.read_u32()?;

                if let Some(ref mut enc) = self.common.encrypted {
                    if let Some(parameters) = enc.channels.get_mut(&id_send) {
                        parameters.recipient_channel = id_recv;
                        parameters.recipient_window_size = window;
                        parameters.recipient_maximum_packet_size = max_packet;
                        parameters.confirmed = true;
                    } else {
                        // We've not requested this channel, close connection.
                        return Err(Error::Inconsistent.into());
                    }
                } else {
                    return Err(Error::Inconsistent.into());
                };
                let c = client.take().unwrap();
                let (c, s) = c
                    .channel_open_confirmation(id_send, max_packet, window, self)
                    .await?;
                *client = Some(c);
                Ok(s)
            }
            msg::CHANNEL_CLOSE => {
                debug!("channel_close");
                let mut r = buf.reader(1);
                let channel_num = ChannelId(r.read_u32()?);
                if let Some(ref mut enc) = self.common.encrypted {
                    enc.channels.remove(&channel_num);
                }
                let c = client.take().unwrap();
                let (c, s) = c.channel_close(channel_num, self).await?;
                *client = Some(c);
                Ok(s)
            }
            msg::CHANNEL_EOF => {
                debug!("channel_eof");
                let mut r = buf.reader(1);
                let channel_num = ChannelId(r.read_u32()?);
                let c = client.take().unwrap();
                let (c, s) = c.channel_eof(channel_num, self).await?;
                *client = Some(c);
                Ok(s)
            }
            msg::CHANNEL_OPEN_FAILURE => {
                debug!("channel_open_failure");
                let mut r = buf.reader(1);
                let channel_num = ChannelId(r.read_u32()?);
                let reason_code = ChannelOpenFailure::from_u32(r.read_u32()?).unwrap();
                let descr = std::str::from_utf8(r.read_string()?)?;
                let language = std::str::from_utf8(r.read_string()?)?;
                if let Some(ref mut enc) = self.common.encrypted {
                    enc.channels.remove(&channel_num);
                }
                let c = client.take().unwrap();
                let (c, s) = c
                    .channel_open_failure(channel_num, reason_code, descr, language, self)
                    .await?;
                *client = Some(c);
                Ok(s)
            }
            msg::CHANNEL_DATA => {
                debug!("channel_data");
                let mut r = buf.reader(1);
                let channel_num = ChannelId(r.read_u32()?);
                let data = r.read_string()?;
                let target = self.common.config.window_size;
                let mut c = client.take().unwrap();
                if let Some(ref mut enc) = self.common.encrypted {
                    if enc.adjust_window_size(channel_num, data, target) {
                        let next_window = c.adjust_window(channel_num, self.target_window_size);
                        if next_window > 0 {
                            self.target_window_size = next_window
                        }
                    }
                }
                let (c, s) = c.data(channel_num, &data, self).await?;
                *client = Some(c);
                Ok(s)
            }
            msg::CHANNEL_EXTENDED_DATA => {
                debug!("channel_extended_data");
                let mut r = buf.reader(1);
                let channel_num = ChannelId(r.read_u32()?);
                let extended_code = r.read_u32()?;
                let data = r.read_string()?;
                let target = self.common.config.window_size;
                let mut c = client.take().unwrap();
                if let Some(ref mut enc) = self.common.encrypted {
                    if enc.adjust_window_size(channel_num, data, target) {
                        let next_window = c.adjust_window(channel_num, self.target_window_size);
                        if next_window > 0 {
                            self.target_window_size = next_window
                        }
                    }
                }
                let (c, s) = c
                    .extended_data(channel_num, extended_code, &data, self)
                    .await?;
                *client = Some(c);
                Ok(s)
            }
            msg::CHANNEL_REQUEST => {
                let mut r = buf.reader(1);
                let channel_num = ChannelId(r.read_u32()?);
                let req = r.read_string()?;
                debug!(
                    "channel_request: {:?} {:?}",
                    channel_num,
                    std::str::from_utf8(req)
                );
                let cl = client.take().unwrap();
                let (c, s) = match req {
                    b"forwarded_tcpip" => {
                        let a = std::str::from_utf8(r.read_string()?)?;
                        let b = r.read_u32()?;
                        let c = std::str::from_utf8(r.read_string()?)?;
                        let d = r.read_u32()?;
                        cl.channel_open_forwarded_tcpip(channel_num, a, b, c, d, self)
                            .await?
                    }
                    b"xon-xoff" => {
                        r.read_byte()?; // should be 0.
                        let client_can_do = r.read_byte()?;
                        cl.xon_xoff(channel_num, client_can_do != 0, self).await?
                    }
                    b"exit-status" => {
                        r.read_byte()?; // should be 0.
                        let exit_status = r.read_u32()?;
                        cl.exit_status(channel_num, exit_status, self).await?
                    }
                    b"exit-signal" => {
                        r.read_byte()?; // should be 0.
                        let signal_name = Sig::from_name(r.read_string()?)?;
                        let core_dumped = r.read_byte()?;
                        let error_message = std::str::from_utf8(r.read_string()?)?;
                        let lang_tag = std::str::from_utf8(r.read_string()?)?;
                        cl.exit_signal(
                            channel_num,
                            signal_name,
                            core_dumped != 0,
                            error_message,
                            lang_tag,
                            self,
                        )
                        .await?
                    }
                    _ => {
                        info!("Unknown channel request {:?}", std::str::from_utf8(req));
                        (cl, self)
                    }
                };
                *client = Some(c);
                Ok(s)
            }
            msg::CHANNEL_WINDOW_ADJUST => {
                debug!("channel_window_adjust");
                let mut r = buf.reader(1);
                let channel_num = ChannelId(r.read_u32()?);
                let amount = r.read_u32()?;
                let mut new_value = 0;
                debug!("amount: {:?}", amount);
                if let Some(ref mut enc) = self.common.encrypted {
                    if let Some(ref mut channel) = enc.channels.get_mut(&channel_num) {
                        channel.recipient_window_size += amount;
                        new_value = channel.recipient_window_size;
                    } else {
                        return Err(Error::WrongChannel.into());
                    }
                }
                let c = client.take().unwrap();
                let (c, s) = c.window_adjusted(channel_num, new_value, self).await?;
                *client = Some(c);
                Ok(s)
            }
            msg::GLOBAL_REQUEST => {
                let mut r = buf.reader(1);
                let req = r.read_string()?;
                info!("Unhandled global request: {:?}", std::str::from_utf8(req));
                Ok(self)
            }
            msg::CHANNEL_SUCCESS => {
                let mut r = buf.reader(1);
                let channel_num = ChannelId(r.read_u32()?);
                let c = client.take().unwrap();
                let (c, s) = c.channel_success(channel_num, self).await?;
                *client = Some(c);
                Ok(s)
            }
            msg::CHANNEL_OPEN => self.client_handle_channel_open(buf).await,
            _ => {
                info!("Unhandled packet: {:?}", buf);
                Ok(self)
            }
        }
    }

    pub(crate) fn create_forwarded_tcpip_channel(
        &mut self,
        remote_channel: u32,
        sender: UnboundedSender<OpenChannelMsg>,
        window_size: u32,
        max_packet_size: u32,
    ) -> Result<(), anyhow::Error> {
        if let Some(ref mut enc) = self.common.encrypted {
            let forwarded_channel_id = enc.new_channel_id();
            let (open_msg_sender, open_msg_receiver) = unbounded_channel();
            self.channels.insert(forwarded_channel_id, open_msg_sender);
            debug!("=== channel inserted: {}", forwarded_channel_id.0);
            let enc_ch = crate::Channel {
                recipient_channel: remote_channel,
                sender_channel: forwarded_channel_id,
                recipient_window_size: window_size,
                sender_window_size: self.common.config.window_size,
                recipient_maximum_packet_size: max_packet_size,
                sender_maximum_packet_size: self.common.config.maximum_packet_size,
                /// Has the other side confirmed the channel?
                confirmed: true,
                wants_reply: false,
                pending_data: std::collections::VecDeque::new(),
            };
            enc.channels.insert(forwarded_channel_id, enc_ch);
            self.confirm_channel_open(remote_channel, forwarded_channel_id.0);
            sender
                .send(OpenChannelMsg::Open {
                    id: forwarded_channel_id,
                    window_size,
                    max_packet_size,
                })
                .map_err(|e| {
                    error!("failed to send channel open msg on creating forwarded tcpip channel")
                });
            info!("=== sent open msg");
            Ok(())
        } else {
            Err(Error::Inconsistent.into())
        }
    }

    async fn client_handle_channel_open(mut self, buf: &[u8]) -> Result<Self, anyhow::Error> {
        debug!("client handling channel open");
        // https://tools.ietf.org/html/rfc4254#section-5.1
        let mut r = buf.reader(1);
        let typ = r.read_string()?;
        let sender_channel = r.read_u32()?;
        let window = r.read_u32()?;
        let maxpacket = r.read_u32()?;
        debug!("server request open channel:");
        debug!("--- sender: {}", sender_channel);
        debug!("--- type: {}", String::from_utf8_lossy(typ));
        debug!("--- window: {}", window);
        debug!("--- maxpacket: {}", maxpacket);
        match typ {
            b"forwarded-tcpip" => {
                let address = r.read_string()?;
                debug!("-- connect addr: {}", String::from_utf8_lossy(address));
                let port = r.read_u32()?;
                debug!("-- connect port: {}", port);
                let orig_addr = r.read_string()?;
                debug!("-- origin addr: {}", String::from_utf8_lossy(orig_addr));
                let orig_port = r.read_u32()?;
                debug!("-- origin port: {}", orig_port);
                let ip_addr: IpAddr = match String::from_utf8_lossy(address).parse() {
                    Ok(add) => add,
                    Err(e) => {
                        error!("invalid forwarded-tcpip channel open request: {:?}", e);
                        return Ok(self);
                    }
                };
                let local_addr = SocketAddr::new(ip_addr, port as u16);
                let ch = match self
                    .remote_forward_channels
                    .get(&local_addr)
                    .and_then(|ch_id| self.channels.get(ch_id))
                {
                    Some(ch) => ch.clone(),
                    None => return Ok(self),
                };
                self.create_forwarded_tcpip_channel(sender_channel, ch, window, maxpacket)?;
                Ok(self)
            }
            t => {
                debug!("unknown channel type: {:?}", t);
                if let Some(ref mut enc) = self.common.encrypted {
                    push_packet!(enc.write, {
                        enc.write.push(msg::CHANNEL_OPEN_FAILURE);
                        enc.write.push_u32_be(sender_channel);
                        enc.write.push_u32_be(3); // SSH_OPEN_UNKNOWN_CHANNEL_TYPE
                        enc.write.extend_ssh_string(b"Unknown channel type");
                        enc.write.extend_ssh_string(b"en");
                    });
                }
                Ok(self)
            }
        }
    }

    pub fn confirm_channel_open(&mut self, remote_channel_no: u32, local_channel_no: u32) {
        if let Some(ref mut enc) = self.common.encrypted {
            debug!("=== enc channels: {:#?}", enc.channels);
            client_confirm_channel_open(
                &mut enc.write,
                remote_channel_no,
                local_channel_no,
                self.common.config.as_ref(),
            );
        }
    }

    pub(crate) fn write_auth_request_if_needed(&mut self, user: &str, meth: auth::Method) -> bool {
        let mut is_waiting = false;
        if let Some(ref mut enc) = self.common.encrypted {
            is_waiting = match enc.state {
                EncryptedState::WaitingAuthRequest(_) => true,
                EncryptedState::WaitingServiceRequest {
                    accepted,
                    ref mut sent,
                } => {
                    debug!("sending ssh-userauth service requset");
                    if !*sent {
                        let p = b"\x05\0\0\0\x0Cssh-userauth";
                        self.common.cipher.write(p, &mut self.common.write_buffer);
                        *sent = true
                    }
                    accepted
                }
                EncryptedState::InitCompression | EncryptedState::Authenticated => false,
            };
            debug!(
                "write_auth_request_if_needed: is_waiting = {:?}",
                is_waiting
            );
            if is_waiting {
                enc.write_auth_request(user, &meth);
            }
        }
        self.common.auth_user.clear();
        self.common.auth_user.push_str(user);
        self.common.auth_method = Some(meth);
        is_waiting
    }
}

impl Encrypted {
    fn write_auth_request(&mut self, user: &str, auth_method: &auth::Method) -> bool {
        // The server is waiting for our USERAUTH_REQUEST.
        push_packet!(self.write, {
            self.write.push(msg::USERAUTH_REQUEST);

            match *auth_method {
                auth::Method::Password { ref password } => {
                    self.write.extend_ssh_string(user.as_bytes());
                    self.write.extend_ssh_string(b"ssh-connection");
                    self.write.extend_ssh_string(b"password");
                    self.write.push(1);
                    self.write.extend_ssh_string(password.as_bytes());
                    true
                }
                auth::Method::PublicKey { ref key } => {
                    self.write.extend_ssh_string(user.as_bytes());
                    self.write.extend_ssh_string(b"ssh-connection");
                    self.write.extend_ssh_string(b"publickey");
                    self.write.push(0); // This is a probe

                    debug!("write_auth_request: {:?}", key.name());
                    self.write.extend_ssh_string(key.name().as_bytes());
                    key.push_to(&mut self.write);
                    true
                }
                auth::Method::FuturePublicKey { ref key, .. } => {
                    self.write.extend_ssh_string(user.as_bytes());
                    self.write.extend_ssh_string(b"ssh-connection");
                    self.write.extend_ssh_string(b"publickey");
                    self.write.push(0); // This is a probe

                    self.write.extend_ssh_string(key.name().as_bytes());
                    key.push_to(&mut self.write);
                    true
                }
            }
        })
    }

    fn client_make_to_sign<Key: Named + PubKey>(
        &mut self,
        user: &str,
        key: &Key,
        buffer: &mut CryptoVec,
    ) -> usize {
        buffer.clear();
        buffer.extend_ssh_string(self.session_id.as_ref());

        let i0 = buffer.len();
        buffer.push(msg::USERAUTH_REQUEST);
        buffer.extend_ssh_string(user.as_bytes());
        buffer.extend_ssh_string(b"ssh-connection");
        buffer.extend_ssh_string(b"publickey");
        buffer.push(1);
        buffer.extend_ssh_string(key.name().as_bytes());
        key.push_to(buffer);
        i0
    }

    fn client_send_signature(
        &mut self,
        user: &str,
        method: &auth::Method,
        buffer: &mut CryptoVec,
    ) -> Result<(), anyhow::Error> {
        match method {
            &auth::Method::PublicKey { ref key } => {
                let i0 = self.client_make_to_sign(user, key.as_ref(), buffer);
                // Extend with self-signature.
                key.add_self_signature(buffer)?;
                push_packet!(self.write, {
                    self.write.extend(&buffer[i0..]);
                })
            }
            _ => {}
        }
        Ok(())
    }
}

fn client_confirm_channel_open(
    buffer: &mut CryptoVec,
    remote_channel_no: u32,
    local_channel_no: u32,
    config: &super::Config,
) {
    push_packet!(buffer, {
        buffer.push(msg::CHANNEL_OPEN_CONFIRMATION);
        buffer.push_u32_be(remote_channel_no); // remote channel number.
        buffer.push_u32_be(local_channel_no); // our channel number.
        buffer.push_u32_be(config.window_size);
        buffer.push_u32_be(config.maximum_packet_size);
    });
}
