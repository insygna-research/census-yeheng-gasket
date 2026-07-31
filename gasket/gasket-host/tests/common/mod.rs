//! Deterministic fake `StreamFn` for offline integration tests (dev-only).

#![cfg(test)]

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use gasket_core::{AgentMessage, ModelSpec, StreamChunk, StreamFn, ToolDefinition};

/// Each `stream()` call pops one script and yields its chunks in order.
/// The Nth call returns the Nth script, so a tool call is script 1
/// (`ToolCallDelta → Done`, triggers execution) and script 2 (`TextDelta →
/// Done`) closes the turn. Fully deterministic.
///
/// Script underflow panics rather than silently yielding `Done`: a silent
/// fallback would turn a mis-written test (or a retry the test didn't budget
/// for) into a false positive pass.
pub struct FakeStream {
    scripts: Mutex<VecDeque<Vec<StreamChunk>>>,
}

impl FakeStream {
    pub fn new(scripts: Vec<Vec<StreamChunk>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
        }
    }
}

impl StreamFn for FakeStream {
    fn stream(
        &self,
        _model: &ModelSpec,
        _messages: &[AgentMessage],
        _system: &str,
        _tools: &[ToolDefinition],
        _signal: Option<Arc<AtomicBool>>,
    ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
        let chunks = self.scripts.lock().unwrap().pop_front().expect(
            "FakeStream: script underflow — test supplied fewer scripts than stream() calls",
        );
        Box::pin(futures_util::stream::iter(chunks))
    }
}
