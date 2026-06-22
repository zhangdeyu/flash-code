use std::sync::Arc;

use clap::Parser;
use tokio::sync::mpsc;

use flash_code::config::Config;
use flash_code::engine::ApprovalMode;
use flash_code::protocol::Event;
use flash_code::provider::openai::OpenAiProvider;
use flash_code::session::Session;
use flash_code::sink::console::ConsoleSink;
use flash_code::sink::EventSink;
use flash_code::tool::bash::BashTool;

/// flash-code: an AI coding agent powered by OpenAI-compatible models.
#[derive(Parser)]
#[command(name = "flash", version, about)]
struct Cli {
    /// Approval mode: "yolo" (auto-approve all) or "default" (require approval).
    #[arg(long, default_value = "yolo")]
    mode: String,

    /// The query to send to the agent.
    query: String,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let mode = match cli.mode.as_str() {
        "yolo" => ApprovalMode::Yolo,
        "default" => ApprovalMode::Default,
        other => {
            eprintln!("unknown mode: {other} (expected 'yolo' or 'default')");
            std::process::exit(1);
        }
    };

    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            eprintln!("create ~/.flash/config.toml with:\n  api_key = \"sk-...\"");
            std::process::exit(1);
        }
    };

    let provider = OpenAiProvider::new(config.api_key, config.base_url, config.model);

    // JSONL to stdout via stderr-based approach: write JSONL to a temp file
    // For v1, just use ConsoleSink (human-readable to stderr)
    let sink: Arc<dyn EventSink> = Arc::new(ConsoleSink::new());

    let session_id = uuid::Uuid::new_v4().to_string();
    sink.emit(Event::SessionStarted {
        session_id: session_id.clone(),
    })
    .await;

    let mut session = Session::new(session_id, vec![], sink);

    let tools: Vec<Box<dyn flash_code::tool::Tool>> = vec![Box::new(BashTool)];

    // For v1 yolo mode, approval_rx is unused but required by the API
    let (_approval_tx, mut approval_rx) = mpsc::channel::<bool>(1);

    // Set up Ctrl-C handler
    let cancel = session.cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
    });

    if let Err(e) = session.send(cli.query, mode, &provider, &tools, &mut approval_rx).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
