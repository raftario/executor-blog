mod executor;
mod parking;

pub use crate::executor::{spawn, spawn_blocking, Executor};
pub use async_task::Task;

#[cfg(test)]
mod tests {
    #[test]
    fn block_on() {
        let four = super::Executor::new().block_on(async { 2 + 2 });
        assert_eq!(four, 4);
    }

    #[test]
    fn spawn() {
        let four = super::Executor::new().block_on(async { super::spawn(async { 2 + 2 }).await });
        assert_eq!(four, 4);
    }

    #[test]
    fn spawn_blocking() {
        let four = super::Executor::new()
            .block_on(async { super::spawn_blocking(|| 2 + 2).await.unwrap() });
        assert_eq!(four, 4);
    }

    #[test]
    fn tcp() {
        use async_net::{TcpListener, TcpStream};
        use futures::{AsyncReadExt, AsyncWriteExt};

        let server = async {
            let listener = TcpListener::bind("localhost:8080").await.unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();

            let mut buf = [0; 5];
            stream.read_exact(buf.as_mut()).await.unwrap();
            assert_eq!(buf.as_ref(), b"hello");

            stream.write_all(b"world").await.unwrap();
        };

        let client = async {
            let mut stream = TcpStream::connect("localhost:8080").await.unwrap();

            stream.write_all(b"hello").await.unwrap();

            let mut buf = [0; 5];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf.as_ref(), b"world");
        };

        super::Executor::new().block_on(async {
            let server = super::spawn(server);
            client.await;
            server.await;
        });
    }
}