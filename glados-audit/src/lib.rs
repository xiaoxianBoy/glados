use std::path::PathBuf;

use anyhow::Result;
use ethereum_types::H256;
use ethportal_api::types::content_key::{BlockHeaderKey, HistoryContentKey, OverlayContentKey};
use sea_orm::{DatabaseConnection, EntityTrait, QueryOrder, QuerySelect};
use tokio::{
    sync::mpsc,
    time::{interval, Duration},
};
use tracing::{debug, error, info};

use entity::{contentaudit, contentkey};
use glados_core::jsonrpc::PortalClient;

pub mod cli;

const AUDIT_PERIOD_SECONDS: u64 = 120;

pub async fn run_glados_audit(conn: DatabaseConnection, ipc_path: PathBuf) {
    let (tx, rx) = mpsc::channel(100);

    tokio::spawn(do_audit_orchestration(tx, conn.clone()));
    tokio::spawn(perform_content_audits(rx, ipc_path, conn));

    debug!("setting up CTRL+C listener");
    tokio::signal::ctrl_c()
        .await
        .expect("failed to pause until ctrl-c");

    info!("got CTRL+C. shutting down...");
}

async fn do_audit_orchestration(tx: mpsc::Sender<HistoryContentKey>, conn: DatabaseConnection) -> !
where
    Vec<u8>: From<HistoryContentKey>,
{
    debug!("initializing audit process");

    let mut interval = interval(Duration::from_secs(AUDIT_PERIOD_SECONDS));
    loop {
        interval.tick().await;

        // Lookup a content key to be audited
        let content_key_db_entries = match contentkey::Entity::find()
            .order_by_desc(contentkey::Column::CreatedAt)
            .limit(10)
            .all(&conn)
            .await
        {
            Ok(content_key_db_entries) => content_key_db_entries,
            Err(err) => {
                error!("DB Error looking up content key: {err}");
                continue;
            }
        };
        debug!(
            "Adding {} content keys to the audit queue.",
            content_key_db_entries.len()
        );
        for content_key_db in content_key_db_entries {
            info!("Content Key: {:?}", content_key_db.content_key);
            // Get the block hash (by removing the first byte from the content key)
            let hash = H256::from_slice(&content_key_db.content_key[1..33]);
            let content_key = HistoryContentKey::BlockHeader(BlockHeaderKey {
                block_hash: hash.to_fixed_bytes(),
            });

            // Send it to the audit process
            tx.send(content_key)
                .await
                .expect("Channel closed, perform_content_audits task likely crashed");
        }
    }
}

async fn perform_content_audits(
    mut rx: mpsc::Receiver<HistoryContentKey>,
    ipc_path: PathBuf,
    conn: DatabaseConnection,
) -> Result<()>
where
    Vec<u8>: From<HistoryContentKey>,
{
    let mut client = PortalClient::from_ipc(&ipc_path)?;

    while let Some(content_key) = rx.recv().await {
        debug!(
            content.key=?content_key,
            content.id=?content_key.content_id(),
            "auditing content",
        );
        let content = client.get_content(&content_key)?;

        let raw_data = content.raw;

        let Ok(Some(content_key_id)) = contentkey::get(&content_key, &conn).await else {
            debug!(
                content.key=?content_key,
                content.id=?content_key.content_id(),
                "no content found",
            );
            continue
        };
        contentaudit::create(content_key_id.id, raw_data.len() > 2, &conn).await;

        info!("Successfully audited content.");
    }
    Ok(())
}
