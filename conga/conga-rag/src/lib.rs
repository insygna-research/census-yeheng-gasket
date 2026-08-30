//! conga-rag — personal RAG pipeline (ingest → clean → chunk → embed → store)
//! and headless retrieval CLI, built on the conga harness.

pub mod chunk;
pub mod clean;
pub mod config;
pub mod embed;
pub mod source;
pub mod store;
