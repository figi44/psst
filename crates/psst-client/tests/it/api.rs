use psst_client::api::{ApiClient, Method};
use psst_proto::Endpoint;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn an_answer_too_large_to_hold_is_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = stream.read(&mut request).await;
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n").await;
        let megabyte = vec![b' '; 1 << 20];
        for _ in 0..65 {
            if stream.write_all(&megabyte).await.is_err() {
                return;
            }
        }
    });
    let api = ApiClient::new(Endpoint::Tcp { host: "127.0.0.1".into(), port }, None);
    let err = api.call::<serde_json::Value>(Method::Get, "/api/history", None::<&()>).await.unwrap_err();
    assert!(err.to_string().contains("larger than 64 MiB"), "{err}");
}
