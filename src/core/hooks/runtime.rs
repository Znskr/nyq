//! Hook runtime: registration, freezing, and dispatch (ported 1:1 from
//! zig-reference/src/core/hooks/runtime.zig).

use super::definitions::{
    AttentionRequiredInput, AttentionRequiredRunFn, HandlerContext, HandlerError, HookKind,
    Invocation, Limits, MutationDispatchError, PostTurnEndInput, PostTurnEndRunFn,
    PreToolUseAction, PreToolUseInput, PreToolUseOutcome, PreToolUseRunFn, RegistrationError,
    StopAction, StopInput, StopOutcome, StopRunFn, ViewError,
};
#[cfg(test)]
use super::definitions::{Scope, ScopeKind};
use crate::core::mcp::json_value;
use crate::core::shared::{debug_trace, io};

#[derive(Clone, Debug)]
struct RegisteredHook {
    event: HookKind,
    name: String,
    ctx: HandlerContext,
}

#[derive(Clone, Debug)]
struct RegisteredPreToolUseHandler {
    hook: RegisteredHook,
    run: PreToolUseRunFn,
}

#[derive(Clone, Debug)]
struct RegisteredStopHandler {
    hook: RegisteredHook,
    run: StopRunFn,
}

#[derive(Clone, Debug)]
struct RegisteredPostTurnEndHandler {
    hook: RegisteredHook,
    run: PostTurnEndRunFn,
}

#[derive(Clone, Debug)]
struct RegisteredAttentionRequiredHandler {
    hook: RegisteredHook,
    run: AttentionRequiredRunFn,
}

trait HookEntry {
    fn hook(&self) -> &RegisteredHook;
}

impl HookEntry for RegisteredPreToolUseHandler {
    fn hook(&self) -> &RegisteredHook {
        &self.hook
    }
}

impl HookEntry for RegisteredStopHandler {
    fn hook(&self) -> &RegisteredHook {
        &self.hook
    }
}

impl HookEntry for RegisteredPostTurnEndHandler {
    fn hook(&self) -> &RegisteredHook {
        &self.hook
    }
}

impl HookEntry for RegisteredAttentionRequiredHandler {
    fn hook(&self) -> &RegisteredHook {
        &self.hook
    }
}

pub struct Runtime {
    pre_tool_use_handlers: Vec<RegisteredPreToolUseHandler>,
    stop_handlers: Vec<RegisteredStopHandler>,
    post_turn_end_handlers: Vec<RegisteredPostTurnEndHandler>,
    attention_required_handlers: Vec<RegisteredAttentionRequiredHandler>,
    frozen: std::cell::Cell<bool>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::init()
    }
}

impl Runtime {
    pub fn init() -> Runtime {
        Runtime {
            pre_tool_use_handlers: Vec::new(),
            stop_handlers: Vec::new(),
            post_turn_end_handlers: Vec::new(),
            attention_required_handlers: Vec::new(),
            frozen: std::cell::Cell::new(false),
        }
    }

    pub fn deinit(&mut self) {
        self.pre_tool_use_handlers.clear();
        self.stop_handlers.clear();
        self.post_turn_end_handlers.clear();
        self.attention_required_handlers.clear();
        self.frozen.set(false);
    }

    pub fn register_pre_tool_use(
        &mut self,
        handler: super::definitions::PreToolUseHandler,
    ) -> Result<(), RegistrationError> {
        self.validate_registration(HookKind::PreToolUse, handler.name, &self.pre_tool_use_handlers)?;
        let hook = copy_hook(HookKind::PreToolUse, handler.name, handler.ctx);
        self.pre_tool_use_handlers.push(RegisteredPreToolUseHandler {
            hook,
            run: handler.run,
        });
        Ok(())
    }

    pub fn register_stop(
        &mut self,
        handler: super::definitions::StopHandler,
    ) -> Result<(), RegistrationError> {
        self.validate_registration(HookKind::Stop, handler.name, &self.stop_handlers)?;
        let hook = copy_hook(HookKind::Stop, handler.name, handler.ctx);
        self.stop_handlers.push(RegisteredStopHandler {
            hook,
            run: handler.run,
        });
        Ok(())
    }

    pub fn register_post_turn_end(
        &mut self,
        handler: super::definitions::PostTurnEndHandler,
    ) -> Result<(), RegistrationError> {
        self.validate_registration(
            HookKind::PostTurnEnd,
            handler.name,
            &self.post_turn_end_handlers,
        )?;
        let hook = copy_hook(HookKind::PostTurnEnd, handler.name, handler.ctx);
        self.post_turn_end_handlers
            .push(RegisteredPostTurnEndHandler {
                hook,
                run: handler.run,
            });
        Ok(())
    }

    pub fn register_attention_required(
        &mut self,
        handler: super::definitions::AttentionRequiredHandler,
    ) -> Result<(), RegistrationError> {
        self.validate_registration(
            HookKind::AttentionRequired,
            handler.name,
            &self.attention_required_handlers,
        )?;
        let hook = copy_hook(HookKind::AttentionRequired, handler.name, handler.ctx);
        self.attention_required_handlers
            .push(RegisteredAttentionRequiredHandler {
                hook,
                run: handler.run,
            });
        Ok(())
    }

    pub fn freeze(&self) -> RuntimeView<'_> {
        self.frozen.set(true);
        self.current_view()
    }

    pub fn view(&self) -> Result<RuntimeView<'_>, ViewError> {
        if !self.frozen.get() {
            return Err(ViewError::RuntimeNotFrozen);
        }
        Ok(self.current_view())
    }

    fn current_view(&self) -> RuntimeView<'_> {
        RuntimeView {
            pre_tool_use_handlers: &self.pre_tool_use_handlers,
            stop_handlers: &self.stop_handlers,
            post_turn_end_handlers: &self.post_turn_end_handlers,
            attention_required_handlers: &self.attention_required_handlers,
        }
    }

    fn validate_registration<E: HookEntry>(
        &self,
        event: HookKind,
        name: &str,
        registered_handlers: &[E],
    ) -> Result<(), RegistrationError> {
        if self.frozen.get() {
            return Err(RegistrationError::RuntimeFrozen);
        }
        validate_handler_name(name)?;
        for registered in registered_handlers {
            debug_assert_eq!(registered.hook().event, event);
            if registered.hook().name == name {
                return Err(RegistrationError::DuplicateHandlerName);
            }
        }
        Ok(())
    }
}

fn copy_hook(event: HookKind, name: &str, ctx: HandlerContext) -> RegisteredHook {
    RegisteredHook {
        event,
        name: name.to_string(),
        ctx,
    }
}

#[derive(Clone, Copy)]
pub struct RuntimeView<'r> {
    pre_tool_use_handlers: &'r [RegisteredPreToolUseHandler],
    stop_handlers: &'r [RegisteredStopHandler],
    post_turn_end_handlers: &'r [RegisteredPostTurnEndHandler],
    attention_required_handlers: &'r [RegisteredAttentionRequiredHandler],
}

impl RuntimeView<'static> {
    pub fn empty() -> RuntimeView<'static> {
        RuntimeView {
            pre_tool_use_handlers: &[],
            stop_handlers: &[],
            post_turn_end_handlers: &[],
            attention_required_handlers: &[],
        }
    }
}

impl<'r> RuntimeView<'r> {
    pub fn has_pre_tool_use(&self) -> bool {
        !self.pre_tool_use_handlers.is_empty()
    }

    pub fn run_pre_tool_use(
        &self,
        input: PreToolUseInput<'_>,
    ) -> Result<PreToolUseOutcome, MutationDispatchError> {
        if self.pre_tool_use_handlers.is_empty() {
            return Ok(PreToolUseOutcome::Unchanged);
        }
        let mut dispatch = PreToolUseDispatch {
            input,
            current_arguments: None,
        };
        dispatch.run(self.pre_tool_use_handlers)
    }

    pub fn has_stop(&self) -> bool {
        !self.stop_handlers.is_empty()
    }

    pub fn run_stop(&self, input: StopInput<'_>) -> StopOutcome {
        if self.stop_handlers.is_empty() {
            return StopOutcome::Allow;
        }
        let dispatch = StopDispatch { input };
        dispatch.run(self.stop_handlers)
    }

    pub fn has_post_turn_end(&self) -> bool {
        !self.post_turn_end_handlers.is_empty()
    }

    pub fn run_post_turn_end(&self, input: PostTurnEndInput) {
        run_post_turn_end_handlers(&input, self.post_turn_end_handlers);
    }

    pub fn has_attention_required(&self) -> bool {
        !self.attention_required_handlers.is_empty()
    }

    pub fn run_attention_required(&self, input: AttentionRequiredInput) {
        run_attention_required_handlers(&input, self.attention_required_handlers);
    }
}

struct PreToolUseDispatch<'a> {
    input: PreToolUseInput<'a>,
    current_arguments: Option<String>,
}

impl PreToolUseDispatch<'_> {
    fn run(
        &mut self,
        handlers: &[RegisteredPreToolUseHandler],
    ) -> Result<PreToolUseOutcome, MutationDispatchError> {
        for handler in handlers {
            match self.run_handler(handler)? {
                PreToolUseActionResult::Next => {}
                PreToolUseActionResult::Blocked(reason) => {
                    return Ok(PreToolUseOutcome::Blocked(reason))
                }
            }
        }
        Ok(self.take_outcome())
    }

    fn run_handler(
        &mut self,
        handler: &RegisteredPreToolUseHandler,
    ) -> Result<PreToolUseActionResult, MutationDispatchError> {
        let trace = HandlerTrace::start(
            &handler.hook,
            &self.input.invocation,
            Some(self.input.call_id),
        );
        let action = match (handler.run)(handler.hook.ctx, self.handler_input()) {
            Ok(action) => action,
            Err(err) => {
                trace.fail(err.as_str());
                return Err(match err {
                    HandlerError::Failed => MutationDispatchError::HandlerFailed,
                    HandlerError::Cancelled => MutationDispatchError::Cancelled,
                });
            }
        };
        match apply_pre_tool_use_action(action, &trace)? {
            PreToolUseApply::Next => Ok(PreToolUseActionResult::Next),
            PreToolUseApply::Rewritten(copied) => {
                self.current_arguments = Some(copied);
                Ok(PreToolUseActionResult::Next)
            }
            PreToolUseApply::Blocked(reason) => {
                self.current_arguments = None;
                Ok(PreToolUseActionResult::Blocked(reason))
            }
        }
    }

    fn take_outcome(&mut self) -> PreToolUseOutcome {
        match self.current_arguments.take() {
            Some(arguments_json) => PreToolUseOutcome::Rewritten(arguments_json),
            None => PreToolUseOutcome::Unchanged,
        }
    }

    fn handler_input(&self) -> PreToolUseInput<'_> {
        let mut input = self.input.clone();
        if let Some(arguments_json) = &self.current_arguments {
            input.arguments_json = arguments_json;
        }
        input
    }
}

enum PreToolUseActionResult {
    Next,
    Blocked(String),
}

enum PreToolUseApply {
    Next,
    Rewritten(String),
    Blocked(String),
}

fn apply_pre_tool_use_action(
    action: PreToolUseAction<'_>,
    trace: &HandlerTrace<'_>,
) -> Result<PreToolUseApply, MutationDispatchError> {
    match action {
        PreToolUseAction::Continue => {
            trace.finish("continue");
            Ok(PreToolUseApply::Next)
        }
        PreToolUseAction::RewriteArguments(borrowed) => {
            if borrowed.len() > Limits::arguments_json_bytes() {
                trace.finish("rewrite_too_large");
                return Err(MutationDispatchError::HandlerOutputTooLarge);
            }
            let copied = borrowed.to_string();
            if let Err(err) = validate_json_object(&copied) {
                trace.fail(err.as_str());
                return Err(err);
            }
            trace.finish("rewrite");
            Ok(PreToolUseApply::Rewritten(copied))
        }
        PreToolUseAction::Block(borrowed) => {
            if borrowed.is_empty() {
                trace.finish("invalid_block");
                return Err(MutationDispatchError::InvalidHandlerOutput);
            }
            if borrowed.len() > Limits::reason_bytes() {
                trace.finish("block_too_large");
                return Err(MutationDispatchError::HandlerOutputTooLarge);
            }
            let reason = borrowed.to_string();
            trace.finish("block");
            Ok(PreToolUseApply::Blocked(reason))
        }
    }
}

enum StopActionResult {
    Next,
    Done(StopOutcome),
}

struct StopDispatch<'a> {
    input: StopInput<'a>,
}

impl StopDispatch<'_> {
    fn run(&self, handlers: &[RegisteredStopHandler]) -> StopOutcome {
        for handler in handlers {
            match self.run_handler(handler) {
                StopActionResult::Next => {}
                StopActionResult::Done(outcome) => return outcome,
            }
        }
        StopOutcome::Allow
    }

    fn run_handler(&self, handler: &RegisteredStopHandler) -> StopActionResult {
        let trace = HandlerTrace::start(&handler.hook, &self.input.invocation, None);
        let action = match (handler.run)(handler.hook.ctx, self.input.clone()) {
            Ok(action) => action,
            Err(err) => {
                trace.fail(err.as_str());
                return StopActionResult::Done(StopOutcome::Allow);
            }
        };
        apply_stop_action(action, self.input.can_continue, &trace)
    }
}

fn apply_stop_action(
    action: StopAction<'_>,
    can_continue: bool,
    trace: &HandlerTrace<'_>,
) -> StopActionResult {
    match action {
        StopAction::Allow => {
            trace.finish("allow");
            StopActionResult::Next
        }
        StopAction::ContinueOnce(borrowed) => {
            if !can_continue {
                trace.finish("continue_not_allowed");
                return StopActionResult::Done(StopOutcome::Allow);
            }
            if borrowed.len() > Limits::context_bytes() {
                trace.finish("invalid_continue");
                return StopActionResult::Done(StopOutcome::Allow);
            }
            let context = borrowed.to_string();
            trace.finish("continue_once");
            StopActionResult::Done(StopOutcome::ContinueOnce(context))
        }
    }
}

fn run_post_turn_end_handlers(input: &PostTurnEndInput, handlers: &[RegisteredPostTurnEndHandler]) {
    for handler in handlers {
        let trace = HandlerTrace::start(&handler.hook, &input.invocation, None);
        match (handler.run)(handler.hook.ctx, input.clone()) {
            Ok(()) => trace.finish("ok"),
            Err(err) => trace.fail(err.as_str()),
        }
    }
}

fn run_attention_required_handlers(
    input: &AttentionRequiredInput,
    handlers: &[RegisteredAttentionRequiredHandler],
) {
    for handler in handlers {
        let trace = HandlerTrace::start(&handler.hook, &input.invocation, None);
        match (handler.run)(handler.hook.ctx, input.clone()) {
            Ok(()) => trace.finish("ok"),
            Err(err) => trace.fail(err.as_str()),
        }
    }
}

fn validate_handler_name(name: &str) -> Result<(), RegistrationError> {
    if name.is_empty() {
        return Err(RegistrationError::EmptyHandlerName);
    }
    if name.len() > Limits::handler_name_bytes() {
        return Err(RegistrationError::HandlerNameTooLong);
    }
    for byte in name.bytes() {
        let valid = byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_' || byte == b'-';
        if !valid {
            return Err(RegistrationError::InvalidHandlerName);
        }
    }
    Ok(())
}

fn validate_json_object(arguments_json: &str) -> Result<(), MutationDispatchError> {
    let parsed = json_value::parse_json(arguments_json)
        .map_err(|_| MutationDispatchError::InvalidHandlerOutput)?;
    if !parsed.is_object() {
        return Err(MutationDispatchError::InvalidHandlerOutput);
    }
    Ok(())
}

struct HandlerTrace<'a> {
    event: HookKind,
    handler_name: &'a str,
    invocation: Invocation,
    call_id: Option<&'a str>,
    started_ms: i64,
}

impl<'a> HandlerTrace<'a> {
    fn start(
        hook: &'a RegisteredHook,
        invocation: &Invocation,
        call_id: Option<&'a str>,
    ) -> HandlerTrace<'a> {
        let trace = HandlerTrace {
            event: hook.event,
            handler_name: hook.name.as_str(),
            invocation: invocation.clone(),
            call_id,
            started_ms: io::milli_timestamp(),
        };
        trace.emit_start();
        trace
    }

    fn finish(&self, outcome: &str) {
        let duration_ms = self.duration_ms();
        let ctx = trace_context(&self.invocation);
        if let Some(id) = self.call_id {
            debug_trace::eventf(
                "hooks",
                "handler_finish",
                ctx,
                format_args!(
                    "lifecycle_event={} handler={} scope={} call_id={} duration_ms={} outcome={}",
                    self.event.definition().lifecycle_event,
                    self.handler_name,
                    self.invocation.scope.kind.as_str(),
                    id,
                    duration_ms,
                    outcome
                ),
            );
        } else {
            debug_trace::eventf(
                "hooks",
                "handler_finish",
                ctx,
                format_args!(
                    "lifecycle_event={} handler={} scope={} duration_ms={} outcome={}",
                    self.event.definition().lifecycle_event,
                    self.handler_name,
                    self.invocation.scope.kind.as_str(),
                    duration_ms,
                    outcome
                ),
            );
        }
    }

    fn fail(&self, err: &str) {
        let duration_ms = self.duration_ms();
        let ctx = trace_context(&self.invocation);
        if let Some(id) = self.call_id {
            debug_trace::eventf(
                "hooks",
                "handler_finish",
                ctx,
                format_args!(
                    "lifecycle_event={} handler={} scope={} call_id={} duration_ms={} outcome=error error={}",
                    self.event.definition().lifecycle_event,
                    self.handler_name,
                    self.invocation.scope.kind.as_str(),
                    id,
                    duration_ms,
                    err
                ),
            );
        } else {
            debug_trace::eventf(
                "hooks",
                "handler_finish",
                ctx,
                format_args!(
                    "lifecycle_event={} handler={} scope={} duration_ms={} outcome=error error={}",
                    self.event.definition().lifecycle_event,
                    self.handler_name,
                    self.invocation.scope.kind.as_str(),
                    duration_ms,
                    err
                ),
            );
        }
    }

    fn emit_start(&self) {
        let ctx = trace_context(&self.invocation);
        if let Some(id) = self.call_id {
            debug_trace::eventf(
                "hooks",
                "handler_start",
                ctx,
                format_args!(
                    "lifecycle_event={} handler={} scope={} call_id={}",
                    self.event.definition().lifecycle_event,
                    self.handler_name,
                    self.invocation.scope.kind.as_str(),
                    id
                ),
            );
        } else {
            debug_trace::eventf(
                "hooks",
                "handler_start",
                ctx,
                format_args!(
                    "lifecycle_event={} handler={} scope={}",
                    self.event.definition().lifecycle_event,
                    self.handler_name,
                    self.invocation.scope.kind.as_str()
                ),
            );
        }
    }

    fn duration_ms(&self) -> i64 {
        (io::milli_timestamp() - self.started_ms).max(0)
    }
}

fn trace_context(invocation: &Invocation) -> debug_trace::TraceContext {
    debug_trace::TraceContext {
        turn_id: invocation.turn_id.unwrap_or(0),
        step_id: 0,
        subagent_id: invocation.scope.subagent_id.unwrap_or(0),
    }
}

#[cfg(test)]
fn test_invocation() -> Invocation {
    Invocation {
        scope: Scope {
            kind: ScopeKind::Interactive,
            workspace_root: "/tmp/workspace".to_string(),
            session_id: Some("session".to_string()),
            subagent_id: None,
        },
        turn_id: Some(42),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::shared::types::{ProviderCompletionDisposition, TurnPresentationOutcome};
    use super::super::definitions::AttentionKind;
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn continue_tool(_: HandlerContext, _: PreToolUseInput) -> Result<PreToolUseAction, HandlerError> {
        Ok(PreToolUseAction::Continue)
    }

    fn allow_stop(_: HandlerContext, _: StopInput) -> Result<StopAction, HandlerError> {
        Ok(StopAction::Allow)
    }

    fn note_post_turn_end(_: HandlerContext, _: PostTurnEndInput) -> Result<(), HandlerError> {
        Ok(())
    }

    fn note_attention_required(_: HandlerContext, _: AttentionRequiredInput) -> Result<(), HandlerError> {
        Ok(())
    }

    #[test]
    fn runtime_validates_names_owns_copies_freezes_and_exposes_lifecycle_events() {
        let mut runtime = Runtime::init();

        assert_eq!(runtime.view().err(), Some(ViewError::RuntimeNotFrozen));
        assert_eq!(
            runtime.register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                name: "",
                ctx: 0,
                run: continue_tool,
            }),
            Err(RegistrationError::EmptyHandlerName)
        );
        assert_eq!(
            runtime.register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                name: "bad name",
                ctx: 0,
                run: continue_tool,
            }),
            Err(RegistrationError::InvalidHandlerName)
        );

        let long_name = "a".repeat(Limits::handler_name_bytes() + 1);
        assert_eq!(
            runtime.register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                name: &long_name,
                ctx: 0,
                run: continue_tool,
            }),
            Err(RegistrationError::HandlerNameTooLong)
        );

        let name = String::from("alpha");
        runtime
            .register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                name: name.as_str(),
                ctx: 0,
                run: continue_tool,
            })
            .unwrap();
        assert_eq!(
            runtime.register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                name: "alpha",
                ctx: 0,
                run: continue_tool,
            }),
            Err(RegistrationError::DuplicateHandlerName)
        );
        runtime
            .register_stop(super::super::definitions::StopHandler {
                name: "alpha",
                ctx: 0,
                run: allow_stop,
            })
            .unwrap();
        runtime
            .register_post_turn_end(super::super::definitions::PostTurnEndHandler {
                name: "alpha",
                ctx: 0,
                run: note_post_turn_end,
            })
            .unwrap();
        assert_eq!(
            runtime.register_post_turn_end(super::super::definitions::PostTurnEndHandler {
                name: "alpha",
                ctx: 0,
                run: note_post_turn_end,
            }),
            Err(RegistrationError::DuplicateHandlerName)
        );
        runtime
            .register_attention_required(super::super::definitions::AttentionRequiredHandler {
                name: "alpha",
                ctx: 0,
                run: note_attention_required,
            })
            .unwrap();
        assert_eq!(
            runtime.register_attention_required(super::super::definitions::AttentionRequiredHandler {
                name: "alpha",
                ctx: 0,
                run: note_attention_required,
            }),
            Err(RegistrationError::DuplicateHandlerName)
        );

        let view = runtime.freeze();
        let second_view = runtime.freeze();
        assert!(view.has_pre_tool_use());
        assert!(view.has_stop());
        assert!(view.has_post_turn_end());
        assert!(view.has_attention_required());
        assert!(second_view.has_pre_tool_use());
        assert!(second_view.has_stop());
        assert!(second_view.has_post_turn_end());
        assert!(second_view.has_attention_required());
        assert_eq!(
            runtime.register_stop(super::super::definitions::StopHandler {
                name: "late",
                ctx: 0,
                run: allow_stop,
            }),
            Err(RegistrationError::RuntimeFrozen)
        );
        assert_eq!(
            runtime.register_post_turn_end(super::super::definitions::PostTurnEndHandler {
                name: "late",
                ctx: 0,
                run: note_post_turn_end,
            }),
            Err(RegistrationError::RuntimeFrozen)
        );
        assert_eq!(
            runtime.register_attention_required(super::super::definitions::AttentionRequiredHandler {
                name: "late",
                ctx: 0,
                run: note_attention_required,
            }),
            Err(RegistrationError::RuntimeFrozen)
        );

        // Zig asserts `!@hasDecl(Runtime, ...)` for retired surfaces; the Rust
        // port simply never declares those register methods.
    }

    fn check_empty_view(view: &RuntimeView) {
        assert!(!view.has_pre_tool_use());
        assert!(!view.has_stop());
        assert!(!view.has_post_turn_end());
        assert!(!view.has_attention_required());

        let mut pre = view
            .run_pre_tool_use(PreToolUseInput {
                invocation: test_invocation(),
                step_index: 1,
                call_id: "call",
                tool_name: "read_file",
                arguments_json: "{}",
            })
            .unwrap();
        pre.deinit();
        assert_eq!(pre, PreToolUseOutcome::Unchanged);

        let mut stop = view.run_stop(StopInput {
            invocation: test_invocation(),
            step_index: 1,
            assistant_text: "done",
            provider_disposition: ProviderCompletionDisposition::Completed,
            can_continue: true,
        });
        stop.deinit();
        assert_eq!(stop, StopOutcome::Allow);

        view.run_post_turn_end(PostTurnEndInput {
            invocation: test_invocation(),
            outcome: TurnPresentationOutcome::Completed,
            provider_disposition: Some(ProviderCompletionDisposition::Completed),
        });
        view.run_attention_required(AttentionRequiredInput {
            invocation: test_invocation(),
            kind: AttentionKind::Permission,
        });
    }

    #[test]
    fn empty_views_dispatch_without_allocation() {
        let mut runtime = Runtime::init();
        let view = runtime.freeze();
        check_empty_view(&view);
        check_empty_view(&RuntimeView::empty());
    }

    thread_local! {
        static REWRITE: RefCell<RewriteCapture> = RefCell::new(RewriteCapture::default());
    }

    #[derive(Default)]
    struct RewriteCapture {
        order: Vec<u8>,
        second_saw_first: bool,
        third_called: bool,
    }

    impl RewriteCapture {
        fn note(&mut self, value: u8) {
            self.order.push(value);
        }
    }

    fn rewrite_first(_: HandlerContext, _: PreToolUseInput) -> Result<PreToolUseAction, HandlerError> {
        REWRITE.with(|c| c.borrow_mut().note(1));
        Ok(PreToolUseAction::RewriteArguments("{\"a\":1}"))
    }

    fn rewrite_second(_: HandlerContext, input: PreToolUseInput) -> Result<PreToolUseAction, HandlerError> {
        REWRITE.with(|c| {
            let mut c = c.borrow_mut();
            c.note(2);
            c.second_saw_first = input.arguments_json == "{\"a\":1}";
        });
        Ok(PreToolUseAction::RewriteArguments("{\"b\":2}"))
    }

    fn rewrite_block(_: HandlerContext, input: PreToolUseInput) -> Result<PreToolUseAction, HandlerError> {
        REWRITE.with(|c| c.borrow_mut().note(3));
        if input.arguments_json != "{\"b\":2}" {
            return Err(HandlerError::Failed);
        }
        Ok(PreToolUseAction::Block("blocked by policy"))
    }

    fn rewrite_never(_: HandlerContext, _: PreToolUseInput) -> Result<PreToolUseAction, HandlerError> {
        REWRITE.with(|c| c.borrow_mut().third_called = true);
        Ok(PreToolUseAction::Continue)
    }

    #[test]
    fn pre_tool_use_chains_rewrites_in_registration_order_and_first_block_wins() {
        REWRITE.with(|c| *c.borrow_mut() = RewriteCapture::default());
        let mut runtime = Runtime::init();
        runtime
            .register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                name: "first",
                ctx: 0,
                run: rewrite_first,
            })
            .unwrap();
        runtime
            .register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                name: "second",
                ctx: 0,
                run: rewrite_second,
            })
            .unwrap();
        runtime
            .register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                name: "block",
                ctx: 0,
                run: rewrite_block,
            })
            .unwrap();
        runtime
            .register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                name: "never",
                ctx: 0,
                run: rewrite_never,
            })
            .unwrap();
        let mut outcome = runtime
            .freeze()
            .run_pre_tool_use(PreToolUseInput {
                invocation: test_invocation(),
                step_index: 2,
                call_id: "call-1",
                tool_name: "read_file",
                arguments_json: "{\"original\":true}",
            })
            .unwrap();
        assert_eq!(
            outcome,
            PreToolUseOutcome::Blocked("blocked by policy".to_string())
        );
        outcome.deinit();
        REWRITE.with(|c| {
            let c = c.borrow();
            assert_eq!(c.order, vec![1, 2, 3]);
            assert!(c.second_saw_first);
            assert!(!c.third_called);
        });
    }

    fn rewrite_borrow(_: HandlerContext, input: PreToolUseInput) -> Result<PreToolUseAction, HandlerError> {
        Ok(PreToolUseAction::RewriteArguments(input.arguments_json))
    }

    #[test]
    fn pre_tool_use_returns_an_owned_final_rewrite() {
        let mut source = String::from("{\"x\":1}");
        let mut runtime = Runtime::init();
        runtime
            .register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                name: "rewrite",
                ctx: 0,
                run: rewrite_borrow,
            })
            .unwrap();
        let mut outcome = runtime
            .freeze()
            .run_pre_tool_use(PreToolUseInput {
                invocation: test_invocation(),
                step_index: 1,
                call_id: "call",
                tool_name: "tool",
                arguments_json: &source,
            })
            .unwrap();
        source.replace_range(5..6, "9");
        assert_eq!(
            outcome,
            PreToolUseOutcome::Rewritten("{\"x\":1}".to_string())
        );
        outcome.deinit();
    }

    thread_local! {
        static CURRENT_REWRITE: Cell<&'static str> = Cell::new("");
    }

    fn rewrite_current(_: HandlerContext, _: PreToolUseInput) -> Result<PreToolUseAction, HandlerError> {
        Ok(PreToolUseAction::RewriteArguments(
            CURRENT_REWRITE.with(Cell::get),
        ))
    }

    #[test]
    fn pre_tool_use_rejects_non_object_malformed_trailing_and_oversized_rewrites() {
        for output in ["[]", "1", "null", "{", "{} trailing"] {
            CURRENT_REWRITE.with(|c| c.set(output));
            let mut runtime = Runtime::init();
            runtime
                .register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                    name: "invalid",
                    ctx: 0,
                    run: rewrite_current,
                })
                .unwrap();
            assert_eq!(
                runtime.freeze().run_pre_tool_use(PreToolUseInput {
                    invocation: test_invocation(),
                    step_index: 1,
                    call_id: "call",
                    tool_name: "tool",
                    arguments_json: "{}",
                }),
                Err(MutationDispatchError::InvalidHandlerOutput)
            );
        }

        let oversized: &'static str =
            Box::leak(" ".repeat(Limits::arguments_json_bytes() + 1).into_boxed_str());
        CURRENT_REWRITE.with(|c| c.set(oversized));
        let mut runtime = Runtime::init();
        runtime
            .register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                name: "oversized",
                ctx: 0,
                run: rewrite_current,
            })
            .unwrap();
        assert_eq!(
            runtime.freeze().run_pre_tool_use(PreToolUseInput {
                invocation: test_invocation(),
                step_index: 1,
                call_id: "call",
                tool_name: "tool",
                arguments_json: "{}",
            }),
            Err(MutationDispatchError::HandlerOutputTooLarge)
        );
    }

    thread_local! {
        static TOOL_ERROR_CANCELLED: Cell<bool> = Cell::new(false);
    }

    fn error_tool(_: HandlerContext, _: PreToolUseInput) -> Result<PreToolUseAction, HandlerError> {
        if TOOL_ERROR_CANCELLED.with(Cell::get) {
            Err(HandlerError::Cancelled)
        } else {
            Err(HandlerError::Failed)
        }
    }

    #[test]
    fn pre_tool_use_maps_handler_errors_and_allocation_failure_without_leaking_prior_rewrites() {
        for cancelled in [false, true] {
            TOOL_ERROR_CANCELLED.with(|c| c.set(cancelled));
            let mut runtime = Runtime::init();
            runtime
                .register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                    name: "error",
                    ctx: 0,
                    run: error_tool,
                })
                .unwrap();
            let result = runtime.freeze().run_pre_tool_use(PreToolUseInput {
                invocation: test_invocation(),
                step_index: 1,
                call_id: "call",
                tool_name: "tool",
                arguments_json: "{}",
            });
            if cancelled {
                assert_eq!(result, Err(MutationDispatchError::Cancelled));
            } else {
                assert_eq!(result, Err(MutationDispatchError::HandlerFailed));
            }
        }

        // A failing handler after a successful rewrite propagates its error;
        // the prior rewrite is dropped by the dispatch (Rust owns it).
        TOOL_ERROR_CANCELLED.with(|c| c.set(false));
        CURRENT_REWRITE.with(|c| c.set("{\"ok\":true}"));
        let mut chained = Runtime::init();
        chained
            .register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                name: "rewrite",
                ctx: 0,
                run: rewrite_current,
            })
            .unwrap();
        chained
            .register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                name: "error",
                ctx: 0,
                run: error_tool,
            })
            .unwrap();
        assert_eq!(
            chained.freeze().run_pre_tool_use(PreToolUseInput {
                invocation: test_invocation(),
                step_index: 1,
                call_id: "call",
                tool_name: "tool",
                arguments_json: "{}",
            }),
            Err(MutationDispatchError::HandlerFailed)
        );

        // Zig simulates an allocation failure with a FailingAllocator here; the
        // Rust port uses the infallible global allocator, so OutOfMemory is an
        // API-only variant that is never produced.
    }

    thread_local! {
        static STOP_CAPTURE: RefCell<StopCapture> = RefCell::new(StopCapture::default());
    }

    #[derive(Default)]
    struct StopCapture {
        calls: usize,
        third_called: bool,
    }

    fn stop_allow(_: HandlerContext, _: StopInput) -> Result<StopAction, HandlerError> {
        STOP_CAPTURE.with(|c| c.borrow_mut().calls += 1);
        Ok(StopAction::Allow)
    }

    fn stop_continue(_: HandlerContext, input: StopInput) -> Result<StopAction, HandlerError> {
        STOP_CAPTURE.with(|c| c.borrow_mut().calls += 1);
        Ok(StopAction::ContinueOnce(input.assistant_text))
    }

    fn stop_third(_: HandlerContext, _: StopInput) -> Result<StopAction, HandlerError> {
        STOP_CAPTURE.with(|c| c.borrow_mut().third_called = true);
        Ok(StopAction::Allow)
    }

    #[test]
    fn stop_returns_the_first_owned_continuation_and_honors_can_continue() {
        STOP_CAPTURE.with(|c| *c.borrow_mut() = StopCapture::default());
        let mut runtime = Runtime::init();
        runtime
            .register_stop(super::super::definitions::StopHandler {
                name: "allow",
                ctx: 0,
                run: stop_allow,
            })
            .unwrap();
        runtime
            .register_stop(super::super::definitions::StopHandler {
                name: "continue",
                ctx: 0,
                run: stop_continue,
            })
            .unwrap();
        runtime
            .register_stop(super::super::definitions::StopHandler {
                name: "never",
                ctx: 0,
                run: stop_third,
            })
            .unwrap();
        let view = runtime.freeze();

        let mut source = String::from("continue");
        let mut outcome = view.run_stop(StopInput {
            invocation: test_invocation(),
            step_index: 3,
            assistant_text: &source,
            provider_disposition: ProviderCompletionDisposition::Completed,
            can_continue: true,
        });
        source.replace_range(0..1, "X");
        assert_eq!(outcome, StopOutcome::ContinueOnce("continue".to_string()));
        outcome.deinit();
        STOP_CAPTURE.with(|c| {
            let c = c.borrow();
            assert_eq!(c.calls, 2);
            assert!(!c.third_called);
        });

        let mut normalized = view.run_stop(StopInput {
            invocation: test_invocation(),
            step_index: 3,
            assistant_text: "candidate",
            provider_disposition: ProviderCompletionDisposition::LengthLimited,
            can_continue: false,
        });
        assert_eq!(normalized, StopOutcome::Allow);
        normalized.deinit();
    }

    thread_local! {
        static STOP_ERROR_CANCELLED: Cell<bool> = Cell::new(false);
        static CURRENT_CONTINUE: Cell<&'static str> = Cell::new("");
    }

    fn stop_error(_: HandlerContext, _: StopInput) -> Result<StopAction, HandlerError> {
        if STOP_ERROR_CANCELLED.with(Cell::get) {
            Err(HandlerError::Cancelled)
        } else {
            Err(HandlerError::Failed)
        }
    }

    fn stop_output(_: HandlerContext, _: StopInput) -> Result<StopAction, HandlerError> {
        Ok(StopAction::ContinueOnce(CURRENT_CONTINUE.with(Cell::get)))
    }

    #[test]
    fn stop_fails_open_on_handler_errors_output_limits_and_copy_failures() {
        for cancelled in [false, true] {
            STOP_ERROR_CANCELLED.with(|c| c.set(cancelled));
            let mut runtime = Runtime::init();
            runtime
                .register_stop(super::super::definitions::StopHandler {
                    name: "error",
                    ctx: 0,
                    run: stop_error,
                })
                .unwrap();
            let mut handler_failed = runtime.freeze().run_stop(StopInput {
                invocation: test_invocation(),
                step_index: 1,
                assistant_text: "done",
                provider_disposition: ProviderCompletionDisposition::Completed,
                can_continue: true,
            });
            assert_eq!(handler_failed, StopOutcome::Allow);
            handler_failed.deinit();
        }

        let oversized: &'static str =
            Box::leak(" ".repeat(Limits::context_bytes() + 1).into_boxed_str());
        CURRENT_CONTINUE.with(|c| c.set(oversized));
        let mut runtime = Runtime::init();
        runtime
            .register_stop(super::super::definitions::StopHandler {
                name: "oversized",
                ctx: 0,
                run: stop_output,
            })
            .unwrap();
        let view = runtime.freeze();
        let mut oversized_outcome = view.run_stop(StopInput {
            invocation: test_invocation(),
            step_index: 1,
            assistant_text: "done",
            provider_disposition: ProviderCompletionDisposition::Completed,
            can_continue: true,
        });
        assert_eq!(oversized_outcome, StopOutcome::Allow);
        oversized_outcome.deinit();

        // The Zig copy-failure (FailingAllocator) branch is N/A in Rust; the
        // outcome is an owned `String` so copy failure cannot occur.
    }

    thread_local! {
        static POST_CAPTURE: RefCell<PostTurnEndCapture> = RefCell::new(PostTurnEndCapture::default());
    }

    #[derive(Default)]
    struct PostTurnEndCapture {
        order: Vec<u8>,
        saw_scope: bool,
        saw_turn_id: bool,
        saw_outcome: bool,
        saw_disposition: bool,
    }

    fn post_note(capture: &mut PostTurnEndCapture, value: u8) {
        capture.order.push(value);
    }

    fn post_first(_: HandlerContext, input: PostTurnEndInput) -> Result<(), HandlerError> {
        POST_CAPTURE.with(|c| {
            let mut c = c.borrow_mut();
            post_note(&mut c, 1);
            c.saw_scope = input.invocation.scope.kind == ScopeKind::Interactive
                && input.invocation.scope.workspace_root == "/tmp/workspace"
                && input.invocation.scope.session_id.as_deref() == Some("session");
            c.saw_turn_id = input.invocation.turn_id == Some(42);
            c.saw_outcome = input.outcome == TurnPresentationOutcome::Completed;
            c.saw_disposition =
                input.provider_disposition == Some(ProviderCompletionDisposition::LengthLimited);
        });
        Ok(())
    }

    fn post_failing(_: HandlerContext, _: PostTurnEndInput) -> Result<(), HandlerError> {
        POST_CAPTURE.with(|c| post_note(&mut c.borrow_mut(), 2));
        Err(HandlerError::Failed)
    }

    fn post_cancelled(_: HandlerContext, _: PostTurnEndInput) -> Result<(), HandlerError> {
        POST_CAPTURE.with(|c| post_note(&mut c.borrow_mut(), 3));
        Err(HandlerError::Cancelled)
    }

    fn post_fourth(_: HandlerContext, _: PostTurnEndInput) -> Result<(), HandlerError> {
        POST_CAPTURE.with(|c| post_note(&mut c.borrow_mut(), 4));
        Ok(())
    }

    #[test]
    fn post_turn_end_runs_side_effect_handlers_in_order_and_fails_open_on_handler_errors() {
        POST_CAPTURE.with(|c| *c.borrow_mut() = PostTurnEndCapture::default());
        let mut runtime = Runtime::init();
        runtime
            .register_post_turn_end(super::super::definitions::PostTurnEndHandler {
                name: "first",
                ctx: 0,
                run: post_first,
            })
            .unwrap();
        runtime
            .register_post_turn_end(super::super::definitions::PostTurnEndHandler {
                name: "failing",
                ctx: 0,
                run: post_failing,
            })
            .unwrap();
        runtime
            .register_post_turn_end(super::super::definitions::PostTurnEndHandler {
                name: "cancelled",
                ctx: 0,
                run: post_cancelled,
            })
            .unwrap();
        runtime
            .register_post_turn_end(super::super::definitions::PostTurnEndHandler {
                name: "fourth",
                ctx: 0,
                run: post_fourth,
            })
            .unwrap();

        runtime.freeze().run_post_turn_end(PostTurnEndInput {
            invocation: test_invocation(),
            outcome: TurnPresentationOutcome::Completed,
            provider_disposition: Some(ProviderCompletionDisposition::LengthLimited),
        });

        POST_CAPTURE.with(|c| {
            let c = c.borrow();
            assert_eq!(c.order, vec![1, 2, 3, 4]);
            assert!(c.saw_scope);
            assert!(c.saw_turn_id);
            assert!(c.saw_outcome);
            assert!(c.saw_disposition);
        });
    }

    thread_local! {
        static ATTENTION_CAPTURE: RefCell<AttentionRequiredCapture> =
            RefCell::new(AttentionRequiredCapture::default());
    }

    #[derive(Default)]
    struct AttentionRequiredCapture {
        order: Vec<u8>,
        saw_input: bool,
    }

    fn attention_first(_: HandlerContext, input: AttentionRequiredInput) -> Result<(), HandlerError> {
        ATTENTION_CAPTURE.with(|c| {
            let mut c = c.borrow_mut();
            c.order.push(1);
            c.saw_input = input.invocation.scope.kind == ScopeKind::Interactive
                && input.invocation.turn_id == Some(42)
                && input.kind == AttentionKind::RouteRecovery;
        });
        Ok(())
    }

    fn attention_failing(_: HandlerContext, _: AttentionRequiredInput) -> Result<(), HandlerError> {
        ATTENTION_CAPTURE.with(|c| c.borrow_mut().order.push(2));
        Err(HandlerError::Failed)
    }

    fn attention_cancelled(_: HandlerContext, _: AttentionRequiredInput) -> Result<(), HandlerError> {
        ATTENTION_CAPTURE.with(|c| c.borrow_mut().order.push(3));
        Err(HandlerError::Cancelled)
    }

    fn attention_fourth(_: HandlerContext, _: AttentionRequiredInput) -> Result<(), HandlerError> {
        ATTENTION_CAPTURE.with(|c| c.borrow_mut().order.push(4));
        Ok(())
    }

    #[test]
    fn attention_required_runs_in_order_propagates_input_and_fails_open() {
        ATTENTION_CAPTURE.with(|c| *c.borrow_mut() = AttentionRequiredCapture::default());
        let mut runtime = Runtime::init();
        runtime
            .register_attention_required(super::super::definitions::AttentionRequiredHandler {
                name: "first",
                ctx: 0,
                run: attention_first,
            })
            .unwrap();
        runtime
            .register_attention_required(super::super::definitions::AttentionRequiredHandler {
                name: "failing",
                ctx: 0,
                run: attention_failing,
            })
            .unwrap();
        runtime
            .register_attention_required(super::super::definitions::AttentionRequiredHandler {
                name: "cancelled",
                ctx: 0,
                run: attention_cancelled,
            })
            .unwrap();
        runtime
            .register_attention_required(super::super::definitions::AttentionRequiredHandler {
                name: "fourth",
                ctx: 0,
                run: attention_fourth,
            })
            .unwrap();

        runtime.freeze().run_attention_required(AttentionRequiredInput {
            invocation: test_invocation(),
            kind: AttentionKind::RouteRecovery,
        });

        ATTENTION_CAPTURE.with(|c| {
            let c = c.borrow();
            assert_eq!(c.order, vec![1, 2, 3, 4]);
            assert!(c.saw_input);
        });
    }

    static CONCURRENT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static CONCURRENT_FAILED: AtomicBool = AtomicBool::new(false);

    fn concurrent_run(_: HandlerContext, _: PreToolUseInput) -> Result<PreToolUseAction, HandlerError> {
        CONCURRENT_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(PreToolUseAction::Continue)
    }

    fn run_concurrent(view: &RuntimeView, input: PreToolUseInput) {
        match view.run_pre_tool_use(input) {
            Ok(mut outcome) => outcome.deinit(),
            Err(_) => {
                CONCURRENT_FAILED.store(true, Ordering::SeqCst);
            }
        }
    }

    #[test]
    fn a_frozen_view_is_safe_to_copy_across_concurrent_invocations() {
        CONCURRENT_CALLS.store(0, Ordering::SeqCst);
        CONCURRENT_FAILED.store(false, Ordering::SeqCst);
        let mut runtime = Runtime::init();
        runtime
            .register_pre_tool_use(super::super::definitions::PreToolUseHandler {
                name: "concurrent",
                ctx: 0,
                run: concurrent_run,
            })
            .unwrap();
        let view = runtime.freeze();

        std::thread::scope(|scope| {
            let input = PreToolUseInput {
                invocation: test_invocation(),
                step_index: 1,
                call_id: "call",
                tool_name: "tool",
                arguments_json: "{}",
            };
            let i1 = input.clone();
            let i2 = input;
            let first = scope.spawn(move || run_concurrent(&view, i1));
            let second = scope.spawn(move || run_concurrent(&view, i2));
            first.join().unwrap();
            second.join().unwrap();
        });

        assert!(!CONCURRENT_FAILED.load(Ordering::SeqCst));
        assert_eq!(CONCURRENT_CALLS.load(Ordering::SeqCst), 2);
    }
}
