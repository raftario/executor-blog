use async_net::{TcpListener, TcpStream};
use futures::{AsyncReadExt, AsyncWriteExt};
use std::{
    fmt,
    io::ErrorKind::{ConnectionAborted, ConnectionReset, Interrupted, UnexpectedEof},
};
use tokio::{
    select,
    sync::broadcast::{self, Receiver, Sender},
};
use tracing::{info, instrument};
use tracing_subscriber::{fmt::format::FmtSpan, EnvFilter};

fn main() {
    tracing_subscriber::fmt()
        .with_span_events(FmtSpan::CLOSE)
        .with_thread_names(true)
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    executor::block_on(server())
}

async fn server() {
    let listener = TcpListener::bind("localhost:8080").await.unwrap();
    let (tx, _) = broadcast::channel(1);

    loop {
        let (stream, _) = listener.accept().await.unwrap();
        executor::spawn(client(DbgStream(stream), tx.clone(), tx.subscribe())).detach();
    }
}

#[instrument(skip(tx, rx))]
async fn client(stream: DbgStream, tx: Sender<String>, mut rx: Receiver<String>) {
    let DbgStream(mut stream) = stream;

    info!("connected");

    let mut nbuf = [0; 4];
    let mut mbuf = vec![0; 4];

    loop {
        select! {
            res = stream.read_exact(nbuf.as_mut()) => {
                match res {
                    Err(e)
                        if e.kind() == ConnectionReset
                            || e.kind() == ConnectionAborted
                            || e.kind() == UnexpectedEof =>
                    {
                        break
                    }
                    Err(e) if e.kind() == Interrupted => continue,
                    res => {
                        res.unwrap();
                    }
                }

                let len = u32::from_ne_bytes(nbuf) as usize;

                mbuf.resize(len, 0);
                stream.read_exact(mbuf.as_mut()).await.unwrap();

                let msg = std::str::from_utf8(mbuf.as_ref());
                if let Ok(msg) = msg {
                    info!("message: {}", msg);

                    tx.send(msg.to_string()).unwrap();
                    rx.recv().await.unwrap();
                }
            },
            res = rx.recv() => {
                let msg = res.unwrap();
                let len = (msg.len() as u32).to_ne_bytes();

                mbuf.resize(4 + msg.len(), 0);
                mbuf[..4].copy_from_slice(len.as_ref());
                mbuf[4..].copy_from_slice(msg.as_bytes());

                stream.write_all(&mbuf).await.unwrap();
            },
        }
    }

    info!("disconnected");
}

struct DbgStream(TcpStream);

impl fmt::Debug for DbgStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TcpStream")
            .field(&self.0.peer_addr().unwrap())
            .finish()
    }
}
