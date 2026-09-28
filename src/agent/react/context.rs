use std::time::SystemTime;

use serde::Serialize;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventName {
    ToolCall,
    ToolResult,
    Thought,
    Answer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    pub name: EventName,
    pub content: String,
    pub role: Role,
    #[serde(serialize_with = "serialize_timestamp")]
    pub timestamp: SystemTime,
}

fn serialize_timestamp<S>(timestamp: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let millis = timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    serializer.serialize_u64(millis)
}

impl Event {
    pub fn new(name: EventName, content: impl Into<String>, role: Role) -> Self {
        Self {
            name,
            content: content.into(),
            role,
            timestamp: SystemTime::now(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExecuteContext {
    turn: usize,
    id: Uuid,
    status: Status,
    events: Vec<Event>,
}

impl ExecuteContext {
    pub fn new() -> Self {
        Self {
            turn: 0,
            id: Uuid::new_v4(),
            status: Status::Running,
            events: Vec::new(),
        }
    }

    pub fn turn(&self) -> usize {
        self.turn
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn status(&self) -> Status {
        self.status
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn push_event(&mut self, event: Event) {
        self.events.push(event);
    }

    pub fn set_status(&mut self, status: Status) {
        self.status = status;
    }

    pub fn set_turn(&mut self, turn: usize) {
        self.turn = turn;
        self.events.clear();
    }
}

impl Default for ExecuteContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_context_starts_idle_and_running() {
        let first = ExecuteContext::new();
        let second = ExecuteContext::new();

        assert_eq!(first.turn(), 0);
        assert_eq!(first.status(), Status::Running);
        assert!(first.events().is_empty());
        assert_ne!(first.id(), second.id(), "每次执行都应拿到唯一 ID");
    }

    #[test]
    fn event_records_name_content_role_and_timestamp() {
        let event = Event::new(EventName::Thought, "我先查一下", Role::Assistant);

        assert_eq!(event.name, EventName::Thought);
        assert_eq!(event.content, "我先查一下");
        assert_eq!(event.role, Role::Assistant);
        assert!(event.timestamp > SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn event_serializes_as_flat_json() {
        let timestamp =
            SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_000);
        let event = Event {
            name: EventName::ToolCall,
            content: "{}".to_owned(),
            role: Role::Assistant,
            timestamp,
        };

        let value = serde_json::to_value(&event).expect("事件应能序列化");

        assert_eq!(value["name"], "tool_call");
        assert_eq!(value["content"], "{}");
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["timestamp"], 1_700_000_000_000u64);
    }

    #[test]
    fn set_turn_sets_turn_and_clears_previous_events() {
        let mut context = ExecuteContext::new();
        context.push_event(Event::new(EventName::Thought, "想", Role::Assistant));

        context.set_turn(2);

        assert_eq!(context.turn(), 2);
        assert!(context.events().is_empty(), "只保留当前 turn 的事件");
    }

    #[test]
    fn events_keep_insertion_order() {
        let mut context = ExecuteContext::new();
        context.push_event(Event::new(EventName::Thought, "想", Role::Assistant));
        context.push_event(Event::new(EventName::ToolCall, "{}", Role::Assistant));
        context.push_event(Event::new(EventName::ToolResult, "echo:{}", Role::Tool));
        context.push_event(Event::new(EventName::Answer, "答案", Role::Assistant));

        let observed: Vec<(EventName, &str, Role)> = context
            .events()
            .iter()
            .map(|event| (event.name, event.content.as_str(), event.role))
            .collect();

        assert_eq!(
            observed,
            vec![
                (EventName::Thought, "想", Role::Assistant),
                (EventName::ToolCall, "{}", Role::Assistant),
                (EventName::ToolResult, "echo:{}", Role::Tool),
                (EventName::Answer, "答案", Role::Assistant),
            ]
        );
    }

    #[test]
    fn status_transitions_are_recorded() {
        let mut context = ExecuteContext::new();

        context.set_status(Status::Completed);
        assert_eq!(context.status(), Status::Completed);

        context.set_status(Status::Failed);
        assert_eq!(context.status(), Status::Failed);
    }
}
