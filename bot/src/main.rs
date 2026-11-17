//! A minimal Telegram bot: in allowed chats it replies to messages with their links,
//! domains swapped to services that make good previews.

mod access;
mod app;
mod config;
mod limits;
mod links;
mod menu;
mod telegram;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use crate::access::Access;
use crate::app::{Action, App};
use crate::config::Config;
use crate::telegram::{POLL_SECONDS, Telegram, describe, is_bad_token};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE: &str = "usage: bot [CONFIG]\n\nCONFIG defaults to bot.toml. The token comes from TELEGRAM_BOT_TOKEN.";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_target(false)
        .init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let arg = std::env::args_os().nth(1);
    if arg.as_ref().is_some_and(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let path = arg.map_or_else(|| PathBuf::from("bot.toml"), PathBuf::from);
    let config = Config::load(&path)?;
    let token = std::env::var("TELEGRAM_BOT_TOKEN").context("TELEGRAM_BOT_TOKEN isn't set")?;
    let access = Access::load(&config)?;
    let telegram = Arc::new(Telegram::new(token.trim())?);

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let me = tokio::select! {
        me = get_me(&telegram) => me?,
        () = &mut shutdown => return Ok(()),
    };
    telegram.setup(config.owner_id).await;
    info!(bot = %me, domains = config.domains.len(), "started");
    let mut app = App::new(&config, access, me);

    let mut tasks = JoinSet::new();
    // Groups whose prompt to the owner didn't go out, from the tasks sending them.
    let (unasked_tx, mut unasked) = tokio::sync::mpsc::unbounded_channel();
    let mut offset = None;
    let mut backoff = Duration::from_secs(1);
    loop {
        let batch = tokio::select! {
            batch = telegram.updates(offset, POLL_SECONDS) => batch,
            () = &mut shutdown => break,
        };
        while tasks.try_join_next().is_some() {}
        while let Ok(group) = unasked.try_recv() {
            app.prompt_failed(group);
        }
        let batch = match batch {
            Ok(batch) => {
                backoff = Duration::from_secs(1);
                batch
            }
            // Revoked while running: retrying would never work.
            Err(e) if is_bad_token(&e) => {
                bail!(
                    "Telegram rejected TELEGRAM_BOT_TOKEN, so it was revoked or changed; set the new one and restart the bot: {}",
                    describe(&e)
                )
            }
            Err(e) => {
                warn!("polling failed, retrying in {}s: {}", backoff.as_secs(), describe(&e));
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    () = &mut shutdown => break,
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        for (id, update) in batch {
            offset = Some(id + 1);
            let Some(update) = update else { continue };
            let actions = app.handle(update.content, Instant::now());
            if actions.is_empty() {
                continue;
            }
            // Sending can be slow (Telegram may fetch the preview first), so it runs on the side;
            // one update's actions stay in order.
            let (telegram, unasked) = (Arc::clone(&telegram), unasked_tx.clone());
            tasks.spawn(async move {
                for action in &actions {
                    if !telegram.run(action).await
                        && let Action::Ask(group, _) = action
                    {
                        // The loop is gone only at shutdown, when this no longer matters.
                        let _ = unasked.send(*group);
                    }
                }
            });
        }
    }

    info!("shutting down");
    // Replies in flight first, then the confirmation, all within the 10 seconds Docker waits
    // before it kills the container.
    let drained = tokio::time::timeout(Duration::from_secs(7), async { while tasks.join_next().await.is_some() {} });
    if drained.await.is_err() {
        warn!("gave up waiting for {} replies in flight", tasks.len());
    }
    // Confirm handled updates so they aren't delivered again after a restart.
    if offset.is_some() {
        match tokio::time::timeout(Duration::from_secs(2), telegram.updates(offset, 0)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!("couldn't confirm the last updates: {}", describe(&e)),
            Err(_) => warn!("couldn't confirm the last updates in time"),
        }
    }
    Ok(())
}

/// Asks Telegram for the bot's @name, retrying network errors; a wrong token stops the bot.
async fn get_me(telegram: &Telegram) -> Result<String> {
    let mut backoff = Duration::from_secs(1);
    loop {
        match telegram.get_me().await {
            Ok(user) => return Ok(user.username.unwrap_or_default()),
            Err(e) if is_bad_token(&e) => bail!("Telegram rejected TELEGRAM_BOT_TOKEN: {}", describe(&e)),
            Err(e) => {
                warn!("can't reach Telegram, retrying in {}s: {}", backoff.as_secs(), describe(&e));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
