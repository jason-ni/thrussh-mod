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
    let mut channel = session.channel_open_session().await.unwrap();
    println!("=== new channel: {:?}", channel);
    let mut file = tokio::fs::File::open("Cargo.lock").await.unwrap();
    channel.exec(true, "scp -vt .").await.unwrap();
    let msg = channel.wait().await.unwrap();
    println!("=== msg : {:?}", msg);
    let msg = channel.wait().await.unwrap();
    println!("=== msg : {:?}", msg);
    /*
    channel
        .data(b"C0644 21620 Cargo.lock\n".as_ref())
        .await
        .unwrap();

     */
    println!("=== after write head");
    let (mut rh, mut wh) = channel.split().unwrap();
    let ret = rh.read_u8().await.unwrap();
    println!("scp ret: {}", ret);
    // starting scp command successfully returns 0 in one byte.
    assert_eq!(0, ret);
    wh.write_all(b"C0644 21620 Cargo.lock\n".as_ref())
        .await
        .unwrap();
    let mut buf: String = String::new();
    file.read_to_string(&mut buf).await.unwrap();
    println!("=== after read file");
    wh.write_all(&buf.as_bytes()).await.unwrap();
    println!("=== after write buf");
    wh.write_u8(0).await.unwrap();
    println!("=== after write zeo");
    wh.shutdown().await.unwrap();
    session
        .disconnect(Disconnect::ByApplication, "", "English")
        .await
        .unwrap();
}
