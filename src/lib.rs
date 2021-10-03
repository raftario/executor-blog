#![forbid(unsafe_code)]

mod executor;
mod parking;

pub use crate::executor::{block_on, spawn, spawn_blocking, Executor};
pub use async_task::Task;

#[cfg(test)]
mod tests {
    use std::sync::Once;
    use tracing_subscriber::{fmt::format::FmtSpan, EnvFilter};

    fn init() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            tracing_subscriber::fmt()
                .with_test_writer()
                .with_span_events(FmtSpan::CLOSE)
                .with_thread_names(true)
                .with_env_filter(EnvFilter::from_default_env())
                .init()
        });
    }

    #[test]
    fn block_on() {
        init();

        let four = super::block_on(async { 2 + 2 });
        assert_eq!(four, 4);
    }

    #[test]
    fn spawn() {
        init();

        let four = super::block_on(async { super::spawn(async { 2 + 2 }).await });
        assert_eq!(four, 4);
    }

    #[test]
    fn spawn_blocking() {
        init();

        let four = super::block_on(async { super::spawn_blocking(|| 2 + 2).await });
        assert_eq!(four, 4);
    }

    #[test]
    fn spawn_spawn_blocking() {
        init();

        let four = super::block_on(async {
            super::spawn(async { super::spawn_blocking(|| 2 + 2).await }).await
        });
        assert_eq!(four, 4);
    }

    #[test]
    fn spawn_blocking_spawn() {
        init();

        let four = super::block_on(async {
            let four = super::spawn_blocking(|| super::spawn(async { 2 + 2 })).await;
            four.await
        });
        assert_eq!(four, 4);
    }

    #[test]
    fn tcp() {
        init();

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

        super::block_on(async {
            let server = super::spawn(server);
            client.await;
            server.await;
        });
    }
}
