extern crate env_logger;
extern crate futures;
extern crate thrussh;
extern crate thrussh_keys;
extern crate tokio;
use std::str::FromStr;
use std::sync::Arc;
use thrussh::client::{channel::ChannelExt, shell::upgrade_to_shell};
use thrussh::*;
use thrussh_keys::*;
use tokio::net::TcpStream;

struct Client {}

impl client::Handler for Client {
    type FutureUnit = futures::future::Ready<Result<(Self, client::Session), anyhow::Error>>;
    type FutureBool = futures::future::Ready<Result<(Self, bool), anyhow::Error>>;

    fn finished_bool(self, b: bool) -> Self::FutureBool {
        futures::future::ready(Ok((self, b)))
    }
    fn finished(self, session: client::Session) -> Self::FutureUnit {
        futures::future::ready(Ok((self, session)))
    }
    fn check_server_key(self, server_public_key: &key::PublicKey) -> Self::FutureBool {
        println!("check_server_key: {:?}", server_public_key);
        self.finished_bool(true)
    }
}

use bytes::BytesMut;
use futures::{FutureExt, StreamExt};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() {
    env_logger::init();
    let config = thrussh::client::Config::default();
    let config = Arc::new(config);
    let sh = Client {};

    //let key = thrussh_keys::key::KeyPair::generate_ed25519().unwrap();
    //let mut agent = thrussh_keys::agent::client::AgentClient::connect_env().await.unwrap();
    //agent.add_identity(&key, &[]).await.unwrap();
    let kp = thrussh_keys::load_secret_key("id_rsa", None).unwrap();
    let mut session = thrussh::client::connect(config, "127.0.0.1:2222", sh)
        .await
        .unwrap();
    let kp_ref = Arc::new(kp);
    let auth_res = session
        .authenticate_publickey("jason", kp_ref)
        .await
        .unwrap();
    assert!(auth_res, true);
    let x11_channel = session.channel_open_session().await.unwrap();
    let mut channel = session.channel_open_session().await.unwrap();

    // The OpenSSH server assigns screen number automatically, if we request the x11_screen_number with 0.
    channel
        .request_x11(
            x11_channel.id(),
            false,
            false,
            "MIT-MAGIC-COOKIE-1",
            "TEST",
            0,
        )
        .await
        .unwrap();
    let listen_fut = async move {
        let mut listener = thrussh::client::tunnel::RemoteForwardListener::new(x11_channel);
        while let Some(mut ch) = listener.next().await {
            let fut = async move {
                // we assume the local display number is 127.0.0.1:0.0
                let addr = SocketAddr::from_str("127.0.0.1:6000").unwrap();
                let conn = match TcpStream::connect(addr).await {
                    Ok(conn) => conn,
                    Err(e) => {
                        // if we can't connect to X11 server, the forwarded channel should be closed.
                        ch.eof().await;
                        return Ok::<(), anyhow::Error>(());
                    }
                };
                let (conn_rh, conn_wh) = conn.into_split();
                let (ch_rh, ch_wh) = ch.split().expect("split channel error");
                tokio::spawn(thrussh::client::tunnel::copy(conn_rh, ch_wh, true));
                tokio::spawn(thrussh::client::tunnel::copy(ch_rh, conn_wh, true));
                Ok::<(), anyhow::Error>(())
            };
            tokio::spawn(fut);
        }
    };
    tokio::spawn(listen_fut);
    let channel = upgrade_to_shell(channel).await.unwrap();

    let (mut shell_reader, mut shell_writer) = channel.split().unwrap();
    let fut = async move {
        let mut buf = BytesMut::with_capacity(1024);
        loop {
            buf.clear();
            tokio::io::stdin().read_buf(&mut buf).await.unwrap();
            shell_writer.write_all(&buf).await.unwrap();
        }
    };
    tokio::spawn(fut.boxed());
    let mut buf: BytesMut = BytesMut::with_capacity(1024);
    loop {
        shell_reader.read_buf(&mut buf).await.unwrap();
        tokio::io::stdout().write_all(&buf).await.unwrap();
        buf.clear();
    }
}
