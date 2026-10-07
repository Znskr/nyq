//! OpenAI-compatible streaming Chat Completions provider.

use std::collections::BTreeMap;

use anyhow::Result;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};

use super::{sse, EventSink, Provider};
use crate::types::{Item, Request, Response, StreamEvent, Usage};

pub struct OpenAiProvider {
    pub label: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
}

impl OpenAiProvider {
    pub fn new(base_url: String, api_key: Option<String>, model: String) -> Self {
        Self::with_label("openai".into(), base_url, api_key, model)
    }

    pub fn with_label(
        label: String,
        base_url: String,
        api_key: Option<String>,
        model: String,
    ) -> Self {
        Self {
            label,
            base_url,
            api_key,
            model,
        }
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        #[derive(serde::Deserialize)]
        struct Catalog {
            data: Vec<Model>,
        }
        #[derive(serde::Deserialize)]
        struct Model {
            id: String,
        }
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(3))
            .timeout(std::time::Duration::from_secs(5))
            .build()?;
        let mut request = client.get(format!("{}/models", self.base_url.trim_end_matches('/')));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let catalog: Catalog = sse::checked(request.send().await?).await?.json().await?;
        Ok(catalog.data.into_iter().map(|m| m.id).collect())
    }

    fn body(&self, req: Request<'_>) -> Value {
        let mut messages = vec![json!({"role":"system", "content":req.system})];
        let mut assistant: Option<Value> = None;
        for item in req.items {
            if matches!(item, Item::Reasoning { .. }) {
                continue;
            }
            if !matches!(item, Item::Assistant { .. } | Item::ToolCall { .. }) {
                if let Some(message) = assistant.take() {
                    messages.push(message);
                }
            }
            match item {
                Item::User { text, images } if images.is_empty() => {
                    messages.push(json!({"role":"user","content":text}))
                }
                Item::User { text, images } => {
                    let mut content = vec![json!({"type":"text","text":text})];
                    content.extend(
                        images
                            .iter()
                            .map(|i| json!({"type":"image_url","image_url":{"url":i.data_url()}})),
                    );
                    messages.push(json!({"role":"user","content":content}))
                }
                Item::Compaction { summary } => messages
                    .push(json!({"role":"user","content":crate::types::compaction_text(summary)})),
                Item::ToolResult { id, output, .. } => {
                    messages.push(json!({"role":"tool","tool_call_id":id,"content":output}))
                }
                Item::Assistant { text } => {
                    let message =
                        assistant.get_or_insert_with(|| json!({"role":"assistant","content":null}));
                    let mut content = message["content"].as_str().unwrap_or_default().to_owned();
                    content.push_str(text);
                    message["content"] = json!(content);
                }
                Item::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    let message =
                        assistant.get_or_insert_with(|| json!({"role":"assistant","content":null}));
                    if message.get("tool_calls").is_none() {
                        message["tool_calls"] = json!([]);
                    }
                    message["tool_calls"].as_array_mut().unwrap().push(json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments}}));
                }
                Item::Reasoning { .. } => {}
            }
        }
        if let Some(message) = assistant {
            messages.push(message);
        }
        let mut body = json!({"model":self.model,"messages":messages,"stream":true,"stream_options":{"include_usage":true}});
        if !req.tools.is_empty() {
            body["tools"] = json!(req.tools.iter().map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.parameters}})).collect::<Vec<_>>());
        }
        body
    }
}

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
    started: bool,
    position: usize,
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    fn fork(&self) -> Box<dyn Provider> {
        Box::new(Self::with_label(self.label.clone(), self.base_url.clone(), self.api_key.clone(), self.model.clone()))
    }
    fn name(&self) -> &str {
        &self.label
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn set_model(&mut self, model: String) {
        self.model = model;
    }

    async fn stream(&self, req: Request<'_>, on_event: EventSink<'_>) -> Result<Response> {
        let client = reqwest::Client::new();
        let mut request = client
            .post(format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            ))
            .json(&self.body(req));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let mut events = sse::checked(request.send().await?)
            .await?
            .bytes_stream()
            .eventsource();
        let mut response = Response::default();
        let mut calls = BTreeMap::<u64, Call>::new();
        let mut completed = false;
        while let Some(event) = events.next().await {
            let event = event?;
            if event.data.trim() == "[DONE]" {
                completed = true;
                break;
            }
            let data: Value = serde_json::from_str(&event.data)?;
            if data.get("error").is_some() {
                anyhow::bail!("{}", sse::failure(&data));
            }
            if let Some(usage) = data.get("usage").filter(|v| !v.is_null()) {
                response.usage = Some(Usage {
                    input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
                    output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
                    cached_tokens: usage["prompt_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .unwrap_or(0),
                });
            }
            // A request asks for one choice; ignore any unsolicited alternatives.
            let Some(choice) = data["choices"]
                .as_array()
                .and_then(|cs| cs.iter().find(|c| c["index"].as_u64().unwrap_or(0) == 0))
            else {
                continue;
            };
            if let Some(reason) = choice.get("finish_reason").filter(|v| !v.is_null()) {
                match reason.as_str() {
                    Some("stop" | "tool_calls" | "function_call") => completed = true,
                    Some(other) => anyhow::bail!("OpenAI stream stopped early: finish_reason {other}"),
                    None => anyhow::bail!("OpenAI stream stopped early: finish_reason {reason}"),
                }
            }
            let delta = &choice["delta"];
            for (key, reasoning) in [
                ("reasoning_content", true),
                ("reasoning", true),
                ("content", false),
                ("refusal", false),
            ] {
                if let Some(text) = delta[key].as_str().filter(|s| !s.is_empty()) {
                    on_event(if reasoning {
                        StreamEvent::Reasoning(text.into())
                    } else {
                        StreamEvent::Text(text.into())
                    });
                    match (response.items.last_mut(), reasoning) {
                        (Some(Item::Reasoning { summary, .. }), true) => summary.push_str(text),
                        (Some(Item::Assistant { text: content }), false) => content.push_str(text),
                        (_, true) => response.items.push(Item::Reasoning {
                            id: None,
                            summary: text.into(),
                            encrypted: None,
                        }),
                        (_, false) => response.items.push(Item::Assistant { text: text.into() }),
                    }
                }
            }
            if let Some(deltas) = delta["tool_calls"].as_array() {
                for delta in deltas {
                    let index = delta["index"].as_u64().unwrap_or(0);
                    let call = calls.entry(index).or_insert_with(|| {
                        let position = response.items.len();
                        response.items.push(Item::ToolCall {
                            id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                        });
                        Call {
                            position,
                            ..Call::default()
                        }
                    });
                    if let Some(id) = delta["id"].as_str() {
                        call.id.push_str(id);
                    }
                    if let Some(name) = delta["function"]["name"].as_str() {
                        call.name.push_str(name);
                    }
                    if let Some(args) = delta["function"]["arguments"].as_str() {
                        call.arguments.push_str(args);
                    }
                    if !call.started && !call.id.is_empty() && !call.name.is_empty() {
                        on_event(StreamEvent::ToolCallStart {
                            id: call.id.clone(),
                            name: call.name.clone(),
                        });
                        call.started = true;
                    }
                }
            }
            if let Some(function) = delta.get("function_call").filter(|v| !v.is_null()) {
                // Legacy completions put the call here instead of `tool_calls`.
                // `u64::MAX` does not collide with a `tool_calls` index.
                let call = calls.entry(u64::MAX).or_insert_with(|| {
                    let position = response.items.len();
                    response.items.push(Item::ToolCall {
                        id: "legacy-0".into(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                    Call { id: "legacy-0".into(), position, ..Call::default() }
                });
                if let Some(name) = function["name"].as_str() {
                    call.name.push_str(name);
                }
                if let Some(args) = function["arguments"].as_str() {
                    call.arguments.push_str(args);
                }
                if !call.started && !call.name.is_empty() {
                    on_event(StreamEvent::ToolCallStart { id: call.id.clone(), name: call.name.clone() });
                    call.started = true;
                }
            }
        }
        for call in calls.into_values() {
            response.items[call.position] = Item::ToolCall {
                id: call.id,
                name: call.name,
                arguments: call.arguments,
            };
        }
        anyhow::ensure!(completed, "OpenAI stream ended before completion");
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        provider::sse::tests::{events, server},
        types::ToolSpec,
    };

    #[tokio::test]
    async fn models_endpoint() {
        let (url, mut requests, task) = server(vec![(
            200,
            json!({"data":[{"id":"a"},{"id":"b"}]}).to_string(),
        )])
        .await;
        let provider =
            OpenAiProvider::with_label("ollama".into(), url, Some("test-key".into()), "a".into());
        assert_eq!(provider.name(), "ollama");
        assert_eq!(provider.list_models().await.unwrap(), vec!["a", "b"]);
        let request = requests.recv().await.unwrap().to_lowercase();
        assert!(request.starts_with("get /models "));
        assert!(request.contains("authorization: bearer test-key"));
        task.await.unwrap();
    }

    #[test]
    fn request_body() {
        let provider = OpenAiProvider::new("unused".into(), None, "test".into());
        let items = vec![
            Item::user("question"),
            Item::Reasoning {
                id: None,
                summary: "skip".into(),
                encrypted: None,
            },
            Item::Assistant {
                text: "working".into(),
            },
            Item::ToolCall {
                id: "a".into(),
                name: "one".into(),
                arguments: "{}".into(),
            },
            Item::ToolCall {
                id: "b".into(),
                name: "two".into(),
                arguments: "{}".into(),
            },
            Item::ToolResult {
                id: "a".into(),
                output: "done".into(),
                is_error: false,
            },
        ];
        let tools = vec![ToolSpec {
            name: "one".into(),
            description: "desc".into(),
            parameters: json!({"type":"object"}),
        }];
        let body = provider.body(Request {
            system: "sys",
            items: &items,
            tools: &tools,
        });
        assert_eq!(body["messages"].as_array().unwrap().len(), 4);
        assert_eq!(
            body["messages"][0],
            json!({"role":"system","content":"sys"})
        );
        assert_eq!(body["messages"][2]["content"], "working");
        assert_eq!(
            body["messages"][2]["tool_calls"].as_array().unwrap().len(),
            2
        );
        assert_eq!(body["messages"][3]["tool_call_id"], "a");
        assert_eq!(
            body["tools"][0]["function"]["parameters"],
            tools[0].parameters
        );
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert!(provider
            .body(Request {
                system: "",
                items: &[],
                tools: &[]
            })
            .get("tools")
            .is_none());
    }

    #[tokio::test]
    async fn streaming() {
        let body = events(&[
            json!({"choices":[{"index":0,"delta":{"reasoning_content":"think"}}]}),
            json!({"choices":[{"index":0,"delta":{"reasoning":" more"}}]}),
            json!({"choices":[{"index":0,"delta":{"content":"hé"}}]}),
            json!({"choices":[{"index":0,"delta":{"content":"llo","tool_calls":[{"index":0,"id":"a","function":{"name":"one","arguments":"{"}},{"index":1,"id":"b","function":{"name":"two","arguments":"{}"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]}}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":3}}}),
        ]) + "data: [DONE]\n\n";
        let (url, mut requests, task) = server(vec![(200, body)]).await;
        let provider = OpenAiProvider::new(url, Some("test-key".into()), "test".into());
        let mut received = Vec::new();
        let response = provider
            .stream(
                Request {
                    system: "sys",
                    items: &[],
                    tools: &[],
                },
                &mut |e| received.push(e),
            )
            .await
            .unwrap();
        assert_eq!(
            response.items,
            vec![
                Item::Reasoning {
                    id: None,
                    summary: "think more".into(),
                    encrypted: None
                },
                Item::Assistant {
                    text: "héllo".into()
                },
                Item::ToolCall {
                    id: "a".into(),
                    name: "one".into(),
                    arguments: "{}".into()
                },
                Item::ToolCall {
                    id: "b".into(),
                    name: "two".into(),
                    arguments: "{}".into()
                }
            ]
        );
        assert_eq!(
            response.usage,
            Some(Usage {
                input_tokens: 10,
                output_tokens: 4,
                cached_tokens: 3
            })
        );
        assert_eq!(received.len(), 6);
        let request = requests.recv().await.unwrap();
        assert!(request.starts_with("POST /chat/completions"));
        assert!(request
            .to_lowercase()
            .contains("authorization: bearer test-key"));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn http_error() {
        let (url, _requests, task) = server(vec![(429, "usage limit reached".into())]).await;
        let provider = OpenAiProvider::new(url, None, "test".into());
        let error = provider
            .stream(
                Request {
                    system: "",
                    items: &[],
                    tools: &[],
                },
                &mut |_| {},
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("429") && error.contains("usage limit reached"));
        task.await.unwrap();
    }

    async fn one_stream(body: String) -> anyhow::Result<Response> {
        let (url, _requests, task) = server(vec![(200, body)]).await;
        let provider = OpenAiProvider::new(url, None, "test".into());
        let result = provider.stream(Request { system: "", items: &[], tools: &[] }, &mut |_| {}).await;
        task.await.unwrap();
        result
    }

    #[tokio::test]
    async fn stream_without_completion_is_an_error() {
        let body = events(&[json!({"choices":[{"index":0,"delta":{"content":"hi"}}]})]);
        let err = one_stream(body).await.unwrap_err();
        assert!(err.to_string().contains("before completion"), "{err}");
    }

    #[tokio::test]
    async fn finish_reason_completes_without_done() {
        let body = events(&[json!({"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]})]);
        let response = one_stream(body).await.unwrap();
        assert_eq!(response.items, vec![Item::Assistant { text: "hi".into() }]);
    }

    #[tokio::test]
    async fn incomplete_finish_reasons_are_errors() {
        for reason in ["length", "content_filter", "tool_calls", "function_call"] {
            let body = events(&[json!({"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason": reason}]})]);
            let result = one_stream(body).await;
            if reason == "tool_calls" || reason == "function_call" {
                assert!(result.is_ok(), "{reason}: {result:?}");
            } else {
                let err = result.unwrap_err().to_string();
                assert!(err.contains(reason), "{err}");
            }
        }
    }

    #[tokio::test]
    async fn legacy_function_call_and_refusal_are_kept() {
        let body = events(&[
            json!({"choices":[{"index":0,"delta":{"function_call":{"name":"get_weather","arguments":"{\"city\":"}},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"function_call":{"arguments":"\"Paris\"}"}},"finish_reason":"function_call"}]}),
        ]);
        let response = one_stream(body).await.unwrap();
        assert_eq!(
            response.items,
            vec![Item::ToolCall { id: "legacy-0".into(), name: "get_weather".into(), arguments: "{\"city\":\"Paris\"}".into() }]
        );
        let body = events(&[json!({"choices":[{"index":0,"delta":{"refusal":"I can't help with that."},"finish_reason":"stop"}]})]);
        let response = one_stream(body).await.unwrap();
        assert_eq!(response.items, vec![Item::Assistant { text: "I can't help with that.".into() }]);
    }
}
