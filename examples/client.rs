extern crate thrussh;
extern crate thrussh_keys;
extern crate futures;
extern crate tokio;
extern crate env_logger;
use std::sync::Arc;
use thrussh::*;
use thrussh_keys::*;
use anyhow::Context;


struct Client {
}

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
    /*
    fn channel_open_confirmation(self, channel: ChannelId, max_packet_size: u32, window_size: u32, session: client::Session) -> Self::FutureUnit {
        println!("channel_open_confirmation: {:?}", channel);
        self.finished(session)
    }
    fn data(self, channel: ChannelId, data: &[u8], session: client::Session) -> Self::FutureUnit {
        println!("data on channel {:?}: {:?}", channel, std::str::from_utf8(data));
        self.finished(session)
    }

     */
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let config = thrussh::client::Config::default();
    let config = Arc::new(config);
    let sh = Client{};

    //let key = thrussh_keys::key::KeyPair::generate_ed25519().unwrap();
    //let mut agent = thrussh_keys::agent::client::AgentClient::connect_env().await.unwrap();
    //agent.add_identity(&key, &[]).await.unwrap();
    let kp = thrussh_keys::load_secret_key("id_rsa", None).unwrap();
    let mut session = thrussh::client::connect(config, "192.168.0.111:77", sh).await.unwrap();
    let kp_ref = Arc::new(kp);
    let auth_res = session.authenticate_publickey("xin", kp_ref).await.unwrap();
    println!("=== auth: {}", auth_res);
    let mut channel = session.channel_open_direct_tcpip(
        "127.0.0.1",
        80,
        "127.0.0.1",
        8989,
    ).await.unwrap();
    println!("=== after open channel");
    let data = b"GET / HTTP/1.1\nHost: 127.0.0.1:80\nUser-Agent: curl/7.68.0\nAccept: */*\nConnection: close\n\n";
    channel.data(&data[..]).await.unwrap();
    if let Some(msg) = channel.wait().await {
        println!("{:?}", msg);
        match msg {
            ChannelMsg::Data {data} => {
                let text = String::from_utf8_lossy(data.as_ref());
                println!("{}", text);
            },
            _ => (),
        }
        if let Some(msg) = channel.wait().await {
            println!("{:?}", msg);
        }
    }
    session.disconnect(Disconnect::ByApplication, "", "English").await.unwrap();
    let res = session.await.context("session await");
    println!("{:#?}", res);
}