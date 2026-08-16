//! conga CLI REPL: 持一个 Host，每行调一次 run_turn，交互式终端 agent。
use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::Arc;

use conga::ToolDefinition;
use conga_host::{
    commands_from_env, install_ctrl_c, load_all_mcp, load_external_tools, ConfigLoader,
    EventPrinter, HookStack, Host, Mode, PermissionPolicy, SessionManager,
};
use reedline::{DefaultPrompt, Reedline, Signal};

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

/// built-in + in-process ext + external + mcp tools, in that precedence order.
fn assemble_tools(
    ext_tools: &[ToolDefinition],
    extra_tools: &[ToolDefinition],
    mcp_tools: &[ToolDefinition],
) -> Vec<ToolDefinition> {
    let mut tools = conga_host::built_in_tools();
    tools.extend(ext_tools.iter().cloned());
    tools.extend(extra_tools.iter().cloned());
    tools.extend(mcp_tools.iter().cloned());
    tools
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host_cfg = match ConfigLoader::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}\nset CONGA_LLM_* in .env or env");
            std::process::exit(1);
        }
    };
    let mode = std::env::args()
        .find_map(|a| a.strip_prefix("--mode=").and_then(Mode::parse))
        .unwrap_or(Mode::AutoEdit);
    let resume_arg = std::env::args().find_map(|a| a.strip_prefix("--resume=").map(String::from));

    let session = SessionManager::new();
    if let Some(r) = resume_arg {
        let res = if r == "last" {
            session.resume_last().await
        } else {
            session.resume(&r).await
        };
        match res {
            Ok(m) => println!("(resumed {} with {} msgs)", session.current_id(), m.len()),
            Err(e) => println!("(resume: {e})"),
        }
    }

    let (ext_tools, ext_hooks) = load_inprocess_ext();
    if !ext_tools.is_empty() {
        eprintln!("(in-process ext tools: {})", ext_tools.len());
    }
    let extra_tools = load_external_from_env().await;
    let mcp_tools = load_all_mcp().await;

    // ext gate first (pattern block), then permission mode/approver.
    let policy = Arc::new(PermissionPolicy::new(mode, Arc::new(stdin_approver)));
    let mut hook_stack = HookStack::new(Vec::new());
    if let Some(h) = ext_hooks {
        hook_stack.push(h);
    }
    hook_stack.push(policy.clone());

    let cwd = conga_host::project_dir();
    let mut host = Host::new(
        host_cfg,
        session,
        policy.clone(),
        conga_host::append_skills("You are a helpful, concise assistant.", &cwd),
        assemble_tools(&ext_tools, &extra_tools, &mcp_tools),
    )
    .with_hooks(Arc::new(hook_stack));
    // The policy's approver waits on stdin; give it the Host's abort signal
    // so Ctrl-C during a pending approval unwinds the wait.
    policy.set_signal(host.signal().clone());

    // Cooperative-abort signal: a Ctrl-C during LLM streaming (cooked tty mode)
    // sets this and the agent loop exits at the next safe point, returning the
    // partial transcript. Every press is honored; run_turn re-arms the flag.
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

async fn load_external_from_env() -> Vec<ToolDefinition> {
    let cmds = commands_from_env();
    if cmds.is_empty() {
        return Vec::new();
    }
    match load_external_tools(&cmds).await {
        Ok(t) => {
            eprintln!(
                "(external tools: {} from {} command(s))",
                t.len(),
                cmds.len()
            );
            t
        }
        Err(e) => {
            eprintln!("(external tools load failed: {e})");
            Vec::new()
        }
    }
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
            None => println!("usage: /mode <suggest|auto-edit|full-auto>"),
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
            let extra = load_external_from_env().await;
            host.set_tools(assemble_tools(ext_tools, &extra, &[]));
            println!("(reloaded {} external tool(s))", extra.len());
        }
        Some("help") => println!(
            "commands: /resume [id|last]  /clear  /mode <suggest|auto-edit|full-auto>  /sessions  /reload-tools  /exit"
        ),
        _ => println!("unknown command; /help"),
    }
}
