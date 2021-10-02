use async_net::TcpStream;
use executor::Executor;
use flume::Sender;
use futures::{AsyncReadExt, AsyncWriteExt};
use std::{
    io::{self, BufRead},
    thread,
};
use tokio::select;

fn main() {
    let executor = Executor::new();
    executor.block_on(client())
}

async fn client() {
    let mut stream = TcpStream::connect("localhost:8080").await.unwrap();
    let (tx, rx) = flume::unbounded();

    thread::spawn(move || reader(tx));

    let mut nbuf = [0; 4];
    let mut mbuf = vec![0; 4];

    loop {
        select! {
            res = stream.read_exact(nbuf.as_mut()) => {
                res.unwrap();
                let len = u32::from_ne_bytes(nbuf) as usize;

                mbuf.resize(len, 0);
                stream.read_exact(mbuf.as_mut()).await.unwrap();

                let msg = std::str::from_utf8(mbuf.as_ref());
                if let Ok(msg) = msg {
                    let msg = msg.to_string();
                    executor::spawn_blocking(move || println!("{}", msg))
                        .await
                        .unwrap();
                }
            },
            res = rx.recv_async() => {
                let msg = res.unwrap();
                let len = (msg.len() as u32).to_ne_bytes();

                mbuf.resize(4 + msg.len(), 0);
                mbuf[..4].copy_from_slice(len.as_ref());
                mbuf[4..].copy_from_slice(msg.as_bytes());

                stream.write_all(&mbuf).await.unwrap();
            },
        }
    }
}

fn reader(tx: Sender<String>) {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut buf = String::new();

    loop {
        stdin.read_line(&mut buf).unwrap();
        tx.send(buf.trim().to_string()).unwrap();
        buf.clear();
    }
}