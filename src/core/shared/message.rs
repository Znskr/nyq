use super::types::{ImageAttachment, PersistedToolStatus, ToolCall, ToolChoice, ToolResultMemory};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Content {
    Text(Vec<u8>),
}
impl Content {
    pub fn as_text(&self) -> &[u8] {
        match self {
            Self::Text(v) => v,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Option<Content>,
    pub images: Vec<ImageAttachment>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<Vec<u8>>,
    pub tool_name: Option<Vec<u8>>,
    pub tool_result_status: Option<PersistedToolStatus>,
    pub tool_result_memory: Option<ToolResultMemory>,
    pub permission_feedback: bool,
    pub owns_content: bool,
    pub owns_tool_calls: bool,
}
impl Message {
    fn base(role: Role) -> Self {
        Self {
            role,
            content: None,
            images: vec![],
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
            tool_result_status: None,
            tool_result_memory: None,
            permission_feedback: false,
            owns_content: false,
            owns_tool_calls: false,
        }
    }
    pub fn system_text(text: &[u8]) -> Self {
        let mut m = Self::base(Role::System);
        m.content = Some(Content::Text(text.to_vec()));
        m
    }
    pub fn system_owned(text: Vec<u8>) -> Self {
        let mut m = Self::base(Role::System);
        m.content = Some(Content::Text(text));
        m.owns_content = true;
        m
    }
    pub fn user_text(text: &[u8]) -> Self {
        let mut m = Self::base(Role::User);
        m.content = Some(Content::Text(text.to_vec()));
        m
    }
    pub fn user_owned(text: Vec<u8>) -> Self {
        let mut m = Self::base(Role::User);
        m.content = Some(Content::Text(text));
        m.owns_content = true;
        m
    }
    pub fn assistant_owned(content: Option<Vec<u8>>, calls: Vec<ToolCall>) -> Self {
        let mut m = Self::base(Role::Assistant);
        m.owns_content = content.is_some();
        m.owns_tool_calls = !calls.is_empty();
        m.content = content.map(Content::Text);
        m.tool_calls = calls;
        m
    }
    pub fn assistant_borrowed(content: Option<&[u8]>, calls: &[ToolCall]) -> Self {
        let mut m = Self::base(Role::Assistant);
        m.content = content.map(|x| Content::Text(x.to_vec()));
        m.tool_calls = calls.to_vec();
        m
    }
    pub fn tool_owned(
        content: Vec<u8>,
        id: &[u8],
        name: &[u8],
        status: PersistedToolStatus,
    ) -> Self {
        let mut m = Self::base(Role::Tool);
        m.content = Some(Content::Text(content));
        m.tool_call_id = Some(id.to_vec());
        m.tool_name = Some(name.to_vec());
        m.tool_result_status = Some(status);
        m.owns_content = true;
        m
    }
    pub fn deinit(&mut self) {
        *self = Self::base(Role::User)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    pub messages: Vec<Message>,
    pub tool_choice: ToolChoice,
}
impl Request {
    pub fn init(messages: &[Message]) -> Self {
        Self {
            messages: messages.to_vec(),
            tool_choice: ToolChoice::Auto,
        }
    }
    pub fn init_with_tool_choice(messages: &[Message], tool_choice: ToolChoice) -> Self {
        Self {
            messages: messages.to_vec(),
            tool_choice,
        }
    }
}
pub fn free_tool_calls(_: Vec<ToolCall>) {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn constructors() {
        let m = Message::system_text(b"rules");
        assert_eq!(m.role, Role::System);
        assert_eq!(m.content.unwrap().as_text(), b"rules");
        let r = Request::init(&[Message::user_text(b"hi")]);
        assert_eq!(r.messages.len(), 1)
    }
}
