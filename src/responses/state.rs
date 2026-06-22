use std::collections::VecDeque;

use serde_json::Value;
use tokio::sync::RwLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsesTurnIdentity {
    pub interaction_id: String,
    pub agent_task_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResponsesStateEntry {
    pub transcript: Vec<Value>,
    pub identity: ResponsesTurnIdentity,
    pub last_response_had_tool_calls: bool,
}

#[derive(Debug)]
pub struct ResponsesStateStore {
    state_limit: usize,
    max_transcript_items: usize,
    inner: RwLock<VecDeque<(String, ResponsesStateEntry)>>,
}

impl ResponsesStateStore {
    pub fn new(state_limit: usize, max_transcript_items: usize) -> Self {
        Self {
            state_limit: state_limit.max(1),
            max_transcript_items: max_transcript_items.max(1),
            inner: RwLock::new(VecDeque::new()),
        }
    }

    pub async fn cache_response_state(
        &self,
        response_id: impl Into<String>,
        input_items: Vec<Value>,
        output_items: Vec<Value>,
        identity: ResponsesTurnIdentity,
        last_response_had_tool_calls: bool,
    ) {
        let response_id = response_id.into();
        if response_id.is_empty() {
            return;
        }
        let mut transcript = input_items;
        transcript.extend(output_items);
        if transcript.len() > self.max_transcript_items {
            transcript = transcript.split_off(transcript.len() - self.max_transcript_items);
        }
        let entry = ResponsesStateEntry {
            transcript,
            identity,
            last_response_had_tool_calls,
        };
        let mut inner = self.inner.write().await;
        if let Some(index) = inner.iter().position(|(key, _)| key == &response_id) {
            inner.remove(index);
        }
        inner.push_back((response_id, entry));
        while inner.len() > self.state_limit {
            inner.pop_front();
        }
    }

    pub async fn get_cached_response_state(
        &self,
        response_id: &str,
    ) -> Option<ResponsesStateEntry> {
        if response_id.is_empty() {
            return None;
        }
        let mut inner = self.inner.write().await;
        let index = inner.iter().position(|(key, _)| key == response_id)?;
        let (key, entry) = inner.remove(index)?;
        let cloned = entry.clone();
        inner.push_back((key, entry));
        Some(cloned)
    }
}

impl Default for ResponsesStateStore {
    fn default() -> Self {
        Self::new(128, 500)
    }
}
