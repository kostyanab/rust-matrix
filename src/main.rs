use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use matrix_sdk::{
    Client as MatrixClient, Room, RoomState,
    config::SyncSettings,
    ruma::events::room::message::{
        MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
    },
};

#[derive(Parser, Debug)]
#[command(name = "alertbot", version, about = "Simple matrix alert bot")]
struct Cli {
    #[arg(long, env = "MATRIX_SERVER_URL")]
    server_url: String,

    #[arg(long, env = "MATRIX_USERNAME")]
    username: String,

    #[arg(long, env = "MATRIX_PASSWORD")]
    password: String,
}

async fn on_room_message(event: OriginalSyncRoomMessageEvent, room: Room) {
    if room.state() != RoomState::Joined {
        return;
    }
    let MessageType::Text(text_content) = event.content.msgtype else {
        return;
    };

    if text_content.body.contains("!temp") {
        let content = RoomMessageEventContent::text_plain("Temp in street: 25");
        println!("sending");
        room.send(content).await.unwrap();
        println!("message sent");
    }
}

async fn login_and_sync(cli: Cli) -> Result<()> {
    let client = MatrixClient::builder()
        .homeserver_url(&cli.server_url)
        .build()
        .await
        .context("Failed to build Matrix client")?;

    client
        .matrix_auth()
        .login_username(&cli.username, &cli.password)
        .initial_device_display_name("alertbot")
        .await
        .context("Login failed")?;

    info!("logged in as {}", cli.username);

    // Первый sync
    let response = client
        .sync_once(SyncSettings::default())
        .await
        .context("Initial sync failed")?;

    // Обработчик сообщений
    client.add_event_handler(on_room_message);

    // Основной цикл
    let settings = SyncSettings::default().token(response.next_batch);
    client.sync(settings).await.context("Sync loop failed")?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();
    info!("Starting bot ...");

    if let Err(err) = login_and_sync(cli).await {
        error!("{:#}", err);
    }

    Ok(())
}
