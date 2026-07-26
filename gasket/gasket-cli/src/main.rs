//! gasket CLI REPL: 组装 gasket-host 模块 + run_agent_loop，交互式终端 agent。
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gasket_core::{
    built_in_tools, run_agent_loop, AgentContext, AgentMessage, ContentBlock, UserMessage,
};
use gasket_host::{ConfigLoader, EventPrinter, Mode, PermissionPolicy, SessionManager};
use reedline::{DefaultPrompt, Reedline, Signal};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host_cfg = match ConfigLoader::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}\nset GASKET_LLM_* in .env or env");
            std::process::exit(1);
        }
    };
    let mode = std::env::args()
        .find_map(|a| a.strip_prefix("--mode=").and_then(Mode::parse))
        .unwrap_or(Mode::AutoEdit);
    let resume_arg = std::env::args().find_map(|a| a.strip_prefix("--resume=").map(String::from));

    let mut session = SessionManager::new();
    let mut history: Vec<AgentMessage> = Vec::new();
    if let Some(r) = resume_arg {
        let res = if r == "last" {
            session.resume_last().await
        } else {
            session.resume(&r).await
        };
        match res {
            Ok(m) => {
                println!("(resumed {} with {} msgs)", session.current_id(), m.len());
                history = m;
            }
            Err(e) => println!("(resume: {e})"),
        }
    }

    let policy = Arc::new(PermissionPolicy::new(mode, stdin_approver));
    let cwd = std::env::current_dir()?;
    let system_prompt = "You are a helpful, concise assistant.".to_string();

    // Cooperative-abort signal: a Ctrl-C during LLM streaming (cooked tty mode)
    // sets this and the agent loop exits at the next safe point, returning the
    // partial transcript. Spawned once; the loop re-arms after each Ctrl-C so
    // every press is honored. At the prompt (raw mode) Ctrl-C is a key event
    // handled by reedline, not a SIGINT, so it doesn't fire here.
    let signal = Arc::new(AtomicBool::new(false));
    {
        let s = signal.clone();
        tokio::spawn(async move {
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    break; // handler could not be installed; give up quietly
                }
                s.store(true, Ordering::Relaxed);
            }
        });
    }
    let config = host_cfg.build_loop_config(
        host_cfg.tunables.max_turns,
        Some(signal.clone()),
        Some(policy.clone()),
    );

    let mut rl = Reedline::create();
    let prompt = DefaultPrompt::default();
    while let Ok(Signal::Success(line)) = rl.read_line(&prompt) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(cmd) = line.strip_prefix('/') {
            handle_slash(cmd, &mut session, &mut history, &policy).await;
            continue;
        }
        let user_msg = AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text(line.to_string())],
            timestamp: gasket_core::now(),
        });
        let context = AgentContext {
            system_prompt: system_prompt.clone(),
            messages: history.clone(),
            tools: built_in_tools(),
            cwd: cwd.clone(),
            env: std::env::vars().collect(),
            session_id: session.current_id().to_string(),
        };
        let mut printer = EventPrinter::new(io::stdout());
        // Clear any abort left over from a prior run before starting this one.
        signal.store(false, Ordering::Relaxed);
        match run_agent_loop(vec![user_msg], context, config.clone(), |ev| {
            printer.on_event(&ev);
        })
        .await
        {
            Ok(new_msgs) => {
                history.extend(new_msgs.iter().cloned());
                let _ = session.append(&new_msgs).await;
            }
            Err(e) => eprintln!("\n(run error: {e})"),
        }
        let _ = io::stdout().flush();
    }
    Ok(())
}

fn stdin_approver(name: &str, _args: &serde_json::Value) -> bool {
    print!("\n[approve {name}? y/N] ");
    let _ = io::stdout().flush();
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
    s.trim().eq_ignore_ascii_case("y")
}

async fn handle_slash(
    cmd: &str,
    session: &mut SessionManager,
    history: &mut Vec<AgentMessage>,
    policy: &Arc<PermissionPolicy>,
) {
    let mut parts = cmd.split_whitespace();
    match parts.next() {
        Some("exit") | Some("quit") => std::process::exit(0),
        Some("clear") => {
            session.clear();
            history.clear();
            println!("(new session)");
        }
        Some("mode") => match parts.next().and_then(Mode::parse) {
            Some(m) => {
                policy.set_mode(m);
                println!("(mode -> {m:?})");
            }
            None => println!("usage: /mode <suggest|auto-edit|full-auto>"),
        },
        Some("resume") => {
            let arg = parts.next().unwrap_or("last");
            let r = if arg == "last" {
                session.resume_last().await
            } else {
                session.resume(arg).await
            };
            match r {
                Ok(m) => {
                    *history = m;
                    println!("(resumed {} with {} msgs)", session.current_id(), history.len());
                }
                Err(e) => println!("(resume: {e})"),
            }
        }
        Some("sessions") => match session.list().await {
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
        Some("help") => println!(
            "commands: /resume [id|last]  /clear  /mode <suggest|auto-edit|full-auto>  /sessions  /exit"
        ),
        _ => println!("unknown command; /help"),
    }
}
