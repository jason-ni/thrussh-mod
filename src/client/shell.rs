use crate::client::Channel;
use crate::pty::Pty;
use crate::ChannelMsg;

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
            ChannelMsg::WindowAdjusted { new_size } => channel.window_size = new_size,
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
