//! conga CLI REPL: 持一个 Host，每行调一次 run_turn，交互式终端 agent。
//! `conga exec <task>`（见 `exec.rs`）是无头一次性入口，共用同一装配。
use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::Arc;

use conga::ToolDefinition;
use conga_host::{gather_tools, install_ctrl_c, EventPrinter, Host, Mode, SessionAssembly};
use reedline::{DefaultPrompt, Reedline, Signal};
mod exec;

/// In-process extensions behind feature `ext`: tools + optional hook chain
/// (`permission_gate`). Without the feature, empty tools / no extra hooks.
fn load_inprocess_ext() -> (Vec<ToolDefinition>, Option<Arc<dyn conga::HookChain>>) {
    #[cfg(feature = "ext")]
    {
        use conga::ExtensionApiImpl;
        let mut api = ExtensionApiImpl::new();
        conga_ext::register_all(&mut api);
        let tools = std::mem::take(&mut api.tools);
        let hooks: Arc<dyn conga::HookChain> = Arc::new(api);
        (tools, Some(hooks))
    }
    #[cfg(not(feature = "ext"))]
    (Vec::new(), None)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `conga exec ...` is the headless one-shot path; everything else is
    // the interactive REPL.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("exec") {
        let code = exec::run(&argv[1..]).await;
        std::process::exit(code);
    }
    let mode = std::env::args()
        .find_map(|a| a.strip_prefix("--mode=").and_then(Mode::parse))
        .unwrap_or(Mode::AutoEdit);
    let resume_arg = std::env::args().find_map(|a| a.strip_prefix("--resume=").map(String::from));

    let (ext_tools, ext_hooks) = load_inprocess_ext();
    if !ext_tools.is_empty() {
        eprintln!("(in-process ext tools: {})", ext_tools.len());
    }

    // One shared assembly (same wiring as the gateway/desktop): skills
    // prompt, ext gate hooks before the permission policy, tool gathering,
    // sub-agent spawner, signal wiring. The stdin approver is the only
    // CLI-specific piece.
    let mut host = match SessionAssembly::build_cli(
        mode,
        Arc::new(stdin_approver),
        resume_arg,
        ext_hooks.into_iter().collect(),
        ext_tools.clone(),
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            eprintln!("config error: {e}\nset CONGA_LLM_* in .env or env");
            std::process::exit(1);
        }
    };

    // Cooperative-abort signal: a Ctrl-C during LLM streaming (cooked tty mode)
    // cancels and the agent loop exits at the next safe point, returning the
    // partial transcript. Every press is honored; run_turn resets the signal.
    // At the prompt (raw mode) Ctrl-C is a key event handled by reedline, not
    // a SIGINT, so it doesn't fire here.
    install_ctrl_c(host.signal().clone());

    let mut rl = Reedline::create();
    let prompt = DefaultPrompt::default();
    while let Ok(Signal::Success(line)) = rl.read_line(&prompt) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(cmd) = line.strip_prefix('/') {
            handle_slash(cmd, &mut host, &ext_tools).await;
            continue;
        }
        // Working history (and its compaction) is log-derived inside
        // run_turn; JSONL on disk stays the append-only full log.
        let mut printer = EventPrinter::new(io::stdout());
        match host
            .run_turn(line, |ev| {
                printer.on_event(&ev);
            })
            .await
        {
            Ok(_summary) => {}
            Err(e) => eprintln!("\n(run error: {e})"),
        }
        let _ = io::stdout().flush();
    }
    Ok(())
}

fn stdin_approver<'a>(
    name: &'a str,
    _args: &'a serde_json::Value,
) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
    // 读 stdin 是阻塞的，挪到 blocking 池，避免卡住 tokio worker。
    let name = name.to_string();
    Box::pin(async move {
        print!("\n[approve {name}? y/N] ");
        let _ = io::stdout().flush();
        tokio::task::spawn_blocking(move || {
            let mut s = String::new();
            let _ = io::stdin().read_line(&mut s);
            s.trim().eq_ignore_ascii_case("y")
        })
        .await
        .unwrap_or(false)
    })
}

async fn handle_slash(cmd: &str, host: &mut Host, ext_tools: &[ToolDefinition]) {
    let mut parts = cmd.split_whitespace();
    match parts.next() {
        Some("exit") | Some("quit") => std::process::exit(0),
        Some("clear") => {
            // Unified semantics: append a Cleared fact to the log (id stays,
            // derive truncates, disk stays append-only). A failed write is
            // reported — a silent failure would resurrect the old history.
            match host.clear_session().await {
                Ok(()) => println!("(cleared)"),
                Err(e) => println!("(clear failed: {e})"),
            }
        }
        Some("mode") => match parts.next().and_then(Mode::parse) {
            Some(m) => {
                host.policy().set_mode(m);
                println!("(mode -> {m:?})");
            }
            None => println!("usage: /mode <suggest|auto-edit|full-auto|plan>"),
        },
        Some("resume") => {
            let arg = parts.next().unwrap_or("last");
            let r = if arg == "last" {
                host.session().resume_last().await
            } else {
                host.session().resume(arg).await
            };
            match r {
                Ok(m) => {
                    // History is re-derived from the event log each turn.
                    println!("(resumed {} with {} msgs)", host.session().current_id(), m.len());
                }
                Err(e) => println!("(resume: {e})"),
            }
        }
        Some("sessions") => match host.session().list().await {
            Ok(list) => {
                if list.is_empty() {
                    println!("(no sessions)");
                }
                for s in list {
                    println!("{} ({} msgs)", s.id, s.msg_count);
                }
            }
            Err(e) => println!("(list: {e})"),
        },
        Some("reload-tools") => {
            // Same gathering as startup (ext tools rank after built-ins;
            // external + MCP are reloaded too - a reload that silently
            // skipped MCP would drift from the initial set).
            host.set_tools(gather_tools(ext_tools.to_vec(), Vec::new(), false).await);
            println!("(reloaded tools)");
        }
        Some("help") => println!(
            "commands: /resume [id|last]  /clear  /mode <suggest|auto-edit|full-auto|plan>  /sessions  /reload-tools  /exit"
        ),
        _ => println!("unknown command; /help"),
    }
}
