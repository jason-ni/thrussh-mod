extern crate env_logger;
extern crate futures;
extern crate thrussh;
extern crate thrussh_keys;
extern crate tokio;
use anyhow::Context;
use futures::{StreamExt, TryFutureExt};
use log::{debug, error};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use thrussh::client::tunnel::{handle_connect, upgrade_to_remote_forward_tcpip_listener};
use thrussh::*;

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
    let channel = session
        .channel_open_session()
        .await
        .context("open channel")
        .unwrap();
    let mut listener = upgrade_to_remote_forward_tcpip_listener(channel, "127.0.0.1", 8989)
        .await
        .unwrap();
    while let Some(channel) = listener.next().await {
        println!("=== handling channel: {:?}", &channel);
        tokio::spawn(handle_connect(
            channel,
            SocketAddr::from_str("127.0.0.1:8081").unwrap(),
            |addr| {
                debug!("=== connecting to {:?}", &addr);
                tokio::net::TcpStream::connect(addr.clone()).map_err(move |e| {
                    error!("failed to connect {:?}: {:?}", addr, &e);
                    e
                })
            },
        ));
    }
    let _ = session
        .disconnect(Disconnect::ByApplication, "", "English")
        .await
        .map_err(|e| {
            println!("=== {:#?}", e);
        });
}
