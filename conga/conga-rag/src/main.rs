//! conga-rag — personal RAG headless CLI.

use clap::{Parser, Subcommand};
use conga_rag::config::RagConfig;
use conga_rag::pipeline;

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
    }
    exit(0)
}
