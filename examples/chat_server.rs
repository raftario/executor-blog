use async_net::{TcpListener, TcpStream};
use futures::{AsyncReadExt, AsyncWriteExt};
use tokio::{
    select,
    sync::broadcast::{self, Receiver, Sender},
};

fn main() {
    executor::block_on(server())
}

async fn server() {
    let listener = TcpListener::bind("localhost:8080").await.unwrap();
    let (tx, _) = broadcast::channel(1);

    loop {
        let (stream, _) = listener.accept().await.unwrap();
        executor::spawn(client(stream, tx.clone(), tx.subscribe())).detach();
    }
}

async fn client(mut stream: TcpStream, tx: Sender<String>, mut rx: Receiver<String>) {
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
}
