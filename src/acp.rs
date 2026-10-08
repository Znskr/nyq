//! `nyq acp`: an Agent Client Protocol agent over stdin/stdout, for editors
//! such as Zed. Each ACP session is a full nyq [`Agent`] (provider, built-in
//! tools, skills, MCP servers, sandbox, saved session); the editor sees the
//! streamed answer, reasoning and tool calls, and answers approvals through
//! `session/request_permission`.
//!
//! stdout carries JSON-RPC only: everything else goes to stderr.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol as sdk;
use sdk::schema::v1 as acp;
use sdk::schema::ProtocolVersion;
use sdk::{Client, ConnectTo, ConnectionTo, Responder};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, Approval, ApprovalRequest, Approver, TurnEnd, Ui};
use crate::mcp::{McpManager, Scope, ServerConfig, Transport};
use crate::models::ModelChoice;
use crate::provider::Provider;
use crate::session::Session;
use crate::tools::{Registry, ToolOutput};
use crate::types::{Image, Item, StreamEvent, Usage};
use crate::{config, mcp, models, prompt, sandbox, session, skills};

/// The global flags `nyq acp` honors.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub yolo: bool,
    pub sandbox: Option<sandbox::Mode>,
    pub allow_network: bool,
    pub model: Option<String>,
    pub provider: Option<String>,
}

const MODE_ASK: &str = "ask";
const MODE_YOLO: &str = "yolo";
const MODEL_OPTION: &str = "model";
/// Tool output shown in the editor is cut to about this many characters.
const MAX_SHOWN: usize = 8_000;
/// Files larger than this are not read back to build an edit diff.
const MAX_DIFF_FILE: u64 = 1024 * 1024;

type ProviderFactory = Arc<dyn Fn() -> anyhow::Result<Box<dyn Provider>> + Send + Sync>;

/// Run the ACP agent on stdin/stdout until the editor closes the pipe.
pub async fn run(opts: Options) -> anyhow::Result<()> {
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
    let home = config::home();
    let policy = sandbox_policy(&opts)?;
    let factory: ProviderFactory = {
        let (home, provider, model) = (home.clone(), opts.provider.clone(), opts.model.clone());
        Arc::new(move || config::provider(&home, provider.as_deref(), model.as_deref()))
    };
    let state = State::new(home, opts, policy, factory, true);
    let transport = sdk::ByteStreams::new(tokio::io::stdout().compat_write(), tokio::io::stdin().compat());
    serve(state, transport).await
}

/// The requested sandbox, or workspace-write where supported (else off, with
/// a warning on stderr). Mirrors the interactive mode.
fn sandbox_policy(opts: &Options) -> anyhow::Result<sandbox::Policy> {
    let mut policy = sandbox::Policy { mode: opts.sandbox.unwrap_or_default(), network: opts.allow_network, writable: Vec::new() };
    if let Err(e) = sandbox::check(&policy) {
        if opts.sandbox.is_some() {
            anyhow::bail!("sandbox unavailable: {e} (use --sandbox off to run commands unconfined)");
        }
        eprintln!("nyq: sandbox unavailable ({e}); shell commands will run unconfined");
        policy.mode = sandbox::Mode::Off;
    }
    Ok(policy)
}

struct State {
    home: PathBuf,
    opts: Options,
    policy: sandbox::Policy,
    make_provider: ProviderFactory,
    /// Whether to query providers for the model picker (off in tests).
    list_models: bool,
    catalog: tokio::sync::OnceCell<Vec<ModelChoice>>,
    sessions: Mutex<HashMap<String, Arc<Live>>>,
}

/// One open ACP session.
struct Live {
    id: acp::SessionId,
    cwd: PathBuf,
    inner: tokio::sync::Mutex<Inner>,
    /// Tokens of prompts running or waiting for the session; `session/cancel` fires them all.
    cancels: Mutex<Vec<(u64, CancellationToken)>>,
    /// The "yolo" session mode: approve everything without asking.
    yolo: Arc<AtomicBool>,
    /// The model picked while a turn was running; swapped in before the next turn.
    next_provider: Mutex<Option<(Box<dyn Provider>, u64)>>,
    /// `provider:model` as shown in the model picker.
    model: Mutex<String>,
}

struct Inner {
    agent: Agent,
    mcp: McpManager,
}

impl State {
    fn new(home: PathBuf, opts: Options, policy: sandbox::Policy, make_provider: ProviderFactory, list_models: bool) -> Arc<Self> {
        Arc::new(Self {
            home,
            opts,
            policy,
            make_provider,
            list_models,
            catalog: tokio::sync::OnceCell::new(),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    fn sessions_dir(&self) -> PathBuf {
        self.home.join("sessions")
    }

    fn get(&self, id: &acp::SessionId) -> Result<Arc<Live>, sdk::Error> {
        lock(&self.sessions)
            .get(&*id.0)
            .cloned()
            .ok_or_else(|| sdk::Error::invalid_params().data(format!("unknown session {id}")))
    }

    async fn catalog(&self) -> &[ModelChoice] {
        self.catalog
            .get_or_init(|| async {
                if !self.list_models {
                    return Vec::new();
                }
                let (choices, warnings) = models::list(&self.home).await;
                for w in warnings {
                    eprintln!("nyq: model list: {w}");
                }
                choices
            })
            .await
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn internal(e: impl std::fmt::Display) -> sdk::Error {
    sdk::Error::new(-32603, e.to_string())
}

fn anyhow_error(e: anyhow::Error) -> sdk::Error {
    internal(format!("{e:#}"))
}

// ------------------------------------------------------------------ server

async fn serve(state: Arc<State>, transport: impl ConnectTo<sdk::Agent>) -> anyhow::Result<()> {
    let (s1, s2, s3, s4, s5, s6, s7, s8, s9) =
        (state.clone(), state.clone(), state.clone(), state.clone(), state.clone(), state.clone(), state.clone(), state.clone(), state.clone());
    let result = sdk::Agent
        .builder()
        .name("nyq")
        .on_receive_request(
            async move |req: acp::InitializeRequest, responder: Responder<acp::InitializeResponse>, _cx: ConnectionTo<Client>| {
                if s1.list_models {
                    // Warm the model catalog so the first session can offer it soon.
                    let st = s1.clone();
                    tokio::spawn(async move {
                        st.catalog().await;
                    });
                }
                responder.respond(initialize(req))
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::NewSessionRequest, responder: Responder<acp::NewSessionResponse>, cx: ConnectionTo<Client>| {
                let st = s2.clone();
                tokio::spawn(async move {
                    let r = new_session(&st, &cx, req).await;
                    let _ = responder.respond_with_result(r);
                });
                Ok(())
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::LoadSessionRequest, responder: Responder<acp::LoadSessionResponse>, cx: ConnectionTo<Client>| {
                let st = s3.clone();
                tokio::spawn(async move {
                    let r = load_session(&st, &cx, req).await;
                    let _ = responder.respond_with_result(r);
                });
                Ok(())
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::PromptRequest, responder: Responder<acp::PromptResponse>, cx: ConnectionTo<Client>| {
                let st = s4.clone();
                let request_cancel = responder.cancellation();
                tokio::spawn(async move {
                    let r = prompt(&st, &cx, req, request_cancel).await;
                    let _ = responder.respond_with_result(r);
                });
                Ok(())
            },
            sdk::on_receive_request!(),
        )
        .on_receive_notification(
            async move |n: acp::CancelNotification, _cx: ConnectionTo<Client>| {
                if let Ok(live) = s5.get(&n.session_id) {
                    for (_, t) in lock(&live.cancels).iter() {
                        t.cancel();
                    }
                }
                Ok(())
            },
            sdk::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: acp::SetSessionModeRequest, responder: Responder<acp::SetSessionModeResponse>, _cx: ConnectionTo<Client>| {
                responder.respond_with_result(set_mode(&s6, req))
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::SetSessionConfigOptionRequest,
                        responder: Responder<acp::SetSessionConfigOptionResponse>,
                        _cx: ConnectionTo<Client>| {
                let st = s7.clone();
                tokio::spawn(async move {
                    let r = set_config_option(&st, req).await;
                    let _ = responder.respond_with_result(r);
                });
                Ok(())
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::ListSessionsRequest, responder: Responder<acp::ListSessionsResponse>, _cx: ConnectionTo<Client>| {
                responder.respond_with_result(list_sessions(&s8, req))
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::CloseSessionRequest, responder: Responder<acp::CloseSessionResponse>, _cx: ConnectionTo<Client>| {
                let removed = lock(&s9.sessions).remove(&*req.session_id.0);
                tokio::spawn(async move {
                    if let Some(live) = removed {
                        for (_, t) in lock(&live.cancels).iter() {
                            t.cancel();
                        }
                        live.inner.lock().await.mcp.shutdown().await;
                    }
                    let _ = responder.respond(acp::CloseSessionResponse::new());
                });
                Ok(())
            },
            sdk::on_receive_request!(),
        )
        .connect_to(transport)
        .await;
    // The editor went away: stop running turns and MCP servers.
    let open: Vec<Arc<Live>> = lock(&state.sessions).drain().map(|(_, l)| l).collect();
    for live in open {
        for (_, t) in lock(&live.cancels).iter() {
            t.cancel();
        }
        live.inner.lock().await.mcp.shutdown().await;
    }
    result.map_err(|e| anyhow::anyhow!("ACP connection: {e}"))
}

fn initialize(req: acp::InitializeRequest) -> acp::InitializeResponse {
    let version = if req.protocol_version < ProtocolVersion::LATEST { req.protocol_version } else { ProtocolVersion::LATEST };
    acp::InitializeResponse::new(version)
        .agent_info(acp::Implementation::new("nyq", env!("CARGO_PKG_VERSION")).title("nyq".to_string()))
        .agent_capabilities(
            acp::AgentCapabilities::new()
                .load_session(true)
                .prompt_capabilities(acp::PromptCapabilities::new().image(true).embedded_context(true))
                .mcp_capabilities(acp::McpCapabilities::new().http(true))
                .session_capabilities(
                    acp::SessionCapabilities::new().list(acp::SessionListCapabilities::new()).close(acp::SessionCloseCapabilities::new()),
                ),
        )
}

// ---------------------------------------------------------------- sessions

fn check_cwd(cwd: &Path) -> Result<(), sdk::Error> {
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(sdk::Error::invalid_params().data(format!("cwd must be an absolute directory: {}", cwd.display())));
    }
    Ok(())
}

async fn new_session(state: &Arc<State>, cx: &ConnectionTo<Client>, req: acp::NewSessionRequest) -> Result<acp::NewSessionResponse, sdk::Error> {
    check_cwd(&req.cwd)?;
    let provider = (state.make_provider)().map_err(anyhow_error)?;
    let session = Session::create(&state.sessions_dir(), &req.cwd, provider.model()).map_err(anyhow_error)?;
    let live = open(state, req.cwd, provider, session, Vec::new(), &req.mcp_servers).await;
    let (modes, options) = describe(state, &live, cx);
    Ok(acp::NewSessionResponse::new(live.id.clone()).modes(modes).config_options(options))
}

async fn load_session(state: &Arc<State>, cx: &ConnectionTo<Client>, req: acp::LoadSessionRequest) -> Result<acp::LoadSessionResponse, sdk::Error> {
    check_cwd(&req.cwd)?;
    let id = req.session_id.0.to_string();
    let (session, meta, items) = session::load(&state.sessions_dir(), &id, &req.cwd).map_err(anyhow_error)?;
    if meta.id != id {
        return Err(sdk::Error::resource_not_found(Some(id)));
    }
    let old = lock(&state.sessions).remove(&id);
    if let Some(old) = old {
        for (_, t) in lock(&old.cancels).iter() {
            t.cancel();
        }
        old.inner.lock().await.mcp.shutdown().await;
    }
    let provider = (state.make_provider)().map_err(anyhow_error)?;
    let live = open(state, req.cwd, provider, session, items, &req.mcp_servers).await;
    {
        let inner = live.inner.lock().await;
        replay(cx, &live.id, &inner.agent);
    }
    let (modes, options) = describe(state, &live, cx);
    Ok(acp::LoadSessionResponse::new().modes(modes).config_options(options))
}

/// Build the agent for a session, connect MCP servers and register it.
async fn open(
    state: &Arc<State>,
    cwd: PathBuf,
    provider: Box<dyn Provider>,
    session: Session,
    items: Vec<Item>,
    client_servers: &[acp::McpServer],
) -> Arc<Live> {
    let sandbox = Arc::new(state.policy.clone());
    let (permissions, warnings) = crate::permissions::Permissions::load(&state.home, &cwd);
    for w in warnings {
        eprintln!("nyq: {w}");
    }
    let model = format!("{}:{}", provider.name(), provider.model());
    let mut agent = Agent {
        session: Some(session),
        provider,
        tools: Registry::builtin(),
        system: prompt::system(&cwd, &sandbox),
        items: Vec::new(),
        cwd: cwd.clone(),
        yolo: state.opts.yolo,
        allowed: Default::default(),
        permissions: Arc::new(permissions),
        usage: Usage::default(),
            stats: Default::default(),
        context_window: 0,
        context_tokens: 0,
        context_estimated: false,
        suppress_auto_compact: false,
        submitted: 0,
        sandbox,
        undo: Default::default(),
    };
    agent.context_window = config::context_window_for(agent.provider.name(), None);
    let skills = skills::discover(&state.home, &cwd);
    if !skills.is_empty() {
        agent.system.push_str(&format!("\n\n{}", skills::prompt_section(&skills)));
        agent.tools.add(skills::tool(skills));
    }
    if !items.is_empty() {
        agent.items = items;
        agent.adopt_transcript();
    }

    let mut mcp = McpManager::new(state.home.clone());
    let (servers, warnings) = mcp::load_configs(&state.home, &cwd);
    for w in warnings {
        eprintln!("nyq: {w}");
    }
    let mut wanted: Vec<ServerConfig> = servers.into_iter().filter(|s| s.scope == Scope::Profile).collect();
    for server in client_servers {
        match server_config(server, &cwd) {
            Ok(cfg) => {
                // The editor's server replaces a profile server of the same name.
                wanted.retain(|s| s.name != cfg.name);
                wanted.push(cfg);
            }
            Err(e) => eprintln!("nyq: {e}"),
        }
    }
    for cfg in &wanted {
        connect_server(&mut mcp, &mut agent, cfg).await;
    }

    let live = Arc::new(Live {
        id: acp::SessionId::new(agent.session.as_ref().map(|s| s.id.clone()).unwrap_or_default()),
        cwd,
        inner: tokio::sync::Mutex::new(Inner { agent, mcp }),
        cancels: Mutex::new(Vec::new()),
        yolo: Arc::new(AtomicBool::new(state.opts.yolo)),
        next_provider: Mutex::new(None),
        model: Mutex::new(model),
    });
    lock(&state.sessions).insert(live.id.0.to_string(), live.clone());
    live
}

/// An MCP server the editor passed in `session/new`.
fn server_config(server: &acp::McpServer, cwd: &Path) -> Result<ServerConfig, String> {
    let (name, transport) = match server {
        acp::McpServer::Stdio(s) => (
            s.name.clone(),
            Transport::Stdio {
                command: s.command.to_string_lossy().into_owned(),
                args: s.args.clone(),
                env: s.env.iter().map(|e| (e.name.clone(), e.value.clone())).collect::<BTreeMap<_, _>>(),
                cwd: Some(cwd.to_path_buf()),
            },
        ),
        acp::McpServer::Http(s) => (
            s.name.clone(),
            Transport::Http { url: s.url.clone(), headers: s.headers.iter().map(|h| (h.name.clone(), h.value.clone())).collect() },
        ),
        acp::McpServer::Sse(s) => return Err(format!("MCP {}: SSE servers are not supported (use streamable HTTP)", s.name)),
        #[allow(unreachable_patterns)]
        _ => return Err("MCP: unsupported server transport".into()),
    };
    if name.trim().is_empty() {
        return Err("MCP: a server without a name was skipped".into());
    }
    Ok(ServerConfig { name, transport, scope: Scope::Profile })
}

async fn connect_server(mcp: &mut McpManager, agent: &mut Agent, cfg: &ServerConfig) {
    match mcp.connect(cfg).await {
        Ok(tools) => {
            let mut added = Vec::new();
            for tool in tools {
                let name = tool.spec().name.clone();
                if let Err(e) = agent.tools.try_insert(tool) {
                    eprintln!("nyq: MCP {}: {e}", cfg.name);
                    agent.tools.remove_names(&added);
                    agent.tools.remove_prefix(&mcp::tool_prefix(&cfg.name));
                    let _ = mcp.disconnect(&cfg.name).await;
                    return;
                }
                added.push(name);
            }
        }
        Err(e) => eprintln!("nyq: MCP {}: {e:#}", cfg.name),
    }
}

/// The session's modes and model picker. When the model catalog is still
/// loading, the picker starts with the current model and is updated later.
fn describe(state: &Arc<State>, live: &Arc<Live>, cx: &ConnectionTo<Client>) -> (acp::SessionModeState, Vec<acp::SessionConfigOption>) {
    let current = if live.yolo.load(Ordering::Relaxed) { MODE_YOLO } else { MODE_ASK };
    let modes = acp::SessionModeState::new(
        current,
        vec![
            acp::SessionMode::new(MODE_ASK, "Ask").description("Ask before edits, commands and other side effects".to_string()),
            acp::SessionMode::new(MODE_YOLO, "Yolo").description("Run every tool call without asking".to_string()),
        ],
    );
    let options = match state.catalog.get() {
        Some(catalog) => model_options(catalog, &lock(&live.model)),
        None => {
            if state.list_models {
                let (st, live, cx) = (state.clone(), live.clone(), cx.clone());
                tokio::spawn(async move {
                    let catalog = st.catalog().await;
                    let options = model_options(catalog, &lock(&live.model));
                    let update = acp::SessionUpdate::ConfigOptionUpdate(acp::ConfigOptionUpdate::new(options));
                    let _ = cx.send_notification(acp::SessionNotification::new(live.id.clone(), update));
                });
            }
            model_options(&[], &lock(&live.model))
        }
    };
    (modes, options)
}

fn model_options(catalog: &[ModelChoice], current: &str) -> Vec<acp::SessionConfigOption> {
    let mut groups: Vec<(String, Vec<acp::SessionConfigSelectOption>)> = Vec::new();
    let listed = catalog.iter().any(|c| format!("{}:{}", c.provider, c.model) == current);
    let mut add = |provider: &str, model: &str| {
        let value = format!("{provider}:{model}");
        let option = acp::SessionConfigSelectOption::new(value, model.to_string());
        match groups.iter_mut().find(|(p, _)| p == provider) {
            Some((_, options)) => {
                if !options.iter().any(|o| o.value == option.value) {
                    options.push(option)
                }
            }
            None => groups.push((provider.to_string(), vec![option])),
        }
    };
    for c in catalog {
        add(&c.provider, &c.model);
    }
    if !listed {
        if let Some((provider, model)) = current.split_once(':') {
            add(provider, model);
        }
    }
    let groups = groups
        .into_iter()
        .map(|(provider, options)| acp::SessionConfigSelectGroup::new(provider.clone(), provider, options))
        .collect::<Vec<_>>();
    vec![acp::SessionConfigOption::select(MODEL_OPTION, "Model", current.to_string(), groups)
        .category(acp::SessionConfigOptionCategory::Model)]
}

fn set_mode(state: &Arc<State>, req: acp::SetSessionModeRequest) -> Result<acp::SetSessionModeResponse, sdk::Error> {
    let live = state.get(&req.session_id)?;
    let yolo = match &*req.mode_id.0 {
        MODE_ASK => false,
        MODE_YOLO => true,
        other => return Err(sdk::Error::invalid_params().data(format!("unknown mode {other}"))),
    };
    live.yolo.store(yolo, Ordering::Relaxed);
    Ok(acp::SetSessionModeResponse::new())
}

async fn set_config_option(state: &Arc<State>, req: acp::SetSessionConfigOptionRequest) -> Result<acp::SetSessionConfigOptionResponse, sdk::Error> {
    let live = state.get(&req.session_id)?;
    if &*req.config_id.0 != MODEL_OPTION {
        return Err(sdk::Error::invalid_params().data(format!("unknown config option {}", req.config_id.0)));
    }
    let value = req.value.as_value_id().ok_or_else(|| sdk::Error::invalid_params().data("model must be a select value"))?.0.to_string();
    let catalog = if state.list_models { state.catalog().await } else { &[] };
    let choice = match catalog.iter().find(|c| format!("{}:{}", c.provider, c.model) == value) {
        Some(c) => c.clone(),
        None => {
            let (provider, model) =
                value.split_once(':').ok_or_else(|| sdk::Error::invalid_params().data(format!("unknown model {value}")))?;
            ModelChoice { provider: provider.into(), model: model.into(), context_window: None, efforts: Vec::new() }
        }
    };
    let window = config::context_window_for(&choice.provider, choice.context_window);
    // Keep the reasoning effort when the new model supports it.
    match live.inner.try_lock() {
        Ok(mut inner) => {
            let effort = inner.agent.provider.effort().map(str::to_string).filter(|e| choice.efforts.contains(e));
            let provider = models::build(&state.home, &choice, effort.clone()).map_err(anyhow_error)?;
            inner.agent.provider = provider;
            inner.agent.set_context_window(window);
            let _ = models::save_last(&state.home, &choice, effort.as_deref());
        }
        Err(_) => {
            // A turn is running; switch before the next one.
            let provider = models::build(&state.home, &choice, None).map_err(anyhow_error)?;
            *lock(&live.next_provider) = Some((provider, window));
            let _ = models::save_last(&state.home, &choice, None);
        }
    }
    *lock(&live.model) = value.clone();
    Ok(acp::SetSessionConfigOptionResponse::new(model_options(catalog, &value)))
}

fn list_sessions(state: &Arc<State>, req: acp::ListSessionsRequest) -> Result<acp::ListSessionsResponse, sdk::Error> {
    let all = session::list(&state.sessions_dir()).map_err(anyhow_error)?;
    let sessions = all
        .into_iter()
        .filter(|m| req.cwd.as_ref().is_none_or(|cwd| &m.cwd == cwd))
        .map(|m| acp::SessionInfo::new(m.id, m.cwd).title(m.title).updated_at(m.created.to_rfc3339()))
        .collect();
    Ok(acp::ListSessionsResponse::new(sessions))
}

// ------------------------------------------------------------------ prompts

async fn prompt(
    state: &Arc<State>,
    cx: &ConnectionTo<Client>,
    req: acp::PromptRequest,
    request_cancel: sdk::RequestCancellation,
) -> Result<acp::PromptResponse, sdk::Error> {
    let live = state.get(&req.session_id)?;
    let item = user_item(&req.prompt, &live.cwd);
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let token = CancellationToken::new();
    let key = NEXT.fetch_add(1, Ordering::Relaxed);
    lock(&live.cancels).push((key, token.clone()));
    // `$/cancel_request` for this prompt works like `session/cancel`.
    let watcher = {
        let token = token.clone();
        tokio::spawn(async move {
            request_cancel.cancelled().await;
            token.cancel();
        })
    };
    let result = {
        let mut inner = live.inner.lock().await;
        if let Some((provider, window)) = lock(&live.next_provider).take() {
            inner.agent.provider = provider;
            inner.agent.set_context_window(window);
        }
        inner.agent.yolo = live.yolo.load(Ordering::Relaxed);
        let perms = Arc::new(Asker { cx: cx.clone(), session: live.id.clone(), cwd: live.cwd.clone(), yolo: live.yolo.clone() });
        let mut ui = AcpUi::new(perms, inner.agent.context_window);
        let result = if token.is_cancelled() { Ok(TurnEnd::Cancelled) } else { inner.agent.turn(item, &mut ui, token.clone()).await };
        let (used, size) = (inner.agent.context_tokens, inner.agent.context_window);
        if size > 0 {
            ui.notify(acp::SessionUpdate::UsageUpdate(acp::UsageUpdate::new(used, size)));
        }
        result
    };
    watcher.abort();
    lock(&live.cancels).retain(|(k, _)| *k != key);
    match result {
        Ok(TurnEnd::Done) => Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
        Ok(TurnEnd::Cancelled) => Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)),
        Err(_) if token.is_cancelled() => Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)),
        Err(e) => Err(anyhow_error(e)),
    }
}

/// One user message from ACP content blocks. Text and links stay inline;
/// embedded resources follow as `<context>` blocks; images are attached.
fn user_item(blocks: &[acp::ContentBlock], cwd: &Path) -> Item {
    let mut text = String::new();
    let mut context = Vec::new();
    let mut images = Vec::new();
    let mut notes = Vec::new();
    for block in blocks {
        match block {
            acp::ContentBlock::Text(t) => text.push_str(&t.text),
            acp::ContentBlock::ResourceLink(link) => text.push_str(&mention(&link.uri, &link.name)),
            acp::ContentBlock::Resource(r) => match &r.resource {
                acp::EmbeddedResourceResource::TextResourceContents(t) => {
                    text.push_str(&mention(&t.uri, ""));
                    context.push(format!("<context ref=\"{}\">\n{}\n</context>", t.uri, t.text));
                }
                acp::EmbeddedResourceResource::BlobResourceContents(b) => {
                    text.push_str(&mention(&b.uri, ""));
                    match b.mime_type.as_deref().filter(|m| supported_image(m)) {
                        Some(mime) => images.push(Image { media_type: mime.into(), data: b.blob.clone() }),
                        None => notes.push(format!("[{} is binary; its content is not included]", b.uri)),
                    }
                }
                #[allow(unreachable_patterns)]
                _ => {}
            },
            acp::ContentBlock::Image(img) => {
                if !img.data.is_empty() && supported_image(&img.mime_type) {
                    images.push(Image { media_type: img.mime_type.clone(), data: img.data.clone() });
                } else if let Some(path) = img.uri.as_deref().and_then(|u| file_path(u, cwd)) {
                    match crate::tools::image::load(&path) {
                        Ok(image) => images.push(image),
                        Err(e) => notes.push(format!("[image not attached: {e:#}]")),
                    }
                } else {
                    notes.push(format!("[an image of type {} was not attached: unsupported]", img.mime_type));
                }
            }
            acp::ContentBlock::Audio(_) => notes.push("[audio input is not supported]".into()),
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
    for extra in context.into_iter().chain(notes) {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&extra);
    }
    Item::User { text, images }
}

fn supported_image(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp")
}

/// `@/abs/path` for files (so the model can read them), else `[name](uri)`.
fn mention(uri: &str, name: &str) -> String {
    match reqwest::Url::parse(uri).ok().filter(|u| u.scheme() == "file").and_then(|u| u.to_file_path().ok()) {
        Some(path) => format!("@{}", path.display()),
        None if name.is_empty() => format!("<{uri}>"),
        None => format!("[{name}]({uri})"),
    }
}

fn file_path(uri: &str, cwd: &Path) -> Option<PathBuf> {
    match reqwest::Url::parse(uri) {
        Ok(u) if u.scheme() == "file" => u.to_file_path().ok(),
        Ok(_) => None,
        Err(_) => Some(cwd.join(uri)),
    }
}

/// Stream a loaded transcript to the editor (`session/load`).
fn replay(cx: &ConnectionTo<Client>, session: &acp::SessionId, agent: &Agent) {
    let send = |update| {
        let _ = cx.send_notification(acp::SessionNotification::new(session.clone(), update));
    };
    let chunk = |text: &str| acp::ContentChunk::new(text.to_string().into());
    for item in &agent.items {
        match item {
            Item::User { text, .. } => send(acp::SessionUpdate::UserMessageChunk(chunk(text))),
            Item::Assistant { text } => send(acp::SessionUpdate::AgentMessageChunk(chunk(text))),
            Item::Reasoning { summary, .. } if !summary.trim().is_empty() => {
                send(acp::SessionUpdate::AgentThoughtChunk(chunk(summary)))
            }
            Item::Compaction { summary } => {
                send(acp::SessionUpdate::AgentThoughtChunk(chunk(&format!("Earlier conversation, summarized:\n\n{summary}"))))
            }
            Item::ToolCall { id, name, arguments } => {
                let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
                let summary = agent.tools.get(name).map(|t| t.summary(&args)).unwrap_or_default();
                let call = acp::ToolCall::new(id.clone(), title(name, &summary))
                    .kind(tool_kind(name))
                    .status(acp::ToolCallStatus::Completed)
                    .locations(locations(name, &summary, &agent.cwd))
                    .raw_input(args);
                send(acp::SessionUpdate::ToolCall(call))
            }
            Item::ToolResult { id, output, is_error } => {
                let fields = acp::ToolCallUpdateFields::new()
                    .status(if *is_error { acp::ToolCallStatus::Failed } else { acp::ToolCallStatus::Completed })
                    .content(vec![text_content(&fenced(&shorten(output), ""))]);
                send(acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(id.clone(), fields)))
            }
            _ => {}
        }
    }
}

// ----------------------------------------------------------------------- ui

/// Asks the editor for permission (`session/request_permission`). Shared by
/// the session's [`AcpUi`] and, as an [`Approver`], by subagents.
struct Asker {
    cx: ConnectionTo<Client>,
    session: acp::SessionId,
    cwd: PathBuf,
    yolo: Arc<AtomicBool>,
}

impl Asker {
    fn notify(&self, update: acp::SessionUpdate) {
        let _ = self.cx.send_notification(acp::SessionNotification::new(self.session.clone(), update));
    }

    /// Show the call as pending and ask. Returns the decision and the tool call id.
    async fn ask(&self, origin: Option<&str>, req: &ApprovalRequest<'_>) -> (Approval, String) {
        let id = uuid::Uuid::new_v4().to_string();
        let mut name = title(req.tool, req.summary);
        if let Some(origin) = origin {
            name = format!("[{origin}] {name}");
        }
        let mut content = Vec::new();
        if let Some(preview) = req.preview.filter(|p| !p.trim().is_empty()) {
            let lang = if is_diff(preview) { "diff" } else { "" };
            content.push(text_content(&fenced(&shorten(preview), lang)));
        }
        let call = acp::ToolCall::new(id.clone(), name.clone())
            .kind(tool_kind(req.tool))
            .status(acp::ToolCallStatus::Pending)
            .content(content)
            .locations(locations(req.tool, req.summary, &self.cwd));
        self.notify(acp::SessionUpdate::ToolCall(call));
        if self.yolo.load(Ordering::Relaxed) {
            return (Approval::Yes, id);
        }
        let mut options = vec![acp::PermissionOption::new("allow_once", "Allow", acp::PermissionOptionKind::AllowOnce)];
        if let Some(rule) = &req.rule {
            options.push(acp::PermissionOption::new(
                "allow_always",
                format!("Always allow {rule} (this session)"),
                acp::PermissionOptionKind::AllowAlways,
            ));
        }
        options.push(acp::PermissionOption::new("reject_once", "Reject", acp::PermissionOptionKind::RejectOnce));
        let request = acp::RequestPermissionRequest::new(
            self.session.clone(),
            // Repeat the title and kind so clients that don't merge with the
            // earlier tool_call card still show what is being approved.
            acp::ToolCallUpdate::new(id.clone(), acp::ToolCallUpdateFields::new().title(name).kind(tool_kind(req.tool))),
            options,
        );
        let answer = match self.cx.send_request(request).block_task().await {
            Ok(response) => match response.outcome {
                acp::RequestPermissionOutcome::Selected(s) => match &*s.option_id.0 {
                    "allow_once" => Approval::Yes,
                    "allow_always" => Approval::Always,
                    _ => Approval::No,
                },
                _ => Approval::No,
            },
            Err(e) => {
                eprintln!("nyq: permission request failed: {e}");
                Approval::No
            }
        };
        (answer, id)
    }
}

#[async_trait::async_trait]
impl Approver for Asker {
    async fn approve(&self, origin: &str, req: &ApprovalRequest<'_>) -> Approval {
        let (answer, id) = self.ask(Some(origin), req).await;
        // The subagent runs the call itself; just settle the card.
        let status = if matches!(answer, Approval::No) { acp::ToolCallStatus::Failed } else { acp::ToolCallStatus::Completed };
        self.notify(acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(id, acp::ToolCallUpdateFields::new().status(status))));
        answer
    }
}

/// A tool call shown in the editor, between `tool_start` and `tool_end`.
struct Running {
    id: String,
    /// For write/edit: the file and its text before the call, for a diff.
    file: Option<(PathBuf, Option<String>)>,
}

struct AcpUi {
    asker: Arc<Asker>,
    /// Approved (or denied) call whose card is already shown.
    pending: Option<String>,
    running: Option<Running>,
    context_window: u64,
}

impl AcpUi {
    fn new(asker: Arc<Asker>, context_window: u64) -> Self {
        Self { asker, pending: None, running: None, context_window }
    }

    fn notify(&self, update: acp::SessionUpdate) {
        self.asker.notify(update);
    }
}

#[async_trait::async_trait]
impl Ui for AcpUi {
    fn stream(&mut self, event: &StreamEvent) {
        let chunk = |t: &str| acp::ContentChunk::new(t.to_string().into());
        match event {
            StreamEvent::Text(t) if !t.is_empty() => self.notify(acp::SessionUpdate::AgentMessageChunk(chunk(t))),
            StreamEvent::Reasoning(t) if !t.is_empty() => self.notify(acp::SessionUpdate::AgentThoughtChunk(chunk(t))),
            _ => {}
        }
    }

    fn end_message(&mut self) {}

    fn tool_start(&mut self, tool: &str, summary: &str) {
        let file = matches!(tool, "write_file" | "edit_file").then(|| {
            let path = resolve(&self.asker.cwd, summary);
            let before = read_small(&path);
            (path, before)
        });
        let id = match self.pending.take() {
            Some(id) => {
                let fields = acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::InProgress);
                self.notify(acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(id.clone(), fields)));
                id
            }
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                let call = acp::ToolCall::new(id.clone(), title(tool, summary))
                    .kind(tool_kind(tool))
                    .status(acp::ToolCallStatus::InProgress)
                    .locations(locations(tool, summary, &self.asker.cwd));
                self.notify(acp::SessionUpdate::ToolCall(call));
                id
            }
        };
        self.running = Some(Running { id, file });
    }

    fn tool_end(&mut self, tool: &str, output: &ToolOutput) {
        let running = self.running.take().or_else(|| self.pending.take().map(|id| Running { id, file: None }));
        let Some(Running { id, file }) = running else {
            // Ended without a start (e.g. refused before running): show it once.
            let id = uuid::Uuid::new_v4().to_string();
            let call = acp::ToolCall::new(id.clone(), title(tool, ""))
                .kind(tool_kind(tool))
                .status(if output.is_error { acp::ToolCallStatus::Failed } else { acp::ToolCallStatus::Completed })
                .content(vec![text_content(&shorten(&output.content))]);
            self.notify(acp::SessionUpdate::ToolCall(call));
            return;
        };
        let content = if output.is_error {
            vec![text_content(&shorten(&output.content))]
        } else if let Some((path, before)) = file {
            match read_small(&path) {
                Some(after) if Some(&after) != before.as_ref() => {
                    vec![acp::ToolCallContent::Diff(acp::Diff::new(path, after).old_text(before))]
                }
                _ => match output.display.as_deref().filter(|d| is_diff(d)) {
                    Some(diff) => vec![text_content(&fenced(&shorten(diff), "diff"))],
                    None => vec![text_content(&shorten(&output.content))],
                },
            }
        } else if prose(tool) {
            vec![text_content(&shorten(&output.content))]
        } else {
            vec![text_content(&fenced(&shorten(&output.content), ""))]
        };
        let fields = acp::ToolCallUpdateFields::new()
            .status(if output.is_error { acp::ToolCallStatus::Failed } else { acp::ToolCallStatus::Completed })
            .content(content);
        self.notify(acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(id, fields)));
    }

    async fn approve(&mut self, req: &ApprovalRequest<'_>) -> Approval {
        let (answer, id) = self.asker.ask(None, req).await;
        self.pending = Some(id);
        answer
    }

    fn usage(&mut self, usage: &Usage) {
        if self.context_window > 0 {
            let used = usage.input_tokens + usage.output_tokens;
            self.notify(acp::SessionUpdate::UsageUpdate(acp::UsageUpdate::new(used, self.context_window)));
        }
    }

    fn notice(&mut self, msg: &str) {
        self.notify(acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(format!("{msg}\n").into())));
    }

    /// No ACP request asks the user a free-form question: the model is told
    /// the question was skipped and proceeds on its own judgment.
    async fn ask(&mut self, _question: &str, _options: &[String]) -> Option<String> {
        None
    }

    fn approver(&self) -> Option<Arc<dyn Approver>> {
        Some(self.asker.clone())
    }
}

// ------------------------------------------------------------ presentation

fn tool_kind(tool: &str) -> acp::ToolKind {
    match tool {
        "read_file" | "list_dir" | "view_image" => acp::ToolKind::Read,
        "grep" | "glob" => acp::ToolKind::Search,
        "write_file" | "edit_file" => acp::ToolKind::Edit,
        "bash" => acp::ToolKind::Execute,
        "web_fetch" | "web_search" => acp::ToolKind::Fetch,
        _ => acp::ToolKind::Other,
    }
}

/// Tools whose output is prose or markdown rather than raw text.
fn prose(tool: &str) -> bool {
    matches!(tool, "subagent" | "web_search" | "web_fetch" | "skill")
}

fn title(tool: &str, summary: &str) -> String {
    let s: String = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = if s.chars().count() > 160 { format!("{}…", s.chars().take(159).collect::<String>()) } else { s };
    if s.is_empty() {
        return tool.to_string();
    }
    match tool {
        "read_file" => format!("Read {s}"),
        "list_dir" => format!("List {s}"),
        "view_image" => format!("View {s}"),
        "glob" => format!("Find {s}"),
        "grep" => format!("Search {s}"),
        "write_file" => format!("Write {s}"),
        "edit_file" => format!("Edit {s}"),
        "bash" => format!("`{}`", s.replace('`', "'")),
        "web_fetch" => format!("Fetch {s}"),
        "web_search" => format!("Search the web: {s}"),
        "subagent" => format!("Subagent: {s}"),
        _ => match tool.strip_prefix("mcp__").and_then(|t| t.split_once("__")) {
            Some((server, name)) => format!("{server}: {name} {s}"),
            None => format!("{tool} {s}"),
        },
    }
}

fn locations(tool: &str, summary: &str, cwd: &Path) -> Vec<acp::ToolCallLocation> {
    match tool {
        "read_file" | "list_dir" | "view_image" | "write_file" | "edit_file" if !summary.is_empty() => {
            vec![acp::ToolCallLocation::new(resolve(cwd, summary))]
        }
        _ => Vec::new(),
    }
}

/// A path from a tool summary (workspace-relative or absolute).
fn resolve(cwd: &Path, summary: &str) -> PathBuf {
    let p = Path::new(summary);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

fn read_small(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_DIFF_FILE {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn is_diff(text: &str) -> bool {
    text.lines().any(|l| l.starts_with("@@")) || text.starts_with("--- ")
}

fn text_content(text: &str) -> acp::ToolCallContent {
    acp::ToolCallContent::from(text.to_string())
}

/// Keep long output readable: the head and the tail, with a marker between.
fn shorten(text: &str) -> String {
    let count = text.chars().count();
    if count <= MAX_SHOWN {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX_SHOWN * 3 / 4).collect();
    let tail: String = text.chars().skip(count - MAX_SHOWN / 4).collect();
    format!("{head}\n… [{} characters omitted] …\n{tail}", count - MAX_SHOWN)
}

/// A markdown code block that the text itself cannot close.
fn fenced(text: &str, lang: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}{lang}\n{}\n{fence}", text.trim_end_matches('\n'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Request, Response};
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    /// Replies from a script, one response per request. `Hang` waits until
    /// the request is dropped (cancelled).
    #[derive(Clone)]
    enum Step {
        Reply(Vec<Item>),
        Hang,
    }

    #[derive(Clone)]
    struct Scripted {
        steps: Arc<Vec<Step>>,
        next: Arc<AtomicUsize>,
        started: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl Provider for Scripted {
        fn name(&self) -> &str {
            "fake"
        }
        fn model(&self) -> &str {
            "scripted"
        }
        fn set_model(&mut self, _model: String) {}
        fn fork(&self) -> Box<dyn Provider> {
            Box::new(self.clone())
        }
        async fn stream(&self, _req: Request<'_>, on_event: crate::provider::EventSink<'_>) -> anyhow::Result<Response> {
            let i = self.next.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            match self.steps.get(i).cloned() {
                Some(Step::Reply(items)) => {
                    for item in &items {
                        if let Item::Assistant { text } = item {
                            on_event(StreamEvent::Text(text.clone()));
                        }
                    }
                    Ok(Response { items, usage: Some(Usage { input_tokens: 10, output_tokens: 5, cached_tokens: 0 }) })
                }
                Some(Step::Hang) => std::future::pending().await,
                None => anyhow::bail!("script exhausted"),
            }
        }
    }

    struct Harness {
        state: Arc<State>,
        client: tokio::io::DuplexStream,
        started: Arc<tokio::sync::Notify>,
        _home: tempfile::TempDir,
    }

    fn harness(steps: Vec<Step>) -> Harness {
        let home = tempfile::tempdir().unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let provider = Scripted { steps: Arc::new(steps), next: Arc::new(AtomicUsize::new(0)), started: started.clone() };
        let factory: ProviderFactory = Arc::new(move || Ok(Box::new(provider.clone()) as Box<dyn Provider>));
        let opts = Options { sandbox: Some(sandbox::Mode::Off), ..Options::default() };
        let policy = sandbox::Policy { mode: sandbox::Mode::Off, ..Default::default() };
        let state = State::new(home.path().to_path_buf(), opts, policy, factory, false);
        let (client, server) = tokio::io::duplex(1 << 20);
        let st = state.clone();
        tokio::spawn(async move {
            use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
            let (read, write) = tokio::io::split(server);
            let _ = serve(st, sdk::ByteStreams::new(write.compat_write(), read.compat())).await;
        });
        Harness { state, client, started, _home: home }
    }

    /// Everything the client saw.
    #[derive(Default)]
    struct Seen {
        updates: Vec<serde_json::Value>,
        permissions: Vec<serde_json::Value>,
    }

    /// Run `body` as an ACP client over the harness pipe, answering
    /// permission requests with `allow_once`.
    async fn as_client<R: Send + 'static>(
        client: tokio::io::DuplexStream,
        seen: Arc<Mutex<Seen>>,
        body: impl AsyncFnOnce(ConnectionTo<sdk::Agent>) -> Result<R, sdk::Error>,
    ) -> R {
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
        let (read, write) = tokio::io::split(client);
        let (s1, s2) = (seen.clone(), seen);
        sdk::Client
            .builder()
            .on_receive_notification(
                async move |n: acp::SessionNotification, _cx: ConnectionTo<sdk::Agent>| {
                    lock(&s1).updates.push(serde_json::to_value(&n.update).unwrap());
                    Ok(())
                },
                sdk::on_receive_notification!(),
            )
            .on_receive_request(
                async move |r: acp::RequestPermissionRequest,
                            responder: Responder<acp::RequestPermissionResponse>,
                            _cx: ConnectionTo<sdk::Agent>| {
                    lock(&s2).permissions.push(serde_json::to_value(&r).unwrap());
                    responder.respond(acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Selected(
                        acp::SelectedPermissionOutcome::new("allow_once"),
                    )))
                },
                sdk::on_receive_request!(),
            )
            .connect_with(sdk::ByteStreams::new(write.compat_write(), read.compat()), body)
            .await
            .unwrap()
    }

    fn write_call() -> Item {
        Item::ToolCall {
            id: "call_1".into(),
            name: "write_file".into(),
            arguments: json!({"path": "out.txt", "content": "done\n"}).to_string(),
        }
    }

    #[tokio::test]
    async fn prompt_runs_tools_with_editor_approval() {
        let h = harness(vec![
            Step::Reply(vec![Item::Assistant { text: "Writing.".into() }, write_call()]),
            Step::Reply(vec![Item::Assistant { text: "All done.".into() }]),
        ]);
        let work = tempfile::tempdir().unwrap();
        let cwd = work.path().to_path_buf();
        let cwd2 = cwd.clone();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let (init, session, stop) = as_client(h.client, seen.clone(), async move |cx: ConnectionTo<sdk::Agent>| {
            let init = cx.send_request(acp::InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
            let session = cx.send_request(acp::NewSessionRequest::new(cwd)).block_task().await?;
            let prompt = vec![acp::ContentBlock::from("copy the notes")];
            let stop = cx.send_request(acp::PromptRequest::new(session.session_id.clone(), prompt)).block_task().await?;
            // Reopening the saved session replays it before answering.
            cx.send_request(acp::LoadSessionRequest::new(session.session_id.clone(), cwd2)).block_task().await?;
            Ok((init, session, stop))
        })
        .await;

        let init = serde_json::to_value(&init).unwrap();
        assert_eq!(init["protocolVersion"], 1);
        assert_eq!(init["agentInfo"]["name"], "nyq");
        assert_eq!(init["agentCapabilities"]["loadSession"], true);
        assert_eq!(init["agentCapabilities"]["promptCapabilities"]["image"], true);
        assert_eq!(init["agentCapabilities"]["promptCapabilities"]["embeddedContext"], true);
        assert_eq!(init["agentCapabilities"]["mcpCapabilities"]["http"], true);
        assert!(session.modes.is_some());
        assert_eq!(stop.stop_reason, acp::StopReason::EndTurn);
        assert_eq!(std::fs::read_to_string(work.path().join("out.txt")).unwrap(), "done\n");

        let seen = lock(&seen);
        assert_eq!(seen.permissions.len(), 1, "{:?}", seen.permissions);
        let perm = &seen.permissions[0];
        let options: Vec<&str> = perm["options"].as_array().unwrap().iter().map(|o| o["optionId"].as_str().unwrap()).collect();
        assert!(options.contains(&"allow_once") && options.contains(&"reject_once"), "{options:?}");
        let call_id = perm["toolCall"]["toolCallId"].as_str().unwrap();

        let kinds: Vec<&str> = seen.updates.iter().map(|u| u["sessionUpdate"].as_str().unwrap()).collect();
        let text: String = seen
            .updates
            .iter()
            .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
            .map(|u| u["content"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(text, "Writing.All done.".repeat(2), "live, then replayed by session/load");
        let card = seen.updates.iter().find(|u| u["sessionUpdate"] == "tool_call").expect("tool_call");
        assert_eq!(card["toolCallId"], call_id);
        assert_eq!(card["kind"], "edit");
        assert_eq!(card["title"], "Write out.txt");
        assert_eq!(card["locations"][0]["path"], work.path().join("out.txt").display().to_string());
        let done = seen
            .updates
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "completed")
            .unwrap_or_else(|| panic!("no completed update in {kinds:?}"));
        assert_eq!(done["toolCallId"], call_id);
        assert_eq!(done["content"][0]["type"], "diff");
        assert_eq!(done["content"][0]["newText"], "done\n");
        assert!(kinds.contains(&"usage_update"), "{kinds:?}");
        let replayed = seen.updates.iter().find(|u| u["sessionUpdate"] == "user_message_chunk").expect("replay");
        assert_eq!(replayed["content"]["text"], "copy the notes");
        assert_eq!(seen.updates.iter().filter(|u| u["sessionUpdate"] == "tool_call").count(), 2, "live + replayed");
        // The conversation was saved like any other nyq session.
        assert_eq!(session::list(&h.state.sessions_dir()).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancel_stops_the_turn() {
        let h = harness(vec![Step::Hang]);
        let work = tempfile::tempdir().unwrap();
        let cwd = work.path().to_path_buf();
        let started = h.started.clone();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let stop = as_client(h.client, seen, async move |cx: ConnectionTo<sdk::Agent>| {
            cx.send_request(acp::InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
            let session = cx.send_request(acp::NewSessionRequest::new(cwd)).block_task().await?;
            let id = session.session_id.clone();
            let pending = cx.send_request(acp::PromptRequest::new(id.clone(), vec!["wait".into()]));
            started.notified().await;
            cx.send_notification(acp::CancelNotification::new(id))?;
            tokio::time::timeout(std::time::Duration::from_secs(10), pending.block_task())
                .await
                .map_err(|_| internal("prompt did not stop"))?
        })
        .await;
        assert_eq!(stop.stop_reason, acp::StopReason::Cancelled);
    }

    #[test]
    fn content_blocks_become_one_user_message() {
        let cwd = Path::new("/work");
        let blocks = vec![
            acp::ContentBlock::from("look at "),
            acp::ContentBlock::ResourceLink(acp::ResourceLink::new("main.rs", "file:///work/src/main.rs")),
            acp::ContentBlock::Resource(acp::EmbeddedResource::new(acp::EmbeddedResourceResource::TextResourceContents(
                acp::TextResourceContents::new("fn main() {}", "file:///work/lib.rs"),
            ))),
            acp::ContentBlock::Image(acp::ImageContent::new("aGk=", "image/png")),
        ];
        let Item::User { text, images } = user_item(&blocks, cwd) else { panic!() };
        // `file:///work/...` is a local path only on Unix; Windows keeps the link.
        if cfg!(unix) {
            assert!(text.starts_with("look at @/work/src/main.rs@/work/lib.rs"), "{text}");
        } else {
            assert!(text.starts_with("look at ") && text.contains("file:///work/src/main.rs"), "{text}");
        }
        assert!(text.contains("<context ref=\"file:///work/lib.rs\">\nfn main() {}\n</context>"), "{text}");
        assert_eq!(images, vec![Image { media_type: "image/png".into(), data: "aGk=".into() }]);
    }

    #[test]
    fn presentation_helpers() {
        assert_eq!(title("bash", "cargo  test\n--all"), "`cargo test --all`");
        assert_eq!(title("mcp__fs__read", "x"), "fs: read x");
        assert_eq!(fenced("a ``` b", ""), "````\na ``` b\n````");
        assert!(shorten(&"x".repeat(20_000)).contains("characters omitted"));
        assert_eq!(model_options(&[], "openai:gpt-5")[0].id.0.as_ref(), MODEL_OPTION);
    }
}
