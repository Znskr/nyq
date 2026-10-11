//! nyq's side of an MCP connection: the capabilities it announces and how it
//! answers server requests (elicitation) and notifications (progress, list
//! changes, elicitation completion).

use std::time::Duration;

use rmcp::model::{
    ClientCapabilities, ClientConfig, CustomNotification, ElicitRequestParams, ElicitResult, ElicitationAction,
    ElicitationCapability, ErrorData, FormElicitationCapability, Implementation, ProgressNotificationParam,
    UrlElicitationCapability,
};
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::{ClientHandler, RoleClient};
use serde_json::{Map, Value};

use crate::mcp::{Refresh, WeakManager};

/// How long a URL elicitation may take before nyq stops waiting for the
/// server's completion notice.
const URL_TIMEOUT: Duration = Duration::from_secs(600);

/// The user's answer to an elicitation.
#[derive(Debug, PartialEq)]
pub enum Elicited {
    Accept(Value),
    Decline,
    Cancel,
}

/// Puts a server's elicitation to the user (or to the editor, over ACP).
#[async_trait::async_trait]
pub trait Elicitor: Send + Sync {
    /// Fill in the form `schema` describes; Accept carries the answers.
    async fn form(&self, server: &str, message: &str, schema: &Value) -> Elicited;
    /// Send the user to `url`; Accept once they agreed to go.
    async fn url(&self, server: &str, message: &str, url: &str, id: &str) -> Elicited;
}

/// Asks in the interactive terminal; declines when there is none (print mode).
pub struct TerminalElicitor;

#[async_trait::async_trait]
impl Elicitor for TerminalElicitor {
    async fn form(&self, server: &str, message: &str, schema: &Value) -> Elicited {
        if crate::ui::tui::active().is_none() {
            return Elicited::Decline;
        }
        fill_form(server, message, schema, |q, o| async move {
            crate::ui::tui::ask_active(&q, &o).await.flatten()
        })
        .await
    }

    async fn url(&self, server: &str, message: &str, url: &str, _id: &str) -> Elicited {
        let question = format!("MCP server {server} asks you to open a page in your browser:\n{message}\n\n{url}");
        let options = ["Open in browser".to_string(), "Decline".to_string()];
        match crate::ui::tui::ask_active(&question, &options).await {
            None => Elicited::Decline,
            Some(None) => Elicited::Cancel,
            Some(Some(a)) if a == options[0] => {
                crate::ui::info(&format!("Opening {url} (nyq waits up to 10 minutes for {server} to finish)"));
                crate::mcp::open_browser(url);
                Elicited::Accept(Value::Object(Map::new()))
            }
            Some(Some(_)) => Elicited::Decline,
        }
    }
}

/// One form field: its question and, for choices, label → value pairs.
fn choices(prop: &Value) -> Vec<(String, Value)> {
    let values = |v: &Value| v.as_array().cloned().unwrap_or_default();
    if let Some(one_of) = prop.get("oneOf").or_else(|| prop.get("anyOf")).and_then(Value::as_array) {
        let titled: Vec<(String, Value)> = one_of
            .iter()
            .filter_map(|o| {
                let c = o.get("const")?;
                let label = o.get("title").and_then(Value::as_str).map(String::from).unwrap_or_else(|| label_of(c));
                Some((label, c.clone()))
            })
            .collect();
        if !titled.is_empty() {
            return titled;
        }
    }
    if let Some(e) = prop.get("enum") {
        let names = values(prop.get("enumNames").unwrap_or(&Value::Null));
        return values(e)
            .into_iter()
            .enumerate()
            .map(|(i, v)| (names.get(i).and_then(Value::as_str).map(String::from).unwrap_or_else(|| label_of(&v)), v))
            .collect();
    }
    if prop.get("type").and_then(Value::as_str) == Some("boolean") {
        return vec![("yes".into(), Value::Bool(true)), ("no".into(), Value::Bool(false))];
    }
    Vec::new()
}

fn label_of(v: &Value) -> String {
    v.as_str().map(String::from).unwrap_or_else(|| v.to_string())
}

/// Turn a typed or chosen answer into the JSON the field's schema asks for.
fn coerce(prop: &Value, options: &[(String, Value)], text: &str) -> Result<Value, String> {
    let text = text.trim();
    let value = if let Some((_, v)) = options.iter().find(|(l, _)| l == text) {
        v.clone()
    } else {
        match prop.get("type").and_then(Value::as_str) {
            Some("boolean") => match text.to_ascii_lowercase().as_str() {
                "y" | "yes" | "true" | "1" => Value::Bool(true),
                "n" | "no" | "false" | "0" => Value::Bool(false),
                _ => return Err("Please answer yes or no.".into()),
            },
            Some("integer") => text.parse::<i64>().map(Value::from).map_err(|_| "Please enter a whole number.".to_string())?,
            Some("number") => {
                let n: f64 = text.parse().map_err(|_| "Please enter a number.".to_string())?;
                serde_json::Number::from_f64(n).map(Value::Number).ok_or("Please enter a finite number.")?
            }
            Some("array") => Value::Array(
                text.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|s| Value::String(s.into())).collect(),
            ),
            _ if !options.is_empty() => {
                let labels: Vec<&str> = options.iter().map(|(l, _)| l.as_str()).collect();
                return Err(format!("Please choose one of: {}.", labels.join(", ")));
            }
            _ => Value::String(text.to_string()),
        }
    };
    match crate::mcp_schema::check(prop, &value).first() {
        Some(problem) => Err(format!("Not accepted: {}.", problem.replace("arguments", "the answer"))),
        None => Ok(value),
    }
}

/// Ask for each field of a form elicitation in turn. Skipping a required
/// field cancels; three invalid answers to one field decline.
pub async fn fill_form<F, Fut>(server: &str, message: &str, schema: &Value, mut ask: F) -> Elicited
where
    F: FnMut(String, Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let props = schema.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
    let required: Vec<&str> = schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let intro = format!("MCP server {server} asks: {message}");
    if props.is_empty() {
        return match ask(intro, vec!["Accept".into(), "Decline".into()]).await.as_deref() {
            Some("Accept") => Elicited::Accept(Value::Object(Map::new())),
            Some(_) => Elicited::Decline,
            None => Elicited::Cancel,
        };
    }
    let mut content = Map::new();
    for (name, prop) in &props {
        let is_required = required.contains(&name.as_str());
        let title = prop.get("title").and_then(Value::as_str).unwrap_or(name);
        let mut question = format!("{intro}\n\n{title}{}", if is_required { " (required)" } else { " (optional; Esc to skip)" });
        if let Some(d) = prop.get("description").and_then(Value::as_str).filter(|d| !d.is_empty()) {
            question.push_str(&format!("\n{d}"));
        }
        let options = choices(prop);
        let mut labels: Vec<String> = options.iter().map(|(l, _)| l.clone()).collect();
        if let Some(default) = prop.get("default").filter(|_| labels.is_empty()) {
            labels.push(label_of(default));
        }
        let mut prompt = question.clone();
        let mut answered = false;
        for _ in 0..3 {
            match ask(prompt.clone(), labels.clone()).await {
                None if is_required => return Elicited::Cancel,
                None => {
                    answered = true;
                    break;
                }
                Some(text) => match coerce(prop, &options, &text) {
                    Ok(v) => {
                        content.insert(name.clone(), v);
                        answered = true;
                        break;
                    }
                    Err(why) => prompt = format!("{why}\n\n{question}"),
                },
            }
        }
        if !answered {
            return Elicited::Decline;
        }
    }
    let content = Value::Object(content);
    if !crate::mcp_schema::check(schema, &content).is_empty() {
        return Elicited::Decline;
    }
    Elicited::Accept(content)
}

/// The `ClientHandler` of one server connection.
pub struct Client {
    pub server: String,
    pub mcp: WeakManager,
}

impl Client {
    fn result(answer: Elicited) -> ElicitResult {
        match answer {
            Elicited::Accept(v) => ElicitResult::new(ElicitationAction::Accept).with_content(v),
            Elicited::Decline => ElicitResult::new(ElicitationAction::Decline),
            Elicited::Cancel => ElicitResult::new(ElicitationAction::Cancel),
        }
    }
}

pub fn client_info() -> ClientConfig {
    let mut caps = ClientCapabilities::default();
    caps.elicitation = Some(
        ElicitationCapability::new()
            .with_form(FormElicitationCapability::new().with_schema_validation(true))
            .with_url(UrlElicitationCapability::new()),
    );
    ClientConfig::new(caps, Implementation::new("nyq", env!("CARGO_PKG_VERSION")))
}

impl ClientHandler for Client {
    fn get_info(&self) -> ClientConfig {
        client_info()
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        let Some(mcp) = self.mcp.upgrade() else { return Ok(Self::result(Elicited::Cancel)) };
        let elicitor = mcp.elicitor();
        Ok(Self::result(match request {
            ElicitRequestParams::FormElicitationParams { message, requested_schema, .. } => {
                let schema = serde_json::to_value(&requested_schema).unwrap_or_default();
                elicitor.form(&self.server, &message, &schema).await
            }
            ElicitRequestParams::UrlElicitationParams { message, url, elicitation_id, .. } => {
                let waiter = mcp.expect_elicitation(&elicitation_id);
                match elicitor.url(&self.server, &message, &url, &elicitation_id).await {
                    Elicited::Accept(v) => match tokio::time::timeout(URL_TIMEOUT, waiter).await {
                        Ok(_) => Elicited::Accept(v),
                        Err(_) => {
                            mcp.complete_elicitation(&elicitation_id);
                            Elicited::Cancel
                        }
                    },
                    other => {
                        mcp.complete_elicitation(&elicitation_id);
                        other
                    }
                }
            }
            #[allow(unreachable_patterns)]
            _ => Elicited::Decline,
        }))
    }

    async fn on_progress(&self, params: ProgressNotificationParam, _context: NotificationContext<RoleClient>) {
        if let Some(mcp) = self.mcp.upgrade() {
            mcp.progress(&self.server, &params);
        }
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.refresh(Refresh::Tools);
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.refresh(Refresh::Prompts);
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.refresh(Refresh::Resources);
    }

    async fn on_custom_notification(&self, notification: CustomNotification, _context: NotificationContext<RoleClient>) {
        if notification.method != "notifications/elicitation/complete" {
            return;
        }
        let id = notification.params.as_ref().and_then(|p| p.get("elicitationId")).and_then(Value::as_str);
        if let (Some(mcp), Some(id)) = (self.mcp.upgrade(), id) {
            mcp.complete_elicitation(id);
        }
    }
}

impl Client {
    /// Re-list in the background: the handler must not block the connection.
    fn refresh(&self, what: Refresh) {
        let Some(mcp) = self.mcp.upgrade() else { return };
        let server = self.server.clone();
        tokio::spawn(async move { mcp.refresh(&server, what).await });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn run(schema: Value, answers: &[Option<&str>]) -> (Elicited, Vec<(String, Vec<String>)>) {
        let answers = std::sync::Mutex::new(answers.iter().map(|a| a.map(String::from)).collect::<std::collections::VecDeque<_>>());
        let asked = std::sync::Mutex::new(Vec::new());
        let out = fill_form("srv", "Need details", &schema, |q, o| {
            asked.lock().unwrap().push((q, o));
            let a = answers.lock().unwrap().pop_front().flatten();
            async move { a }
        })
        .await;
        (out, asked.into_inner().unwrap())
    }

    #[tokio::test]
    async fn forms_are_filled_field_by_field() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "title": "Your name", "minLength": 2},
                "age": {"type": "integer", "minimum": 0},
                "color": {"type": "string", "enum": ["r", "g"], "enumNames": ["Red", "Green"]},
                "subscribe": {"type": "boolean"},
                "note": {"type": "string", "default": "none"}
            },
            "required": ["name", "age"]
        });
        let (out, asked) = run(schema.clone(), &[Some("x"), Some("Ada"), Some("old"), Some("36"), Some("Green"), Some("yes"), None]).await;
        assert_eq!(out, Elicited::Accept(json!({"name": "Ada", "age": 36, "color": "g", "subscribe": true})));
        assert!(asked[0].0.starts_with("MCP server srv asks: Need details\n\nYour name (required)"), "{}", asked[0].0);
        assert!(asked[1].0.starts_with("Not accepted: the answer must be at least 2 characters."), "{}", asked[1].0);
        assert!(asked[3].0.starts_with("Please enter a whole number."));
        assert_eq!(asked[4].1, ["Red", "Green"]);
        assert_eq!(asked[5].1, ["yes", "no"]);
        assert_eq!(asked[6].1, ["none"], "a default is offered as the choice");

        // Skipping a required field cancels; repeated bad answers decline.
        assert_eq!(run(schema.clone(), &[None]).await.0, Elicited::Cancel);
        assert_eq!(run(schema, &[Some("Ada"), Some("a"), Some("b"), Some("c")]).await.0, Elicited::Decline);
        // A form without fields is a plain yes/no.
        assert_eq!(run(json!({"type": "object"}), &[Some("Accept")]).await.0, Elicited::Accept(json!({})));
        assert_eq!(run(json!({"type": "object"}), &[Some("Decline")]).await.0, Elicited::Decline);
    }

    #[test]
    fn capabilities_announce_elicitation() {
        let v = serde_json::to_value(client_info()).unwrap();
        assert_eq!(v["capabilities"]["elicitation"], json!({"form": {"schemaValidation": true}, "url": {}}));
        assert_eq!(v["clientInfo"]["name"], "nyq");
    }
}
