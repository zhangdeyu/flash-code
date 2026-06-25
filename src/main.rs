use std::sync::Arc;

use clap::Parser;

use flash_code::config::Config;
use flash_code::engine::{yolo_approval, DefaultPolicy};
use flash_code::protocol::{ContentBlock, Event};
use flash_code::provider::deepseek::DeepSeekProvider;
use flash_code::session::{AgentConfig, Session};
use flash_code::sink::jsonl::JsonlSink;
use flash_code::sink::EventSink;
use flash_code::tool::bash::BashTool;
use flash_code::tool::ToolRegistry;

#[derive(Parser)]
#[command(name = "flash", version, about)]
struct Cli {
    /// The query to send to the agent.
    query: String,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            eprintln!("create ~/.flash/config.toml with:\n  api_key = \"sk-...\"");
            std::process::exit(1);
        }
    };

    let capability = config.capability();
    let provider = DeepSeekProvider::new(
        config.api_key,
        config.base_url,
        config.model,
        config.reasoning_effort,
        capability,
    );

    let sink: Arc<dyn EventSink> = Arc::new(JsonlSink::stdout());

    let session_id = uuid::Uuid::new_v4().to_string();
    sink.emit(Event::SessionStarted {
        session_id: session_id.clone(),
    })
    .await;

    let mut session = Session::new(session_id, sink);

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(BashTool));

    let policy = DefaultPolicy::default();
    let cfg = AgentConfig::default();

    // Ctrl-C handler
    let cancel_handle = session.cancel_root.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel_handle.cancel();
        }
    });

    let user_input = vec![ContentBlock::text(cli.query)];

    if let Err(e) = session
        .send(
            user_input,
            &provider,
            &registry,
            &policy,
            yolo_approval(),
            &cfg,
        )
        .await
    {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
