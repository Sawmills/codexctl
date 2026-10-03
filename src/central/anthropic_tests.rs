use super::*;
use axum::{
    Json, Router,
    routing::{get, post},
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn cached_usage_never_acquires_a_token_and_machines_share_polling() {
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let app=Router::new()
        .route("/api/oauth/profile",get(||async{Json(json!({"account":{"uuid":"a"},"organization":{"uuid":"o"}}))}))
        .route("/api/oauth/usage",get(move||{let calls=calls.clone();async move{calls.fetch_add(1,Ordering::SeqCst);Json(json!({"five_hour":{"utilization":42,"resets_at":"2099-01-01T00:00:00Z"},"seven_day":null}))}}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("key");
    vault::create_secret(&key, &[8; 32]).unwrap();
    let engine = Engine::open_at(
        &root.path().join("store"),
        &key,
        false,
        Endpoints {
            api: origin.clone(),
            token: format!("{origin}/v1/oauth/token"),
        },
    )
    .unwrap();
    let receipt = engine
        .admit(
            "person",
            "work",
            "first",
            Grant {
                access_token: "fake-a".into(),
                refresh_token: "fake-ra".into(),
                expires_at: now() + 3600000,
                scopes: vec!["user:inference".into(), "user:profile".into()],
            },
            None,
        )
        .await
        .unwrap();
    assert!(
        engine
            .usage("person", &receipt.account_id, true)
            .await
            .unwrap()
            .data
            .is_none()
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let (a, b) = tokio::join!(
        engine.usage("person", &receipt.account_id, false),
        engine.usage("person", &receipt.account_id, false)
    );
    assert_eq!(a.unwrap().data.unwrap()["five_hour"]["utilization"], 42);
    assert!(b.unwrap().data.unwrap()["seven_day"].is_null());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn concurrent_rejections_refresh_once_and_restart_preserves_the_successor() {
    let count = Arc::new(AtomicUsize::new(0));
    let refresh_count = count.clone();
    let app = Router::new()
        .route("/api/oauth/profile", get(|| async { Json(json!({"account":{"uuid":"account-a"},"organization":{"uuid":"organization-a"}})) }))
        .route("/v1/oauth/token", post(move || { let count = refresh_count.clone(); async move {
            count.fetch_add(1, Ordering::SeqCst);
            Json(json!({"access_token":"synthetic-b","refresh_token":"synthetic-rb","expires_in":3600,"scope":"user:inference user:profile"}))
        }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("key");
    vault::create_secret(&key, &[7; 32]).unwrap();
    let store = root.path().join("provider");
    let engine = Engine::open_at(
        &store,
        &key,
        false,
        Endpoints {
            api: origin.clone(),
            token: format!("{origin}/v1/oauth/token"),
        },
    )
    .unwrap();
    let grant = Grant {
        access_token: "synthetic-a".into(),
        refresh_token: "synthetic-ra".into(),
        expires_at: now() + 3600000,
        scopes: vec!["user:inference".into(), "user:profile".into()],
    };
    let receipt = engine
        .admit("person-a", "work", "migration-1", grant, None)
        .await
        .unwrap();
    let current = engine
        .acquire("person-a", &receipt.account_id, None)
        .await
        .unwrap();
    let (left, right) = tokio::join!(
        engine.acquire("person-a", &receipt.account_id, Some(&current.revision)),
        engine.acquire("person-a", &receipt.account_id, Some(&current.revision))
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_eq!(left.access_token, "synthetic-b");
    assert_eq!(right.revision, left.revision);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(
        engine
            .acquire("person-b", &receipt.account_id, None)
            .await
            .is_err()
    );
    drop(engine);
    let engine = Engine::open_at(
        &store,
        &key,
        false,
        Endpoints {
            api: origin.clone(),
            token: format!("{origin}/v1/oauth/token"),
        },
    )
    .unwrap();
    let restarted = engine
        .acquire("person-a", &receipt.account_id, None)
        .await
        .unwrap();
    assert_eq!(restarted.revision, left.revision);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let serialized = serde_json::to_string(&restarted).unwrap();
    assert!(!serialized.contains("refresh"));
    assert!(!serialized.contains("synthetic-rb"));
    task.abort();
}

#[tokio::test]
async fn lost_refresh_response_is_never_replayed_after_restart() {
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let app = Router::new()
        .route(
            "/api/oauth/profile",
            get(|| async { Json(json!({"account":{"uuid":"a"},"organization":{"uuid":"o"}})) }),
        )
        .route(
            "/v1/oauth/token",
            post(move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::BAD_GATEWAY
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("key");
    vault::create_secret(&key, &[9; 32]).unwrap();
    let open = || {
        Engine::open_at(
            &root.path().join("store"),
            &key,
            false,
            Endpoints {
                api: origin.clone(),
                token: format!("{origin}/v1/oauth/token"),
            },
        )
        .unwrap()
    };
    let engine = open();
    let receipt = engine
        .admit(
            "person",
            "work",
            "first",
            Grant {
                access_token: "fake-a".into(),
                refresh_token: "fake-ra".into(),
                expires_at: now() + 3600000,
                scopes: vec!["user:inference".into(), "user:profile".into()],
            },
            None,
        )
        .await
        .unwrap();
    let access = engine
        .acquire("person", &receipt.account_id, None)
        .await
        .unwrap();
    assert!(
        engine
            .acquire("person", &receipt.account_id, Some(&access.revision))
            .await
            .is_err()
    );
    drop(engine);
    let engine = open();
    assert!(
        engine
            .acquire("person", &receipt.account_id, None)
            .await
            .is_err()
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(!engine.accounts("person").await[0].available);
    task.abort();
}

#[tokio::test]
async fn admission_retry_verifies_the_retained_grant_without_replacing_it() {
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let app = Router::new().route(
        "/api/oauth/profile",
        get(move |headers: axum::http::HeaderMap| {
            let calls = calls.clone();
            async move {
                use axum::response::IntoResponse;
                assert_eq!(headers["authorization"], "Bearer original-access");
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
                } else {
                    Json(json!({"account":{"uuid":"a"},"organization":{"uuid":"o"}}))
                        .into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("key");
    vault::create_secret(&key, &[11; 32]).unwrap();
    let open = || {
        Engine::open_at(
            &root.path().join("store"),
            &key,
            false,
            Endpoints {
                api: origin.clone(),
                token: format!("{origin}/token"),
            },
        )
        .unwrap()
    };
    let grant = |access: &str| Grant {
        access_token: access.into(),
        refresh_token: "refresh".into(),
        expires_at: now() + 3600000,
        scopes: vec!["user:inference".into(), "user:profile".into()],
    };
    let engine = open();
    assert!(
        engine
            .admit(
                "person",
                "work",
                "migration",
                grant("original-access"),
                None
            )
            .await
            .is_err()
    );
    drop(engine);
    let engine = open();
    let receipt = engine
        .admit(
            "person",
            "work",
            "migration",
            grant("retry-must-not-replace"),
            None,
        )
        .await
        .unwrap();
    let access = engine
        .acquire("person", &receipt.account_id, None)
        .await
        .unwrap();
    assert_eq!(access.access_token, "original-access");
    assert_eq!(count.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn successor_verification_recovers_after_restart_without_another_refresh() {
    let profiles = Arc::new(AtomicUsize::new(0));
    let refreshes = Arc::new(AtomicUsize::new(0));
    let calls = profiles.clone();
    let exchanges = refreshes.clone();
    let app = Router::new()
        .route("/api/oauth/profile", get(move || {let calls=calls.clone(); async move {
            use axum::response::IntoResponse;
            if calls.fetch_add(1,Ordering::SeqCst)==1 {axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()}
            else {Json(json!({"account":{"uuid":"a"},"organization":{"uuid":"o"}})).into_response()}
        }}))
        .route("/token", post(move || {let exchanges=exchanges.clone(); async move {
            exchanges.fetch_add(1,Ordering::SeqCst);
            Json(json!({"access_token":"successor","refresh_token":"successor-refresh","expires_in":3600,"scope":"user:inference user:profile"}))
        }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("key");
    vault::create_secret(&key, &[12; 32]).unwrap();
    let open = || {
        Engine::open_at(
            &root.path().join("store"),
            &key,
            false,
            Endpoints {
                api: origin.clone(),
                token: format!("{origin}/token"),
            },
        )
        .unwrap()
    };
    let engine = open();
    let receipt = engine
        .admit(
            "person",
            "work",
            "migration",
            Grant {
                access_token: "initial".into(),
                refresh_token: "initial-refresh".into(),
                expires_at: now() + 3600000,
                scopes: vec!["user:inference".into(), "user:profile".into()],
            },
            None,
        )
        .await
        .unwrap();
    let current = engine
        .acquire("person", &receipt.account_id, None)
        .await
        .unwrap();
    assert!(
        engine
            .acquire("person", &receipt.account_id, Some(&current.revision))
            .await
            .is_err()
    );
    drop(engine);
    let engine = open();
    let successor = engine
        .acquire("person", &receipt.account_id, None)
        .await
        .unwrap();
    assert_eq!(successor.access_token, "successor");
    assert_eq!(successor.generation, 2);
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn login_retries_retained_response_without_reusing_authorization_code() {
    use axum::response::IntoResponse;
    let exchanges = Arc::new(AtomicUsize::new(0));
    let profiles = Arc::new(AtomicUsize::new(0));
    let tokens = exchanges.clone();
    let ids = profiles.clone();
    let app=Router::new().route("/token",post(move || {let tokens=tokens.clone();async move {
        tokens.fetch_add(1,Ordering::SeqCst);
        Json(json!({"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600,"scope":"user:inference user:profile"}))
    }})).route("/api/oauth/profile",get(move || {let ids=ids.clone();async move {
        if ids.fetch_add(1,Ordering::SeqCst)==0 {axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()}
        else {Json(json!({"account":{"uuid":"a"},"organization":{"uuid":"o"}})).into_response()}
    }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("key");
    vault::create_secret(&key, &[13; 32]).unwrap();
    let open = || {
        Engine::open_at(
            &root.path().join("store"),
            &key,
            false,
            Endpoints {
                api: origin.clone(),
                token: format!("{origin}/token"),
            },
        )
        .unwrap()
    };
    let engine = open();
    let challenge = engine
        .start_login("person", "machine", "work", false)
        .await
        .unwrap();
    let url = reqwest::Url::parse(&challenge.authorize_url).unwrap();
    let state = url
        .query_pairs()
        .find(|(name, _)| name == "state")
        .unwrap()
        .1
        .into_owned();
    let code = format!("fake-code#{state}");
    assert!(
        engine
            .finish_login("person", "other-machine", &challenge.id, &code)
            .await
            .is_err()
    );
    assert!(
        engine
            .finish_login("person", "machine", &challenge.id, "fake-code#wrong")
            .await
            .is_err()
    );
    assert_eq!(exchanges.load(Ordering::SeqCst), 0);
    assert!(
        engine
            .finish_login("person", "machine", &challenge.id, &code)
            .await
            .is_err()
    );
    drop(engine);
    let engine = open();
    let receipt = engine
        .finish_login("person", "machine", &challenge.id, &code)
        .await
        .unwrap();
    assert_eq!(
        engine
            .acquire("person", &receipt.account_id, None)
            .await
            .unwrap()
            .access_token,
        "new-access"
    );
    assert_eq!(exchanges.load(Ordering::SeqCst), 1);
    task.abort();
}
