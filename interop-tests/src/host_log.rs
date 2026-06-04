//! Forward WASM `tracing` events to the `wasm_ping` host via `POST /log`.

#![cfg(target_arch = "wasm32")]

use std::sync::{Mutex, OnceLock};

use futures_timer::Delay;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::{
    layer::{Context, Layer},
    util::SubscriberInitExt,
    EnvFilter, Registry,
};
use tracing_subscriber::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::WasmLogBatch;

const FLUSH_INTERVAL_MS: u32 = 250;
const MAX_BATCH: usize = 32;

static HOST_BASE: OnceLock<String> = OnceLock::new();
static PENDING: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

struct HostLogLayer;

struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        Self::append_field(&mut self.message, field.name(), value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        Self::append_field(&mut self.message, field.name(), &format!("{value:?}"));
    }
}

impl MessageVisitor {
    fn append_field(out: &mut String, name: &str, value: &str) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(name);
        out.push('=');
        out.push_str(value);
    }
}

impl<S> Layer<S> for HostLogLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let level = *meta.level();
        if level > Level::DEBUG {
            return;
        }

        let mut visitor = MessageVisitor {
            message: String::new(),
        };
        event.record(&mut visitor);
        let line = if visitor.message.is_empty() {
            format!("{level} {}:(event)", meta.target())
        } else {
            format!("{level} {}: {}", meta.target(), visitor.message)
        };

        if let Some(pending) = PENDING.get() {
            let mut buf = pending.lock().expect("log buffer lock");
            buf.push(line);
        }
    }
}

async fn flush_loop() {
    let base = match HOST_BASE.get() {
        Some(b) => b.clone(),
        None => return,
    };
    let url = format!("http://{base}/log");
    let client = reqwest::Client::new();

    loop {
        Delay::new(std::time::Duration::from_millis(FLUSH_INTERVAL_MS as u64)).await;

        let batch: Vec<String> = PENDING
            .get()
            .and_then(|p| {
                let mut buf = p.lock().ok()?;
                if buf.is_empty() {
                    return None;
                }
                let n = buf.len().min(MAX_BATCH);
                let batch: Vec<String> = buf.drain(..n).collect();
                Some(batch)
            })
            .unwrap_or_default();

        if batch.is_empty() {
            continue;
        }

        let _ = client
            .post(&url)
            .json(&WasmLogBatch { lines: batch })
            .send()
            .await;
    }
}

pub(crate) fn init(host_base: &str) {
    console_error_panic_hook::set_once();

    let host = host_base
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string();
    let _ = HOST_BASE.set(host);
    let _ = PENDING.set(Mutex::new(Vec::new()));

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let _ = Registry::default()
        .with(filter)
        .with(HostLogLayer)
        .with(tracing_wasm::WASMLayer::new(
            tracing_wasm::WASMLayerConfigBuilder::new()
                .set_max_level(tracing::Level::DEBUG)
                .build(),
        ))
        .try_init();

    spawn_local(flush_loop());
}
