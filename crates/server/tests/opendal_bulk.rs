//! Exercise the store's bulk-delete contract over the actual S3 adapter.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::{Router, extract::State, routing::post};
use git_cdc_core::store::{OpendalConfig, OpendalStore};

#[derive(Default)]
struct DeleteState {
    batches: Mutex<Vec<Vec<String>>>,
    reject: AtomicBool,
}

async fn delete(State(state): State<Arc<DeleteState>>, body: String) -> String {
    let keys: Vec<_> = body
        .split("<Key>")
        .skip(1)
        .map(|part| part.split_once("</Key>").unwrap().0.to_owned())
        .collect();
    assert!(!keys.is_empty());
    let response = if state.reject.load(Ordering::SeqCst) {
        format!(
            "<DeleteResult><Error><Key>{}</Key><Code>AccessDenied</Code><Message>denied</Message></Error></DeleteResult>",
            keys[0]
        )
    } else {
        "<DeleteResult/>".to_owned()
    };
    state.batches.lock().unwrap().push(keys);
    response
}

#[tokio::test]
async fn s3_deletes_use_native_batches_and_propagate_per_key_errors() {
    let state = Arc::new(DeleteState::default());
    let service = Router::new()
        .route("/{*path}", post(delete))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    let store = OpendalStore::connect(&OpendalConfig {
        scheme: "s3".into(),
        options: vec![
            ("endpoint".into(), endpoint),
            ("bucket".into(), "chunks-test".into()),
            ("region".into(), "us-east-1".into()),
            ("access_key_id".into(), "test".into()),
            ("secret_access_key".into(), "test".into()),
            ("disable_config_load".into(), "true".into()),
        ],
        prefix: "chunks/".into(),
    })
    .unwrap();
    let hashes: Vec<_> = (0u64..2005)
        .map(|n| blake3::hash(&n.to_le_bytes()))
        .collect();
    store.remove_many(&[]).await.unwrap();
    assert!(state.batches.lock().unwrap().is_empty());
    store.remove_many(&hashes).await.unwrap();
    {
        let batches = state.batches.lock().unwrap();
        let mut lengths: Vec<_> = batches.iter().map(Vec::len).collect();
        lengths.sort_unstable();
        assert_eq!(lengths, [5, 1000, 1000]);
        let actual: std::collections::BTreeSet<_> = batches.iter().flatten().cloned().collect();
        let expected = hashes
            .iter()
            .map(|hash| format!("chunks/{}", hash.to_hex()))
            .collect();
        assert_eq!(actual, expected);
    }
    state.reject.store(true, Ordering::SeqCst);
    assert!(
        store.remove_many(&hashes[..2]).await.is_err(),
        "a successful HTTP response with a failed key must fail the sweep"
    );
    server.abort();
}
