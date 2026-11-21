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
use server::cookies::Cookies;
use server::dashboard::{self, auth};
use server::db::Db;
use server::extract::facebook::{Facebook, build_post};
use server::extract::instagram::Instagram;
use server::extract::reddit::Reddit;
use server::extract::threads::Threads;
use server::extract::{CookieChoice, Extractor, Sites, Source, Target, Timings, page};
use server::render::{self, MediaUrl};
use server::settings::LiveSettings;
use server::{public, tasks};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Homura: Telegram link previews and Instant View for social posts (Facebook, Instagram, Reddit and
/// Threads).
///
/// With no command, runs the server. Settings come from the environment (see
/// docs/configuration.md) and the dashboard.
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
        /// A link to a site Homura reads (with --page, the saved one is used when this is left out)
        #[arg(required_unless_present = "page")]
        url: Option<String>,
        /// Use the saved cookies, whatever the cookie policy says
        #[arg(long)]
        cookies: bool,
        /// Read what `record` saved instead of fetching (no network, so no album expansion)
        #[arg(long, conflicts_with = "cookies")]
        page: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
    /// Save a cleaned-up copy of what extraction reads (a Facebook page, another site's answers) as a
    /// test fixture
    Record {
        /// A link to a site Homura reads
        url: String,
        /// Fetch with the saved cookies. The account's ID is replaced, but its name and other
        /// details stay, so never commit such a copy
        #[arg(long)]
        cookies: bool,
        /// Where to write it (default: server/tests/fixtures/<site>-<page type>-<hash>.<html or json>)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Print a one-time link to the dashboard's Security page (valid 30 minutes)
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
    let cookies = Cookies::load(&db).await;
    let extractor = Arc::new(sites(&cookies)?);
    let app = App::new(config, db, cookies, extractor).await?;
    // With Cloudflare Access on, Access signs in, and a link in the log would be one more way.
    if app.config.access.is_none() && !auth::has_login(&app.db).await? {
        let link = auth::create_setup_link(&app.db, &app.config).await?;
        info!(
            "no way to sign in to the dashboard yet (a password or a passkey); \
             within 30 minutes, open this once to set one up: {link}"
        );
    }
    let dashboard = dashboard::router(Arc::clone(&app), shutdown.clone())?;

    let public = tokio::net::TcpListener::bind(app.config.public_addr)
        .await
        .with_context(|| format!("can't listen on PUBLIC_ADDR {}", app.config.public_addr))?;
    let dashboard_listener = tokio::net::TcpListener::bind(app.config.dashboard_addr)
        .await
        .with_context(|| format!("can't listen on DASHBOARD_ADDR {}", app.config.dashboard_addr))?;
    info!(
        public = %app.config.public_addr,
        dashboard = %app.config.dashboard_addr,
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
    let dashboard = axum::serve(dashboard_listener, dashboard.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let served = tokio::try_join!(public.into_future(), dashboard.into_future());
    if let Err(e) = app.stats.flush().await {
        error!("can't save the last statistics: {e:#}");
    }
    served.context("a listener failed")?;
    Ok(())
}

fn sites(cookies: &Cookies) -> Result<Sites> {
    Ok(Sites {
        facebook: Facebook::new(Arc::clone(cookies.of(Source::Facebook)))?,
        instagram: Instagram::new(Arc::clone(cookies.of(Source::Instagram)))?,
        reddit: Reddit::new(Arc::clone(cookies.of(Source::Reddit)))?,
        threads: Threads::new(Arc::clone(cookies.of(Source::Threads)))?,
    })
}

/// Opens the data directory's settings and cookies for a one-off command.
async fn offline() -> Result<(Sites, Arc<server::settings::Settings>)> {
    let config = Config::from_env()?;
    let db = Db::open(&config.data_dir).await?;
    let settings = LiveSettings::load(db.clone()).await?.get();
    Ok((sites(&Cookies::load(&db).await)?, settings))
}

const NOT_A_LINK: &str = "that isn't a link to a site Homura reads (Facebook, Instagram, Reddit or Threads)";

/// Reads a fixture `record` saved: a Facebook page, or another site's answers as JSON.
fn read_saved(path: &std::path::Path, url: Option<&str>) -> Result<(Target, server::extract::Post, Timings)> {
    let bytes = std::fs::read(path).with_context(|| format!("can't read {}", path.display()))?;
    let started = std::time::Instant::now();
    if path.extension().is_some_and(|e| e == "json") {
        let saved: serde_json::Value = serde_json::from_slice(&bytes)?;
        let saved_url = saved.get("url").and_then(serde_json::Value::as_str).map(str::to_string);
        let url = url.map(str::to_string).or(saved_url).context("the file has no \"url\"; give the link")?;
        let target = Target::from_link(&url).context(NOT_A_LINK)?;
        let post = server::extract::read_fixture(&saved, &target)?;
        return Ok((target, post, Timings { extract: started.elapsed(), ..Timings::default() }));
    }
    let saved_url = page::saved_url(&bytes).map(str::to_string);
    let url = url.map(str::to_string).or(saved_url).context("the page has no `<!-- url: -->` line; give the link")?;
    let target = Target::from_link(&url).context(NOT_A_LINK)?;
    let page = page::parse(&bytes)?;
    let parse = started.elapsed();
    let (post, _) = build_post(&page, &target, target.key())?;
    Ok((target, post, Timings { parse, extract: started.elapsed() - parse, ..Timings::default() }))
}

async fn extract_command(url: Option<&str>, cookies: bool, saved: Option<PathBuf>, format: Format) -> Result<()> {
    let (target, post, timings, settings) = match saved {
        Some(path) => {
            let (target, post, timings) = read_saved(&path, url)?;
            (target, post, timings, Arc::new(server::settings::Settings::default()))
        }
        None => {
            let url = url.context("give a link to a post")?;
            let target = Target::from_link(url).context(NOT_A_LINK)?;
            let (sites, settings) = offline().await?;
            let choice = if cookies { CookieChoice::Force } else { CookieChoice::Policy };
            let (post, timings) = sites.extract(&target, &settings, choice).await?;
            (target, post, timings, settings)
        }
    };
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    eprintln!(
        "{} as {} {}: fetch {} ms, parse {} ms, extract {} ms, {} requests",
        target.key(),
        target.source.name(),
        target.page_type.as_str(),
        ms(timings.fetch),
        ms(timings.parse),
        ms(timings.extract),
        timings.requests
    );
    let media: Vec<MediaUrl> = post.media.iter().map(MediaUrl::direct).collect();
    let html = render::preview(&post, &media, &settings, &target);
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&post)?),
        Format::Preview => println!("{}", html.split("<body>").next().unwrap_or_default().trim_end()),
        Format::Iv => println!("{html}"),
    }
    Ok(())
}

async fn record_command(url: &str, cookies: bool, out: Option<PathBuf>) -> Result<()> {
    let target = Target::from_link(url).context(NOT_A_LINK)?;
    let (sites, settings) = offline().await?;
    let saved = sites.record(&target, &settings, cookies).await?;
    let out = out.unwrap_or_else(|| {
        use sha2::Digest;
        let hash = sha2::Sha256::digest(target.key().as_bytes());
        let hash = data_encoding::HEXLOWER.encode(&hash[..4]);
        let extension = if target.source == Source::Facebook { "html" } else { "json" };
        let (site, page_type) = (target.source.as_str(), target.page_type.as_str());
        PathBuf::from(format!("server/tests/fixtures/{site}-{page_type}-{hash}.{extension}"))
    });
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&out, saved).with_context(|| format!("can't write {}", out.display()))?;
    if cookies {
        eprintln!("saved {}; it holds your account's name and more: keep it out of git", out.display());
    } else {
        eprintln!("saved {} (comments and other people removed); review it before committing", out.display());
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
    eprintln!(
        "it works once, within 30 minutes, and reaches only the Security page, for an hour: set a password \
         or add a passkey"
    );
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
