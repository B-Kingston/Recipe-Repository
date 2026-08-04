use crate::{codex_credential, import_legacy_codex_auth, store_codex_credential};
use serde_json::json;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::fs;

async fn database() -> SqlitePool {
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .in_memory(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    db
}

fn credential(access: &str, account_id: &str) -> serde_json::Value {
    json!({
        "type": "oauth",
        "access": access,
        "refresh": "refresh_1",
        "expires": 4_102_444_800_000_i64,
        "accountId": account_id,
    })
}

#[tokio::test]
async fn credential_round_trips_through_the_database() {
    let db = database().await;
    assert!(codex_credential(&db).await.unwrap().is_none());

    store_codex_credential(&db, &credential("access_1", "acct_1"))
        .await
        .unwrap();
    let stored = codex_credential(&db).await.unwrap().unwrap();
    assert_eq!(stored["type"], "oauth");
    assert_eq!(stored["accountId"], "acct_1");

    // A refresh replaces the stored access token rather than appending.
    store_codex_credential(&db, &credential("access_2", "acct_1"))
        .await
        .unwrap();
    assert_eq!(
        codex_credential(&db).await.unwrap().unwrap()["access"],
        "access_2"
    );
}

#[tokio::test]
async fn legacy_auth_json_is_imported_once_and_never_overrides_the_database() {
    let agent_dir =
        std::env::temp_dir().join(format!("kindle-recipes-auth-test-{}", std::process::id()));
    fs::create_dir_all(&agent_dir).unwrap();
    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", &agent_dir);
    }
    let db = database().await;

    // No file yet: nothing to import.
    import_legacy_codex_auth(&db).await.unwrap();
    assert!(codex_credential(&db).await.unwrap().is_none());

    // A legacy Pi CLI credential is imported once.
    fs::write(
        agent_dir.join("auth.json"),
        r#"{"openai-codex":{"type":"oauth","access":"a","refresh":"r","expires":1,"accountId":"x"}}"#,
    )
    .unwrap();
    import_legacy_codex_auth(&db).await.unwrap();
    assert_eq!(
        codex_credential(&db).await.unwrap().unwrap()["accountId"],
        "x"
    );

    // Later changes to the file never overwrite the database.
    fs::write(
        agent_dir.join("auth.json"),
        r#"{"openai-codex":{"type":"oauth","access":"b","refresh":"r","expires":1,"accountId":"y"}}"#,
    )
    .unwrap();
    import_legacy_codex_auth(&db).await.unwrap();
    assert_eq!(
        codex_credential(&db).await.unwrap().unwrap()["accountId"],
        "x"
    );

    unsafe {
        std::env::remove_var("PI_CODING_AGENT_DIR");
    }
    fs::remove_dir_all(&agent_dir).ok();
}
