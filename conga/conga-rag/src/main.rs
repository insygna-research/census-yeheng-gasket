//! conga-rag — personal RAG headless CLI.

use std::sync::Arc;

use clap::{Parser, Subcommand};
use conga_rag::ask;
use conga_rag::config::RagConfig;
use conga_rag::pipeline;
use conga_rag::search;

#[derive(Parser)]
#[command(
    name = "conga-rag",
    version,
    about = "Personal RAG: ingest / search / ask"
)]
struct Cli {
    /// NDJSON on stdout (machine mode); chatter goes to stderr
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan configured sources, embed, and upsert into the vector store
    Ingest {
        /// Only ingest this source (config section name)
        #[arg(short, long)]
        source: Option<String>,
        /// Delete the store file and re-ingest from scratch
        #[arg(long)]
        rebuild: bool,
    },
    /// Vector search over the index
    Search {
        query: String,
        /// Top-k results (default 5)
        #[arg(short, long)]
        k: Option<usize>,
        /// Restrict to one source
        #[arg(short, long)]
        source: Option<String>,
    },
    /// Retrieve top-k, then answer the question with a chat model (CONGA_LLM_*)
    Ask {
        question: String,
        /// Top-k context chunks (default: ask.top_k from config)
        #[arg(short, long)]
        k: Option<usize>,
    },
}

fn exit(code: i32) -> ! {
    std::process::exit(code)
}

fn load_config_or_exit() -> (std::path::PathBuf, RagConfig) {
    match RagConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("conga-rag: {e}");
            exit(2)
        }
    }
}

fn print_stats(json: bool, path: &std::path::Path, s: &pipeline::IngestStats) {
    if json {
        println!(
            "{}",
            serde_json::json!({"store": path.display().to_string(), "scanned": s.scanned,
                "added": s.added, "updated": s.updated, "removed": s.removed,
                "skipped": s.skipped, "failed": s.failed, "chunks": s.chunks})
        );
    } else {
        println!(
            "scanned={} added={} updated={} removed={} skipped={} failed={} chunks={}",
            s.scanned, s.added, s.updated, s.removed, s.skipped, s.failed, s.chunks
        );
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Ingest { source, rebuild } => {
            let (path, cfg) = load_config_or_exit();
            match pipeline::run_ingest(&cfg, source.as_deref(), rebuild).await {
                Ok(s) => {
                    if s.failed > 0 {
                        eprintln!("conga-rag: {failed} 个文件失败(见上方)", failed = s.failed);
                    }
                    print_stats(cli.json, &path, &s);
                    if s.failed > 0 && s.added + s.updated + s.skipped == 0 {
                        exit(1)
                    }
                }
                Err(e) => {
                    eprintln!("conga-rag: {e}");
                    exit(1)
                }
            }
        }
        Cmd::Search { query, k, source } => {
            let (_path, cfg) = load_config_or_exit();
            let k = k.unwrap_or(5);
            match search::run_search(&cfg, &query, k, source.as_deref()).await {
                Ok(hits) => {
                    if hits.is_empty() {
                        eprintln!("conga-rag: 索引为空:请先运行 conga-rag ingest");
                        exit(1)
                    }
                    if cli.json {
                        for h in &hits {
                            println!(
                                "{}",
                                serde_json::json!({"score": h.score, "source": h.source,
                                    "path": h.path, "ordinal": h.ordinal, "content": h.content})
                            );
                        }
                    } else {
                        for (i, h) in hits.iter().enumerate() {
                            println!("[{}] {:.3} {}:{}", i, h.score, h.source, h.path);
                            let preview: String = h.content.chars().take(200).collect();
                            println!("    {preview}");
                        }
                    }
                    exit(0)
                }
                Err(e) => {
                    eprintln!("conga-rag: {e}");
                    exit(1)
                }
            }
        }
        Cmd::Ask { question, k } => {
            let (_path, cfg) = load_config_or_exit();
            let k = k.unwrap_or(cfg.ask.top_k);
            let hits = match search::run_search(&cfg, &question, k, None).await {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("conga-rag: {e}");
                    exit(1)
                }
            };
            if hits.is_empty() {
                eprintln!("conga-rag: 无相关资料");
                exit(1)
            }
            let provider = match conga::ProviderConfig::from_env() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("conga-rag: ask 需要 CONGA_LLM_* 配置: {e}");
                    exit(2)
                }
            };
            let max_tokens: usize = std::env::var("CONGA_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4096);
            let model = conga::ModelSpec {
                id: provider.model.clone(),
                api: provider.api,
                max_tokens,
            };
            let stream: std::sync::Arc<dyn conga::StreamFn> = match provider.api {
                conga::ProviderApi::OpenAiCompat => {
                    Arc::new(conga::OpenAiCompat::from_config(&provider))
                }
                conga::ProviderApi::Anthropic => {
                    Arc::new(conga::AnthropicProvider::from_config(&provider))
                }
            };
            use std::io::Write;
            let mut json_out = |t: &str| {
                if cli.json {
                    println!("{}", serde_json::json!({"type": "delta", "text": t}));
                } else {
                    print!("{t}");
                    let _ = std::io::stdout().flush();
                }
            };
            match ask::run_ask(stream, model, &question, &hits, &mut json_out).await {
                Ok(()) => {
                    if cli.json {
                        let cites: Vec<_> = hits
                            .iter()
                            .enumerate()
                            .map(|(i, h)| {
                                serde_json::json!({"n": i + 1, "source": h.source, "path": h.path,
                                    "score": h.score})
                            })
                            .collect();
                        println!(
                            "{}",
                            serde_json::json!({"type": "citations", "cites": cites})
                        );
                    } else {
                        println!("\n引用:");
                        for (i, h) in hits.iter().enumerate() {
                            println!(
                                "  [{}] {}:{} (score {:.3})",
                                i + 1,
                                h.source,
                                h.path,
                                h.score
                            );
                        }
                    }
                    exit(0)
                }
                Err(e) => {
                    eprintln!("conga-rag: {e}");
                    exit(1)
                }
            }
        }
    }
    exit(0)
}
