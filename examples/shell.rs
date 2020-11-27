extern crate env_logger;
extern crate futures;
extern crate thrussh;
extern crate thrussh_keys;
extern crate tokio;
use anyhow::Context;
use std::sync::Arc;
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
    channel
        .request_pty(true, "vt100", 80, 60, 640, 480, &[])
        .await
        .unwrap();
    if let Some(msg) = channel.wait().await {
        println!("response of pty in channel: {:?}", msg);
        channel.request_shell(true).await.unwrap();
        println!("request shell");
        if let Some(msg) = channel.wait().await {
            println!("response of shell in channel: {:?}", msg);
            match msg {
                ChannelMsg::WindowAdjusted { new_size } => {
                    println!("server tells us initial window size: {}", new_size);
                }
                other => {
                    println!("other msg: {:?}", other);
                }
            };
        }
        println!("should be success message");
        if let Some(msg) = channel.wait().await {
            println!("response of shell in channel: {:?}", msg);
            match msg {
                ChannelMsg::Success => {
                    println!("shell request success");
                }
                other => {
                    println!("other msg: {:?}", other);
                }
            }

            channel.data("top\n".as_bytes()).await.unwrap();
            while let Some(msg) = channel.wait().await {
                match msg {
                    ChannelMsg::Data { data } => {
                        println!("{}", String::from_utf8_lossy(data.as_ref()));
                    }
                    other => println!("other message: {:?}", other),
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(10000)).await;
    }
    session
        .disconnect(Disconnect::ByApplication, "", "English")
        .await
        .unwrap();
    let res = session.await.context("session await");
    println!("{:#?}", res);
}
