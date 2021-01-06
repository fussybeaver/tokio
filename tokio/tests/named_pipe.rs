#![cfg(feature = "full")]
#![warn(rust_2018_idioms)]
#![cfg(target_os = "windows")]

use bytes::Buf;
use futures::{
    future::{select, try_join, Either},
    stream::FuturesUnordered,
    StreamExt,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{NamedPipe, NamedPipeServer},
    time::{sleep, Duration},
};

#[tokio::test]
async fn basic() -> std::io::Result<()> {
    const NUM_CLIENTS: u32 = 255;
    const PIPE_NAME: &'static str = r"\\.\pipe\test-named-pipe-basic";
    let mut buf = [0_u8; 16];

    // Create server to avoid NotFound from clients.
    let server = NamedPipeServer::new(PIPE_NAME)?;

    let server = async move {
        let mut pipe;
        for _ in 0..NUM_CLIENTS {
            pipe = server.accept().await?;
            let mut buf = Vec::new();
            pipe.read_buf(&mut buf).await?;
            let i = (&*buf).get_u32_le();
            pipe.write_all(format!("Server to {}", i).as_bytes())
                .await?;
        }
        std::io::Result::Ok(())
    };

    // concurrent clients
    let clients = (0..NUM_CLIENTS)
        .map(|i| async move {
            let mut pipe = NamedPipe::connect(PIPE_NAME).await?;
            pipe.write_all(&i.to_le_bytes()).await?;
            let mut buf = Vec::new();
            pipe.read_buf(&mut buf).await?;
            assert_eq!(buf, format!("Server to {}", i).as_bytes());
            std::io::Result::Ok(())
        })
        .collect::<FuturesUnordered<_>>()
        .fold(Ok(()), |a, x| async move { a.and(x) });

    try_join(server, clients).await?;

    // client returns not found if there is no server
    let err = NamedPipe::connect(PIPE_NAME).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

    let server = NamedPipeServer::new(PIPE_NAME)?.accept();
    let client = NamedPipe::connect(PIPE_NAME);
    let (mut server, mut client) = try_join(server, client).await?;

    ping_pong(&mut server, &mut client).await?;

    drop(server);

    // Client reads when server is gone
    let len = client.read(&mut buf).await.unwrap();
    assert_eq!(len, 0);

    drop(client);

    let server = NamedPipeServer::new(PIPE_NAME)?.accept();
    let client = NamedPipe::connect(PIPE_NAME);
    let (mut server, mut client) = try_join(server, client).await?;

    ping_pong(&mut server, &mut client).await?;

    drop(client);

    // Server reads when client is gone
    let len = server.read(&mut buf).await?;
    assert_eq!(len, 0);

    // There is no way to connect to a connected server instance
    // even if client is gone.
    let timeout = sleep(Duration::from_millis(300));
    let client = NamedPipe::connect(PIPE_NAME);
    futures::pin_mut!(client);
    futures::pin_mut!(timeout);
    let result = select(timeout, client).await;
    assert!(matches!(result, Either::Left(_)));

    Ok(())
}

async fn ping_pong(l: &mut NamedPipe, r: &mut NamedPipe) -> std::io::Result<()> {
    let mut buf = [b' '; 5];

    l.write_all(b"ping").await?;
    r.read(&mut buf).await?;
    assert_eq!(&buf, b"ping ");
    r.write_all(b"pong").await?;
    l.read(&mut buf).await?;
    assert_eq!(&buf, b"pong ");
    Ok(())
}
