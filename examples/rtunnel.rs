extern crate env_logger;
extern crate futures;
extern crate thrussh;
extern crate thrussh_keys;
extern crate tokio;
use anyhow::Context;
use log::debug;
use std::net::SocketAddr;
use std::sync::Arc;
use thrussh::*;
use thrussh_keys::*;
use tokio::net::TcpStream;
use tokio::sync::mpsc::Sender;

#[tokio::main]
async fn main() {
    env_logger::init();
    let config = thrussh::client::Config::default();
    let config = Arc::new(config);
    let sh = client::tunnel::TunnelClient::new();

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
    println!("=== auth: {}", auth_res);
    let mut channel = session
        .channel_open_session()
        .await
        .context("open channel")
        .unwrap();
    channel
        .tcpip_forward(false, "127.0.0.1", 8989)
        .await
        .unwrap();
    if let Some(msg) = channel.wait().await {
        println!("=== {:#?}", msg);
    }
    tokio::time::sleep(std::time::Duration::from_secs(10000)).await;
    session
        .disconnect(Disconnect::ByApplication, "", "English")
        .await
        .map_err(|e| {
            println!("=== {:#?}", e);
        });
    let res = session.await.context("session await");
    println!("{:#?}", res);
}
