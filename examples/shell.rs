extern crate env_logger;
extern crate futures;
extern crate thrussh;
extern crate thrussh_keys;
extern crate tokio;
use anyhow::Context;
use std::sync::Arc;
use thrussh::client::shell::{upgrade_to_shell, ShellChannel};
use thrussh::*;
use thrussh_keys::*;

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
use futures::FutureExt;
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
    let mut channel = session.channel_open_session().await.unwrap();
    let mut channel = upgrade_to_shell(channel).await.unwrap();

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

    tokio::time::sleep(std::time::Duration::from_secs(10000)).await;
    session
        .disconnect(Disconnect::ByApplication, "", "English")
        .await
        .unwrap();
    let res = session.await.context("session await");
    println!("{:#?}", res);
}
