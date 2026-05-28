use unidrive_mount::fake_jvm::{replies, FakeJvm};
use unidrive_mount::ipc::{IpcClient, IpcError};


#[tokio::test]
async fn open_read_round_trips() {
    let jvm = FakeJvm::spawn(replies(&[(
        "hydration.open_read",
        r#"{"ok":true,"cache_path":"/tmp/cache/foo.txt"}"#,
    )]))
    .await;
    let mut client = IpcClient::connect(&jvm.socket_path).await.unwrap();

    let reply = client.open_read("handle-1", "/foo.txt").await.unwrap();
    assert_eq!(reply.cache_path, std::path::Path::new("/tmp/cache/foo.txt"));

    let recorded = jvm.recorded_requests().await;
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].contains(r#""verb":"hydration.open_read""#));
    assert!(recorded[0].contains(r#""handle_id":"handle-1""#));
    assert!(recorded[0].contains(r#""path":"/foo.txt""#));

    jvm.shutdown().await;
}

#[tokio::test]
async fn open_write_round_trips_with_three_fields() {
    let jvm = FakeJvm::spawn(replies(&[(
        "hydration.open_write",
        r#"{"ok":true,"cache_path":"/tmp/cache/foo.txt"}"#,
    )]))
    .await;
    let mut client = IpcClient::connect(&jvm.socket_path).await.unwrap();

    let reply = client
        .open_write("handle-2", "/foo.txt", "/tmp/cache/foo.txt")
        .await
        .unwrap();
    assert_eq!(reply.cache_path, std::path::Path::new("/tmp/cache/foo.txt"));

    let recorded = jvm.recorded_requests().await;
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].contains(r#""verb":"hydration.open_write""#));
    assert!(recorded[0].contains(r#""handle_id":"handle-2""#));
    assert!(recorded[0].contains(r#""path":"/foo.txt""#));
    assert!(recorded[0].contains(r#""cache_path":"/tmp/cache/foo.txt""#));

    jvm.shutdown().await;
}

#[tokio::test]
async fn close_handle_round_trips() {
    let jvm = FakeJvm::spawn(replies(&[(
        "hydration.close_handle",
        r#"{"ok":true}"#,
    )]))
    .await;
    let mut client = IpcClient::connect(&jvm.socket_path).await.unwrap();

    client.close_handle("handle-3").await.unwrap();

    let recorded = jvm.recorded_requests().await;
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].contains(r#""verb":"hydration.close_handle""#));
    assert!(recorded[0].contains(r#""handle_id":"handle-3""#));

    jvm.shutdown().await;
}

#[tokio::test]
async fn hydrate_round_trips() {
    let jvm = FakeJvm::spawn(replies(&[("hydration.hydrate", r#"{"ok":true}"#)])).await;
    let mut client = IpcClient::connect(&jvm.socket_path).await.unwrap();

    client.hydrate("/bar.txt").await.unwrap();

    let recorded = jvm.recorded_requests().await;
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].contains(r#""verb":"hydration.hydrate""#));
    assert!(recorded[0].contains(r#""path":"/bar.txt""#));

    jvm.shutdown().await;
}

#[tokio::test]
async fn dehydrate_round_trips_ok() {
    let jvm = FakeJvm::spawn(replies(&[("hydration.dehydrate", r#"{"ok":true}"#)])).await;
    let mut client = IpcClient::connect(&jvm.socket_path).await.unwrap();

    client.dehydrate("/baz.txt").await.unwrap();

    let recorded = jvm.recorded_requests().await;
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].contains(r#""verb":"hydration.dehydrate""#));
    assert!(recorded[0].contains(r#""path":"/baz.txt""#));

    jvm.shutdown().await;
}

#[tokio::test]
async fn dehydrate_busy_surfaces_distinct_variant() {
    let jvm = FakeJvm::spawn(replies(&[(
        "hydration.dehydrate",
        r#"{"ok":false,"error":"busy"}"#,
    )]))
    .await;
    let mut client = IpcClient::connect(&jvm.socket_path).await.unwrap();

    let err = client.dehydrate("/open-file.txt").await.unwrap_err();
    assert!(matches!(err, IpcError::Busy), "expected IpcError::Busy, got {err:?}");

    jvm.shutdown().await;
}

#[tokio::test]
async fn subscribe_handshake_round_trips() {
    let jvm = FakeJvm::spawn(replies(&[("hydration.subscribe", r#"{"ok":true}"#)])).await;
    let mut client = IpcClient::connect(&jvm.socket_path).await.unwrap();

    client.subscribe().await.unwrap();

    let recorded = jvm.recorded_requests().await;
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].contains(r#""verb":"hydration.subscribe""#));

    jvm.shutdown().await;
}

#[tokio::test]
async fn last_synced_ok_returns_mtime() {
    let jvm = FakeJvm::spawn(replies(&[(
        "hydration.last_synced",
        r#"{"ok":true,"mtime_ms":1234567890123}"#,
    )]))
    .await;
    let mut client = IpcClient::connect(&jvm.socket_path).await.unwrap();

    let mtime = client.last_synced("/known.txt").await.unwrap();
    assert_eq!(mtime, 1234567890123);

    let recorded = jvm.recorded_requests().await;
    assert!(recorded[0].contains(r#""verb":"hydration.last_synced""#));
    assert!(recorded[0].contains(r#""path":"/known.txt""#));

    jvm.shutdown().await;
}

#[tokio::test]
async fn last_synced_unknown_surfaces_dynamic_reason() {
    let jvm = FakeJvm::spawn(replies(&[(
        "hydration.last_synced",
        r#"{"ok":false,"error":"unknown_path"}"#,
    )]))
    .await;
    let mut client = IpcClient::connect(&jvm.socket_path).await.unwrap();

    let err = client.last_synced("/missing.txt").await.unwrap_err();
    // Per the canonical contract (LastSyncedResult.Unknown(reason: String)),
    // the reason is dynamic — assert variant + non-empty reason, NOT a literal.
    match err {
        IpcError::Unknown { reason } => assert!(!reason.is_empty(), "reason must be non-empty"),
        other => panic!("expected IpcError::Unknown, got {other:?}"),
    }

    jvm.shutdown().await;
}

#[tokio::test]
async fn list_deserialises_entries() {
    let jvm = FakeJvm::spawn(replies(&[(
        "hydration.list",
        r#"{"ok":true,"entries":[{"path":"/a","size":10,"mtime_ms":1000,"hydrated":true,"folder":false},{"path":"/b","size":0,"mtime_ms":2000,"hydrated":false,"folder":true}]}"#,
    )]))
    .await;
    let mut client = IpcClient::connect(&jvm.socket_path).await.unwrap();

    let entries = client.list("/").await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].path, "/a");
    assert_eq!(entries[0].size, 10);
    assert_eq!(entries[0].mtime_ms, 1000);
    assert!(entries[0].hydrated);
    assert!(!entries[0].folder);
    assert_eq!(entries[1].path, "/b");
    assert!(!entries[1].hydrated);
    assert!(entries[1].folder);

    let recorded = jvm.recorded_requests().await;
    assert!(recorded[0].contains(r#""verb":"hydration.list""#));
    assert!(recorded[0].contains(r#""prefix":"/""#));

    jvm.shutdown().await;
}

#[tokio::test]
async fn open_write_begin_ok_returns_cache_path() {
    let jvm = FakeJvm::spawn(replies(&[(
        "hydration.open_write_begin",
        r#"{"ok":true,"cache_path":"/cache/x.txt"}"#,
    )]))
    .await;
    let mut client = IpcClient::connect(&jvm.socket_path).await.unwrap();

    // No handle_id: one-shot / bare-truncate path.
    let reply = client.open_write_begin("/x.txt", None).await.unwrap();
    assert_eq!(reply.cache_path, std::path::PathBuf::from("/cache/x.txt"));

    let recorded = jvm.recorded_requests().await;
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].contains(r#""verb":"hydration.open_write_begin""#));
    assert!(recorded[0].contains(r#""path":"/x.txt""#));
    // No handle_id in request (one-shot path).
    assert!(!recorded[0].contains(r#""handle_id""#));

    jvm.shutdown().await;
}

#[tokio::test]
async fn open_write_begin_with_handle_id_includes_it_in_request() {
    let jvm = FakeJvm::spawn(replies(&[(
        "hydration.open_write_begin",
        r#"{"ok":true,"cache_path":"/cache/x.txt"}"#,
    )]))
    .await;
    let mut client = IpcClient::connect(&jvm.socket_path).await.unwrap();

    // With handle_id: live O_TRUNC open path.
    let _reply = client.open_write_begin("/x.txt", Some("wh-7")).await.unwrap();

    let recorded = jvm.recorded_requests().await;
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].contains(r#""handle_id":"wh-7""#),
        "handle_id must be present in request when provided: {}", recorded[0]);

    jvm.shutdown().await;
}

#[tokio::test]
async fn open_write_begin_error_surfaces_server_error() {
    let jvm = FakeJvm::spawn(replies(&[(
        "hydration.open_write_begin",
        r#"{"ok":false,"error":"unknown_path"}"#,
    )]))
    .await;
    let mut client = IpcClient::connect(&jvm.socket_path).await.unwrap();

    let err = client.open_write_begin("/missing.txt", None).await.unwrap_err();
    assert!(
        matches!(err, IpcError::ServerError(ref s) if s == "unknown_path"),
        "expected IpcError::ServerError(\"unknown_path\"), got {err:?}"
    );

    jvm.shutdown().await;
}
