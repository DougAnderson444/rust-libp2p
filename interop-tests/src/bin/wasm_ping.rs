#![allow(non_upper_case_globals)]

use std::{future::IntoFuture, process::Stdio, time::Duration};

use anyhow::{bail, Context, Result};
use axum::{
    extract::State,
    http::{header, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use interop_tests::{BlpopRequest, Report, WasmLogBatch};
use redis::{AsyncCommands, Client};
use thirtyfour::prelude::*;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpListener,
    process::Child,
    sync::mpsc,
};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{filter::LevelFilter, fmt, prelude::*, EnvFilter};

mod config;

const BIND_ADDR: &str = "127.0.0.1:8080";

/// Embedded Wasm package
///
/// Make sure to build the wasm with `wasm-pack build --target web`
#[derive(rust_embed::RustEmbed)]
#[folder = "pkg"]
struct WasmPackage;

#[derive(Clone)]
struct TestState {
    redis_client: Client,
    config: config::Config,
    results_tx: mpsc::Sender<Result<Report, String>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // start logging
    let env_filter = match std::env::var_os("RUST_LOG") {
        Some(_) => EnvFilter::try_from_default_env().context("invalid `RUST_LOG` filter")?,
        None => EnvFilter::builder()
            .with_default_directive(LevelFilter::INFO.into())
            .parse_lossy("info,wasm_ping=info,wasm=debug,tower_http=warn,hyper=warn"),
    };
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(env_filter)
        .init();

    // read env variables
    let config = config::Config::from_env()?;
    let test_timeout = Duration::from_secs(config.test_timeout);

    // create a redis client
    let redis_client =
        Client::open(config.redis_addr.as_str()).context("Could not connect to redis")?;
    let (results_tx, mut results_rx) = mpsc::channel(1);

    let state = TestState {
        redis_client,
        config,
        results_tx,
    };

    // create a wasm-app service
    let app = Router::new()
        // Redis proxy
        .route("/blpop", post(redis_blpop))
        // Report tests status
        .route("/results", post(post_results))
        .route("/log", post(host_log))
        // Wasm ping test trigger
        .route("/", get(serve_index_html))
        // Wasm app static files
        .fallback(serve_wasm_pkg)
        // Middleware
        .layer(CorsLayer::very_permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // Run the service in background
    let listener = TcpListener::bind(BIND_ADDR)
        .await
        .with_context(|| format!("bind HTTP harness on {BIND_ADDR}"))?;
    tokio::spawn(axum::serve(listener, app).into_future());

    let headless = std::env::var("WASM_PING_HEADLESS")
        .map(|v| v != "0" && v != "false")
        .unwrap_or(true);
    tracing::info!(
        headless,
        "wasm_ping harness: starting chromedriver + Chrome"
    );

    // Start executing the test in a browser
    let (mut chrome, driver) = open_in_browser(headless).await?;
    tracing::info!("wasm_ping harness: browser loaded test page");

    // Wait for the outcome to be reported
    let test_result = match tokio::time::timeout(test_timeout, results_rx.recv()).await {
        Ok(received) => received.unwrap_or(Err("Results channel closed".to_owned())),
        Err(_) => Err("Test timed out".to_owned()),
    };

    // Close the browser after we got the results
    driver.quit().await?;
    chrome.kill().await?;

    match test_result {
        Ok(report) => println!("{}", serde_json::to_string(&report)?),
        Err(error) => bail!("Tests failed: {error}"),
    }

    Ok(())
}

async fn open_in_browser(headless: bool) -> Result<(Child, WebDriver)> {
    // start a webdriver process
    // currently only the chromedriver is supported as firefox doesn't
    // have support yet for the certhashes
    let chromedriver = if cfg!(windows) {
        "chromedriver.cmd"
    } else {
        "chromedriver"
    };
    let mut chrome = tokio::process::Command::new(chromedriver)
        .arg("--port=45782")
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn `{chromedriver}` (is it installed and on PATH?)"))?;
    // read driver's stdout
    let driver_out = chrome
        .stdout
        .take()
        .context("No stdout found for webdriver")?;
    // wait for the 'ready' message
    let mut reader = BufReader::new(driver_out).lines();
    while let Some(line) = reader.next_line().await? {
        tracing::debug!(chromedriver_stdout = %line, "chromedriver log line");
        // "successfully" line format varies by ChromeDriver version (trailing period optional).
        if line.contains("ChromeDriver was started successfully") {
            break;
        }
    }

    // run a webdriver client
    let mut caps = DesiredCapabilities::chrome();
    if headless {
        caps.set_headless()?;
    }
    caps.set_disable_dev_shm_usage()?;
    caps.set_no_sandbox()?;
    let driver = WebDriver::new("http://localhost:45782", caps).await?;
    // go to the wasm test service
    driver.goto(format!("http://{BIND_ADDR}")).await?;

    Ok((chrome, driver))
}

/// Redis proxy handler.
/// `blpop` is currently the only redis client method used in a ping dialer.
async fn redis_blpop(
    state: State<TestState>,
    request: Json<BlpopRequest>,
) -> Result<Json<Vec<String>>, StatusCode> {
    let client = state.0.redis_client;
    let mut conn = client.get_async_connection().await.map_err(|e| {
        tracing::warn!("Failed to connect to redis: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let res: Vec<String> = conn
        .blpop(&request.key, request.timeout as f64)
        .await
        .map_err(|e| {
            tracing::warn!(
                key=%request.key,
                timeout=%request.timeout,
                "Failed to get list elem key within timeout: {e}"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(res))
}

async fn host_log(Json(batch): Json<WasmLogBatch>) -> StatusCode {
    for line in batch.lines {
        if line.starts_with("WARN ") || line.starts_with("ERROR ") {
            tracing::warn!(target: "wasm", "{line}");
        } else {
            tracing::debug!(target: "wasm", "{line}");
        }
    }
    StatusCode::OK
}

/// Receive test results
async fn post_results(
    state: State<TestState>,
    request: Json<Result<Report, String>>,
) -> Result<(), StatusCode> {
    state.0.results_tx.send(request.0).await.map_err(|_| {
        tracing::error!("Failed to send results");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

/// Serve the main page which loads our javascript
async fn serve_index_html(state: State<TestState>) -> Result<impl IntoResponse, StatusCode> {
    let config::Config {
        transport,
        ip,
        is_dialer,
        test_timeout,
        sec_protocol,
        muxer,
        ..
    } = state.0.config;

    let sec_protocol = sec_protocol
        .map(|p| format!(r#""{p}""#))
        .unwrap_or("null".to_owned());
    let muxer = muxer
        .map(|p| format!(r#""{p}""#))
        .unwrap_or("null".to_owned());

    Ok(Html(format!(
        r#"
        <!DOCTYPE html>
        <html>
        <head>
            <meta charset="UTF-8" />
            <title>libp2p ping test</title>
            <script type="module">
                // import a wasm initialization fn and our test entrypoint
                import init, {{ run_test_wasm }} from "/interop_tests.js";

                // initialize wasm
                await init()
                // run our entrypoint with params from the env
                await run_test_wasm(
                    "{transport}",
                    "{ip}",
                    {is_dialer},
                    "{test_timeout}",
                    "{BIND_ADDR}",
                    {sec_protocol},
                    {muxer}
                )
            </script>
        </head>

        <body></body>
        </html>
    "#
    )))
}

async fn serve_wasm_pkg(uri: Uri) -> Result<Response, StatusCode> {
    let path = uri.path().trim_start_matches('/').to_string();
    if let Some(content) = WasmPackage::get(&path) {
        let mime = mime_guess::from_path(&path).first_or_octet_stream();
        Ok(Response::builder()
            .header(header::CONTENT_TYPE, mime.as_ref())
            .body(content.data.into())
            .unwrap())
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}
