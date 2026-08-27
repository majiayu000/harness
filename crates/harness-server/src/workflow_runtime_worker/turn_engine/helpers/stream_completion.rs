use harness_core::agent::StreamItem;
use harness_core::types::Item;

/// Mid-turn budget stop: streamed usage reached the workflow ceiling.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnBudgetStop {
    pub(crate) workflow_id: String,
    pub(crate) spent_usd: f64,
    pub(crate) budget_usd: f64,
}

#[derive(Debug, Default)]
pub(crate) struct StreamCompletionState {
    output_buf: String,
    emitted_agent_completion: bool,
}

impl StreamCompletionState {
    pub(crate) fn normalize(&mut self, stream_item: StreamItem) -> Option<StreamItem> {
        match stream_item {
            StreamItem::MessageDelta { text } => {
                self.output_buf.push_str(&text);
                Some(StreamItem::MessageDelta { text })
            }
            StreamItem::ItemCompleted { item } => {
                if let Item::AgentReasoning { content } = &item {
                    self.output_buf.clear();
                    self.output_buf.push_str(content);
                    self.emitted_agent_completion = true;
                }
                Some(StreamItem::ItemCompleted { item })
            }
            StreamItem::Diagnostic { severity, message } => {
                match severity {
                    harness_core::agent::AgentDiagnosticSeverity::Warning => {
                        tracing::warn!(agent_diagnostic = true, "{message}");
                    }
                    harness_core::agent::AgentDiagnosticSeverity::Error => tracing::error!(
                        agent_diagnostic = true,
                        "non-terminal agent diagnostic: {message}"
                    ),
                }
                Some(StreamItem::Warning { message })
            }
            StreamItem::TurnCompleted { output } => {
                if self.emitted_agent_completion {
                    self.output_buf.clear();
                    return None;
                }
                let content = if output.is_empty() {
                    std::mem::take(&mut self.output_buf)
                } else {
                    output
                };
                if content.is_empty() {
                    None
                } else {
                    self.emitted_agent_completion = true;
                    Some(StreamItem::ItemCompleted {
                        item: Item::AgentReasoning { content },
                    })
                }
            }
            other => Some(other),
        }
    }
}
