//! The server's command line.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use server::app::App;
use server::config::Config;
use server::cookies::CookieStore;
use server::dashboard::{self, auth};
use server::db::Db;
use server::extract::facebook::url::Target;
use server::extract::facebook::{Facebook, build_post, page};
use server::extract::{CookieChoice, Extractor, Media, Timings};
use server::render::{self, MediaUrl};
use server::settings::LiveSettings;
use server::{public, tasks};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Telegram link previews and Instant View for Facebook posts.
///
/// With no command, runs the server. Settings come from the environment
/// (PUBLIC_ADDR, DASHBOARD_ADDR, DATA_DIR, CLIENT_IP_FROM, TRUSTED_PROXIES,
/// DASHBOARD_URL, ACCESS_TEAM, ACCESS_AUD, TELEGRAM_BOT_TOKEN) and the dashboard.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Extract one post and print the result, without starting the server
    Extract {
        /// A facebook.com link (with --page, the page's own link is used when this is left out)
        #[arg(required_unless_present = "page")]
        url: Option<String>,
        /// Use the saved cookies, whatever the cookie policy says
        #[arg(long)]
        cookies: bool,
        /// Read a page saved by `record` instead of fetching (no network, so no album expansion)
        #[arg(long, conflicts_with = "cookies")]
        page: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
    /// Save a cleaned-up copy of a Facebook page as a test fixture
    Record {
        /// A facebook.com link
        url: String,
        /// Fetch with the saved cookies. The account's ID is replaced, but its name and other
        /// details stay, so never commit such a copy
        #[arg(long)]
        cookies: bool,
        /// Where to write it (default: server/tests/fixtures/<page type>-<hash>.html)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Print a one-time link that signs in to the dashboard (valid 30 minutes)
    SetupLink,
    /// Remove the password, authenticator app, passkeys and sessions, then print a setup link
    ResetLogin,
    /// Exit 0 if the running server answers (for the container health check)
    Healthcheck,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    /// The extracted post and step timings
    Json,
    /// The preview tags Telegram reads
    Preview,
    /// The whole page, Instant View article included
    Iv,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            error!("can't start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        match cli.command {
            None => serve().await,
            Some(Command::Extract { url, cookies, page, format }) => {
                extract_command(url.as_deref(), cookies, page, format).await
            }
            Some(Command::Record { url, cookies, out }) => record_command(&url, cookies, out).await,
            Some(Command::SetupLink) => setup_link_command(false).await,
            Some(Command::ResetLogin) => setup_link_command(true).await,
            Some(Command::Healthcheck) => healthcheck().await,
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn serve() -> Result<()> {
    let config = Config::from_env()?;
    let shutdown = CancellationToken::new();
    let db = Db::open(&config.data_dir).await?;
    let cookies = Arc::new(CookieStore::load(db.clone()).await);
    let extractor = Arc::new(Facebook::new(Arc::clone(&cookies))?);
    let app = App::new(config, db, cookies, extractor).await?;
    if !auth::has_login(&app.db).await? {
        let link = auth::create_setup_link(&app.db, &app.config).await?;
        info!("no dashboard password or passkey yet; within 30 minutes, open this once to set one up: {link}");
    }
    let dashboard = dashboard::router(Arc::clone(&app), shutdown.clone())?;

    let public = tokio::net::TcpListener::bind(app.config.public_addr)
        .await
        .with_context(|| format!("can't listen on PUBLIC_ADDR {}", app.config.public_addr))?;
    let admin = tokio::net::TcpListener::bind(app.config.dashboard_addr)
        .await
        .with_context(|| format!("can't listen on DASHBOARD_ADDR {}", app.config.dashboard_addr))?;
    info!(
        public = %app.config.public_addr,
        admin = %app.config.dashboard_addr,
        client_ip_from = app.config.client_ip_from.as_str(),
        "listening"
    );

    tasks::spawn(&app, &shutdown);
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            shutdown_signal().await;
            info!("shutting down");
            shutdown.cancel();
        }
    });

    let public =
        axum::serve(public, public::router(Arc::clone(&app)).into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let admin = axum::serve(admin, dashboard.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let served = tokio::try_join!(public.into_future(), admin.into_future());
    if let Err(e) = app.stats.flush().await {
        error!("can't save the last statistics: {e:#}");
    }
    served.context("a listener failed")?;
    Ok(())
}

/// Opens the data directory's settings and cookies for a one-off command.
async fn offline_facebook() -> Result<(Facebook, Arc<server::settings::Settings>)> {
    let config = Config::from_env()?;
    let db = Db::open(&config.data_dir).await?;
    let settings = LiveSettings::load(db.clone()).await?.get();
    let cookies = Arc::new(CookieStore::load(db).await);
    Ok((Facebook::new(cookies)?, settings))
}

async fn extract_command(url: Option<&str>, cookies: bool, saved: Option<PathBuf>, format: Format) -> Result<()> {
    let (target, post, timings, settings) = match saved {
        Some(path) => {
            let html = std::fs::read(&path).with_context(|| format!("can't read {}", path.display()))?;
            let saved_url = std::str::from_utf8(&html[..html.len().min(2000)])
                .ok()
                .and_then(|head| head.lines().next()?.strip_prefix("<!-- url: ")?.strip_suffix(" -->"))
                .map(str::to_string);
            let url =
                url.map(str::to_string).or(saved_url).context("the page has no `<!-- url: -->` line; give the link")?;
            let target = Target::from_link(&url).context("that isn't a facebook.com link")?;
            let started = std::time::Instant::now();
            let page = page::parse(&html)?;
            let parse = started.elapsed();
            let (post, _) = build_post(&page, &target, target.key())?;
            let timings = Timings { parse, extract: started.elapsed() - parse, ..Timings::default() };
            (target, post, timings, Arc::new(server::settings::Settings::default()))
        }
        None => {
            let url = url.context("give a facebook.com link")?;
            let target = Target::from_link(url).context("that isn't a facebook.com link")?;
            let (facebook, settings) = offline_facebook().await?;
            let choice = if cookies { CookieChoice::Force } else { CookieChoice::Policy };
            let (post, timings) = facebook.extract(&target, &settings, choice).await?;
            (target, post, timings, settings)
        }
    };
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    eprintln!(
        "{} as {}: fetch {} ms, parse {} ms, extract {} ms, {} requests",
        target.key(),
        target.page_type.as_str(),
        ms(timings.fetch),
        ms(timings.parse),
        ms(timings.extract),
        timings.requests
    );
    let media: Vec<MediaUrl> =
        post.media.iter().map(|m: &Media| MediaUrl { src: m.url.clone(), poster: m.thumbnail.clone() }).collect();
    let html = render::preview(&post, &media, &settings, target.original.as_str());
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&post)?),
        Format::Preview => println!("{}", html.split("<body>").next().unwrap_or_default().trim_end()),
        Format::Iv => println!("{html}"),
    }
    Ok(())
}

async fn record_command(url: &str, cookies: bool, out: Option<PathBuf>) -> Result<()> {
    let target = Target::from_link(url).context("that isn't a facebook.com link")?;
    let (facebook, settings) = offline_facebook().await?;
    let html = facebook.record(&target, &settings, cookies).await?;
    let out = out.unwrap_or_else(|| {
        use sha2::Digest;
        let hash = sha2::Sha256::digest(target.key().as_bytes());
        let hash: String = hash[..4].iter().map(|b| format!("{b:02x}")).collect();
        PathBuf::from(format!("server/tests/fixtures/{}-{hash}.html", target.page_type.as_str()))
    });
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&out, html).with_context(|| format!("can't write {}", out.display()))?;
    if cookies {
        eprintln!("saved {}; it holds your account's name and more: keep it out of git", out.display());
    } else {
        eprintln!("saved {} (comments and the viewer block removed); review it before committing", out.display());
    }
    Ok(())
}

/// Prints a setup link; with `reset`, first removes every way to sign in.
async fn setup_link_command(reset: bool) -> Result<()> {
    let config = Config::from_env()?;
    let db = Db::open(&config.data_dir).await?;
    if reset {
        auth::reset_login(&db).await?;
        eprintln!("removed the password, the authenticator app, every passkey and every session");
    }
    let link = auth::create_setup_link(&db, &config).await?;
    if config.dashboard_url.is_none() {
        eprintln!("DASHBOARD_URL isn't set, so this is the path to open on the dashboard:");
    }
    println!("{link}");
    eprintln!("it works once, within 30 minutes");
    Ok(())
}

/// Asks the running server's public listener for `/_/health`. The image has no shell or curl.
async fn healthcheck() -> Result<()> {
    let config = Config::from_env()?;
    let mut addr = config.public_addr;
    if addr.ip().is_unspecified() {
        addr.set_ip(match addr.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        });
    }
    let check = async {
        let mut stream = tokio::net::TcpStream::connect(addr).await?;
        stream.write_all(b"GET /_/health HTTP/1.0\r\nHost: localhost\r\n\r\n").await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        anyhow::Ok(response)
    };
    let response = tokio::time::timeout(Duration::from_secs(3), check).await.context("no answer within 3 s")??;
    if !(response.starts_with(b"HTTP/1.1 200") || response.starts_with(b"HTTP/1.0 200")) {
        bail!("unhealthy: {}", String::from_utf8_lossy(&response[..response.len().min(64)]));
    }
    Ok(())
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
