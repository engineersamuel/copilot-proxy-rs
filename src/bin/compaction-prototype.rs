//! PROTOTYPE — run with: cargo run --bin compaction-prototype

#[path = "../responses/compaction_prototype.rs"]
mod compaction_prototype;

use std::error::Error;
use std::io::{self, Write};

use compaction_prototype::{
    ClientProtocol, CompactedConversation, CompactionDraft, ConversationItem, Role,
    begin_compaction, expand_for_upstream, finish_compaction, sample_model_summary,
    sample_transcript,
};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
struct UiState {
    transcript: Vec<ConversationItem>,
    draft: Option<CompactionDraft>,
    compacted: Option<CompactedConversation>,
    expanded_upstream_request: Option<Value>,
    status: String,
}

impl UiState {
    fn new() -> Self {
        Self {
            transcript: sample_transcript(),
            draft: None,
            compacted: None,
            expanded_upstream_request: None,
            status: "Choose a client protocol to build its internal summary request.".to_string(),
        }
    }

    fn begin(&mut self, protocol: ClientProtocol) {
        let model = match protocol {
            ClientProtocol::OpenAiResponses => "gpt-5.6-sol",
            ClientProtocol::AnthropicMessages => "claude-sonnet-4.6",
        };
        match begin_compaction(&self.transcript, protocol, model) {
            Ok(draft) => {
                self.draft = Some(draft);
                self.compacted = None;
                self.expanded_upstream_request = None;
                self.status =
                    "Summary request ready. [m] supplies the simulated upstream LLM result."
                        .to_string();
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    fn complete_model_call(&mut self) {
        let Some(draft) = self.draft.take() else {
            self.status = "Build a summary request first with [o] or [c].".to_string();
            return;
        };

        match finish_compaction(draft, sample_model_summary()) {
            Ok(compacted) => {
                self.compacted = Some(compacted);
                self.expanded_upstream_request = None;
                self.status = "Client compaction emitted. [x] round-trips and expands it upstream."
                    .to_string();
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    fn expand(&mut self) {
        let Some(compacted) = &self.compacted else {
            self.status = "Complete a model summary first with [m].".to_string();
            return;
        };

        match expand_for_upstream(
            compacted,
            "Continue implementation from the compacted state.",
        ) {
            Ok(request) => {
                self.expanded_upstream_request = Some(request);
                self.status =
                    "Round-tripped compaction expanded into ordinary model-readable input."
                        .to_string();
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    fn append_user(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            self.status = "Usage: a <user message>".to_string();
            return;
        }

        self.transcript.push(ConversationItem::Message {
            role: Role::User,
            text: text.to_string(),
        });
        self.draft = None;
        self.compacted = None;
        self.expanded_upstream_request = None;
        self.status = "User message appended; derived compaction state cleared.".to_string();
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let stdin = io::stdin();
    let mut state = UiState::new();

    loop {
        render(&state)?;

        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        let (command, argument) = trimmed
            .split_once(' ')
            .map_or((trimmed, ""), |(command, argument)| (command, argument));

        match command {
            "o" => state.begin(ClientProtocol::OpenAiResponses),
            "c" => state.begin(ClientProtocol::AnthropicMessages),
            "m" => state.complete_model_call(),
            "x" => state.expand(),
            "a" => state.append_user(argument),
            "r" => state = UiState::new(),
            "q" => break,
            _ => state.status = format!("Unknown command: {command}"),
        }
    }

    Ok(())
}

fn render(state: &UiState) -> Result<(), Box<dyn Error>> {
    print!("\x1b[2J\x1b[H");
    println!("\x1b[1mCompaction contract prototype\x1b[0m");
    println!(
        "\x1b[2mQuestion: can one LLM summary safely round-trip through both client contracts?\x1b[0m\n"
    );
    println!("{}", serde_json::to_string_pretty(state)?);
    println!("\n\x1b[1m{}\x1b[0m", state.status);
    println!(
        "\n[o] OpenAI draft  [c] Claude draft  [m] model result  [x] expand  \
         [a <text>] append  [r] reset  [q] quit"
    );
    print!("> ");
    io::stdout().flush()?;
    Ok(())
}
