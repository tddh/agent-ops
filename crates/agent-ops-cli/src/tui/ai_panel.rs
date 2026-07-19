use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub code_blocks: Vec<String>,
}

const MAX_MESSAGES: usize = 200;

#[derive(Clone)]
pub struct QuestionInfo {
    pub question_id: String,
    pub text: String,
}

#[derive(Clone)]
pub struct AiPanel {
    pub messages: Arc<Mutex<Vec<Message>>>,
    pub input: Arc<Mutex<String>>,
    pub thinking: Arc<Mutex<bool>>,
    pub pending_question: Arc<Mutex<Option<QuestionInfo>>>,
}

impl AiPanel {
    pub fn new() -> Self {
        Self {
            messages: Arc::new(Mutex::new(Vec::new())),
            input: Arc::new(Mutex::new(String::new())),
            thinking: Arc::new(Mutex::new(false)),
            pending_question: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn pending_question(&self) -> Option<QuestionInfo> {
        self.pending_question.lock().await.clone()
    }

    pub async fn set_pending_question(&self, q: Option<QuestionInfo>) {
        *self.pending_question.lock().await = q;
    }

    pub async fn add_message(&self, msg: Message) {
        let mut msgs = self.messages.lock().await;
        if msgs.len() >= MAX_MESSAGES {
            msgs.remove(0);
        }
        msgs.push(msg);
    }

    #[allow(dead_code)]
    pub async fn last_code_block(&self) -> Option<String> {
        let msgs = self.messages.lock().await;
        msgs.iter()
            .rev()
            .find(|m| matches!(m.role, Role::Assistant))
            .and_then(|m| m.code_blocks.last().cloned())
    }

    pub async fn clear(&self) {
        self.messages.lock().await.clear();
    }

    pub async fn append_streaming(&self, text: &str) {
        let mut msgs = self.messages.lock().await;
        let needs_new = msgs
            .last()
            .is_none_or(|m| !matches!(m.role, Role::Assistant));
        if needs_new {
            if msgs.len() >= MAX_MESSAGES {
                msgs.remove(0);
            }
            msgs.push(Message {
                role: Role::Assistant,
                content: text.to_string(),
                code_blocks: vec![],
            });
        } else {
            msgs.last_mut().unwrap().content.push_str(text);
        }
    }

    pub async fn finish_streaming(&self, code_blocks: Vec<String>) {
        let mut msgs = self.messages.lock().await;
        if let Some(last) = msgs.last_mut() {
            if matches!(last.role, Role::Assistant) {
                last.code_blocks = code_blocks;
            }
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect, is_focused: bool, scroll: usize) {
        let msgs = match self.messages.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let input = match self.input.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let thinking = match self.thinking.try_lock() {
            Ok(g) => *g,
            Err(_) => false,
        };

        let mut lines: Vec<Line> = Vec::new();

        for msg in msgs.iter() {
            match msg.role {
                Role::User => {
                    lines.push(Line::from(Span::styled(
                        format!("> {}", msg.content),
                        Style::default().fg(Color::Cyan),
                    )));
                }
                Role::Assistant => {
                    for line in msg.content.lines() {
                        lines.push(Line::from(Span::styled(
                            format!("  {}", line),
                            Style::default().fg(Color::Green),
                        )));
                    }
                }
                Role::System => {
                    lines.push(Line::from(Span::styled(
                        &msg.content,
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
        }

        if thinking {
            lines.push(Line::from(Span::styled(
                "AI thinking...",
                Style::default().fg(Color::Yellow),
            )));
        } else if let Ok(q) = self.pending_question.try_lock() {
            if let Some(ref q) = *q {
                lines.push(Line::from(Span::styled(
                    format!("🔍 AI 提问: {}", q.text),
                    Style::default().fg(Color::Yellow),
                )));
            }
        }

        if is_focused {
            let has_question = self
                .pending_question
                .try_lock()
                .is_ok_and(|q| q.is_some());
            if has_question {
                lines.push(Line::from(Span::styled(
                    "输入回答后按 Enter →",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(format!("> {}_", input)));
            } else if !thinking {
                lines.push(Line::from(format!("> {}_", input)));
            }
        }

        let block = Block::default()
            .borders(Borders::TOP)
            .title(" AI ")
            .border_style(if is_focused {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            });

        let p = Paragraph::new(Text::from(lines))
            .block(block)
            .scroll((scroll as u16, 0));
        f.render_widget(p, area);
    }
}
