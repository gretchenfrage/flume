#[cfg(not(target_os = "unknown"))]
#[async_std::main]
async fn main() {
    let (tx, rx) = flume::bounded(1);

    let t = async_std::task::spawn(async move {
        while let Ok(msg) = rx.recv_async().await {
            println!("Received: {}", msg);
        }
    });

    tx.send_async("Hello, world!").await.unwrap();
    tx.send_async("How are you today?").await.unwrap();

    drop(tx);

    t.await;
}

#[cfg(target_os = "unknown")]
fn main() {}
