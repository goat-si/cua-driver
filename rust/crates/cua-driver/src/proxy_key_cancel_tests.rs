#[tokio::test]
async fn cancelled_key_requests_report_release_pending_without_worker_acknowledgement() {
    let mut receipts = Vec::new();
    for tool in ["press_key", "hotkey"] {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(started_tx)));
        let run = tokio::spawn(run_concurrent_mcp_io(
            BufReader::new(server_read),
            server_write,
            Duration::from_secs(1),
            |_| Ok(()),
            move |_, id| {
                let started_tx = started_tx.clone();
                async move {
                    if let Some(tx) = started_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    std::future::pending::<()>().await;
                    Response::ok(id, serde_json::json!({}))
                }
            },
        ));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut client_read = BufReader::new(client_read);
        client_write
            .write_all(modern_tool_call(7, tool).as_bytes())
            .await
            .unwrap();
        started_rx.await.expect("tool future entered");
        client_write
            .write_all(
                br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}
"#,
            )
            .await
            .unwrap();
        let cancelled = next_response(&mut client_read).await;
        client_write.shutdown().await.unwrap();
        drop(client_write);
        run.await.unwrap().unwrap();
        receipts.push((tool, cancelled));
    }
    assert!(
        receipts.iter().all(
            |(_, receipt)| receipt["result"]["structuredContent"]["effect"] == "unknown"
                && receipt["result"]["structuredContent"]["key_release"] == "pending"
        ),
        "{receipts:?}"
    );
}
