use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use matrix_sdk::{
    Client, Error, LoopCtrl, Room, RoomState,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    ruma::{
        api::client::filter::FilterDefinition,
        events::room::message::{MessageType, OriginalSyncRoomMessageEvent},
    },
};

use rand::{Rng, distr::Alphanumeric, rng};
use serde::{Deserialize, Serialize};
use tokio::fs;

#[derive(Parser, Debug)]
#[command(name = "alertbot", version, about = "Matrix alert bot")]
struct Cli {
    #[arg(long, env = "MATRIX_SERVER_URL")]
    server_url: String,

    #[arg(long, env = "MATRIX_USERNAME")]
    username: String,

    #[arg(long, env = "MATRIX_PASSWORD", hide_env_values = true)]
    password: String,

    /// Recovery Key или Passphrase для Secure Backup (CLI имеет приоритет над ENV).
    #[arg(long, env = "MATRIX_RECOVERY_SECRET", hide_env_values = true)]
    recovery_secret: Option<String>,
}

/// The data needed to re-build a client.
#[derive(Debug, Serialize, Deserialize)]
struct ClientSession {
    /// The URL of the homeserver of the user.
    homeserver: String,

    /// The path of the database.
    db_path: PathBuf,

    /// The passphrase of the database.
    passphrase: String,
}

/// The full session to persist.
#[derive(Debug, Serialize, Deserialize)]
struct FullSession {
    /// The data to re-build the client.
    client_session: ClientSession,

    /// The Matrix user session.
    user_session: MatrixSession,

    /// The latest sync token.
    ///
    /// It is only needed to persist it when using `Client::sync_once()` and we
    /// want to make our syncs faster by not receiving all the initial sync
    /// again.
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("matrix_sdk=warn".parse().unwrap())
                .add_directive("matrix_sdk_crypto=warn".parse().unwrap()),
        )
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();
    info!("Starting bot ...");

    // The folder containing this example's data.
    let data_dir = dirs::data_dir()
        .expect("no data_dir directory found")
        .join("persist_session");
    // Ensure directory exists early.
    tokio::fs::create_dir_all(&data_dir).await?;

    // The file where the session is persisted.
    let session_file = data_dir.join("session");
    info!("Session file: {}", session_file.display());

    let (client, sync_token) = if session_file.exists() {
        restore_session(&session_file, cli.recovery_secret.as_deref()).await?
    } else {
        (
            login(
                &data_dir,
                &session_file,
                &cli.username,
                &cli.password,
                &cli.server_url,
                cli.recovery_secret.as_deref(),
            )
            .await?,
            None,
        )
    };

    sync(client, sync_token, &session_file).await
}

/// Restore a previous session.
async fn restore_session(
    session_file: &Path,
    recovery_secret: Option<&str>,
) -> Result<(Client, Option<String>)> {
    info!(
        "Previous session found in '{}'",
        session_file.to_string_lossy()
    );

    // The session was serialized as JSON in a file.
    let serialized_session = fs::read_to_string(session_file).await?;
    let FullSession {
        client_session,
        user_session,
        sync_token,
    } = serde_json::from_str(&serialized_session)?;

    // Build the client with the previous settings from the session.
    let client = Client::builder()
        .homeserver_url(client_session.homeserver)
        .sqlite_store(client_session.db_path, Some(&client_session.passphrase))
        .build()
        .await?;

    info!("Restoring session for {}…", user_session.meta.user_id);

    // Restore the Matrix user session.
    client.restore_session(user_session).await?;

    // Try to recover Secure Backup using provided secret (if any).
    if let Some(secret) = recovery_secret {
        match client.encryption().recovery().recover(secret).await {
            Ok(()) => {
                info!("Secure Backup successfully recovered (restore_session)");
                if let Err(e) = client.encryption().backups().wait_for_steady_state().await {
                    error!("Backup steady-state wait failed: {e}");
                }
            }
            Err(err) => error!("Secure Backup recovery failed: {err}"),
        }
    }

    Ok((client, sync_token))
}

/// Login with a new device.
async fn login(
    data_dir: &Path,
    session_file: &Path,
    username: &str,
    password: &str,
    client_url: &str,
    recovery_secret: Option<&str>,
) -> Result<Client> {
    info!("No previous session found, logging in…");

    let (client, client_session) = build_client(data_dir, client_url).await?;
    let matrix_auth = client.matrix_auth();

    matrix_auth
        .login_username(username, password)
        .initial_device_display_name("AlertBot")
        .await?;

    // Recover Secure Backup after login if secret provided.
    if let Some(secret) = recovery_secret {
        match client.encryption().recovery().recover(secret).await {
            Ok(()) => {
                info!("Secure Backup successfully recovered using recovery secret");
                if let Err(e) = client.encryption().backups().wait_for_steady_state().await {
                    error!("Backup steady-state wait failed: {e}");
                }
            }
            Err(err) => error!("Failed to recover Secure Backup: {err}"),
        }
    } else {
        info!("No recovery secret provided, skipping Secure Backup recovery");
    }

    // Persist the session to reuse it later.
    let user_session = matrix_auth
        .session()
        .expect("A logged-in client should have a session");
    let serialized_session = serde_json::to_string(&FullSession {
        client_session,
        user_session,
        sync_token: None,
    })?;
    tokio::fs::create_dir_all(data_dir).await?;
    fs::write(session_file, serialized_session).await?;

    info!("Session persisted in {}", session_file.to_string_lossy());

    Ok(client)
}

/// Build a new client.
async fn build_client(data_dir: &Path, homeserver: &str) -> Result<(Client, ClientSession)> {
    // Ensure parent directory exists.
    tokio::fs::create_dir_all(data_dir).await?;

    let mut rng = rng();

    // Each client gets its own SQLite folder.
    let db_subfolder: String = (&mut rng)
        .sample_iter(Alphanumeric)
        .take(7)
        .map(char::from)
        .collect();
    let db_path = data_dir.join(db_subfolder);

    // Generate a random passphrase.
    let passphrase: String = (&mut rng)
        .sample_iter(Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();

    let client = Client::builder()
        .homeserver_url(homeserver)
        .sqlite_store(&db_path, Some(&passphrase))
        .build()
        .await?;

    Ok((
        client,
        ClientSession {
            homeserver: homeserver.to_owned(),
            db_path,
            passphrase,
        },
    ))
}

/// Setup the client to listen to new messages.
async fn sync(
    client: Client,
    initial_sync_token: Option<String>,
    session_file: &Path,
) -> Result<()> {
    info!("Launching a first sync to ignore past messages…");

    // Enable room members lazy-loading, it will speed up the initial sync a lot
    // with accounts in lots of rooms.
    // See <https://spec.matrix.org/v1.6/client-server-api/#lazy-loading-room-members>.
    let filter = FilterDefinition::with_lazy_loading();

    let mut sync_settings = SyncSettings::default().filter(filter.into());

    // We restore the sync where we left.
    // This is not necessary when not using `sync_once`. The other sync methods get
    // the sync token from the store.
    if let Some(sync_token) = initial_sync_token {
        sync_settings = sync_settings.token(sync_token);
    }

    // Let's ignore messages before the program was launched.
    loop {
        match client.sync_once(sync_settings.clone()).await {
            Ok(response) => {
                // Hand over next_batch to the following syncs and persist it.
                sync_settings = sync_settings.token(response.next_batch.clone());
                persist_sync_token(session_file, response.next_batch).await?;
                break;
            }
            Err(error) => {
                error!("An error occurred during initial sync: {error}");
                error!("Trying again…");
            }
        }
    }

    info!("The client is ready! Listening to new messages…");

    // Now that we've synced, let's attach a handler for incoming room messages.
    client.add_event_handler(on_room_message);

    // This loops until we kill the program or an error happens.
    client
        .sync_with_result_callback(sync_settings, |sync_result| async move {
            let response = sync_result?;

            // Persist the token each time to be able to restore our session
            persist_sync_token(session_file, response.next_batch)
                .await
                .map_err(|err| Error::UnknownError(err.into()))?;

            Ok(LoopCtrl::Continue)
        })
        .await?;

    Ok(())
}

/// Persist the sync token for a future session.
/// Note that this is needed only when using `sync_once`. Other sync methods get
/// the sync token from the store.
async fn persist_sync_token(session_file: &Path, sync_token: String) -> Result<()> {
    let serialized_session = fs::read_to_string(session_file).await?;
    let mut full_session: FullSession = serde_json::from_str(&serialized_session)?;

    full_session.sync_token = Some(sync_token);
    let serialized_session = serde_json::to_string(&full_session)?;
    fs::write(session_file, serialized_session).await?;

    Ok(())
}

/// Handle room messages.
async fn on_room_message(event: OriginalSyncRoomMessageEvent, room: Room) {
    // We only want to log text messages in joined rooms.
    if room.state() != RoomState::Joined {
        return;
    }
    let MessageType::Text(text_content) = &event.content.msgtype else {
        return;
    };

    let room_name = match room.display_name().await {
        Ok(room_name) => room_name.to_string(),
        Err(error) => {
            error!("Error getting room display name: {error}");
            // Let's fallback to the room ID.
            room.room_id().to_string()
        }
    };

    info!("[{room_name}] {}: {}", event.sender, text_content.body)
}
