use std::io::{BufRead, Write};
use std::sync::Arc;

use clap::Parser;

use flash_code::config::Config;
use flash_code::engine::{const_approval, yolo_approval, ApprovalCallback, DefaultPolicy};
use flash_code::protocol::{ContentBlock, Event};
use flash_code::provider::openai::OpenAiProvider;
use flash_code::session::{AgentConfig, Session};
use flash_code::sink::console::ConsoleSink;
use flash_code::sink::EventSink;
use flash_code::tool::bash::BashTool;
use flash_code::tool::{ApprovalMode, ToolRegistry};

#[derive(Parser)]
#[command(name = "flash", version, about)]
struct Cli {
    /// Approval mode: "yolo" (auto-approve all) or "default" (require approval).
    #[arg(long, default_value = "yolo")]
    mode: String,

    /// The query to send to the agent.
    query: String,
}

fn stdin_approval() -> ApprovalCallback {
    Arc::new(|call, _ctx| {
        let summary = call.input.get("command").and_then(|v| v.as_str()).map_or_else(
            || serde_json::to_string(&call.input).unwrap_or_default(),
            ToOwned::to_owned,
        );
        Box::pin(async move {
            eprintln!("\n[approval] {summary}");
            eprint!("approve? [y/N] ");
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            let stdin = std::io::stdin();
            let _ = stdin.lock().read_line(&mut line);
            matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        })
    })
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

    let capability = config.capability();
    let provider = OpenAiProvider::new(
        config.api_key,
        config.base_url,
        config.model,
        capability,
    );

    let sink: Arc<dyn EventSink> = Arc::new(ConsoleSink::new());

    let session_id = uuid::Uuid::new_v4().to_string();
    sink.emit(Event::SessionStarted {
        session_id: session_id.clone(),
    })
    .await;

    let mut session = Session::new(session_id, sink);

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(BashTool));

    let approval = match mode {
        ApprovalMode::Yolo => yolo_approval(),
        ApprovalMode::Default => stdin_approval(),
    };
    let _ = const_approval; // suppress unused warning

    let policy = DefaultPolicy::default();
    let cfg = AgentConfig {
        approval_mode: mode,
        ..AgentConfig::default()
    };

    // Ctrl-C handler
    let cancel_handle = session.cancel_root.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel_handle.cancel();
        }
    });

    let user_input = vec![ContentBlock::text(cli.query)];

    if let Err(e) = session
        .send(user_input, &provider, &registry, &policy, approval, &cfg)
        .await
    {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
