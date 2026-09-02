//! Runtime capability guidance injected into the browser-host model context
//! (ported 1:1 from zig-reference/src/core/hosts/browser_capabilities.zig).

#[allow(non_upper_case_globals)]
pub const model_context: &str = concat!(
    "Runtime capabilities: this is the embedded browser version of fx, not locally installed fx. ",
    "Public web fetch, web search, and general outbound network access are unavailable. ",
    "Do not attempt curl, wget, package managers, raw sockets, or equivalent network workarounds through terminal commands. ",
    "If the user asks for external web content, explain this limitation immediately and say that locally installed fx provides the full tool suite. ",
    "When a browser workspace terminal is advertised, use it only for the files and commands that workspace exposes.",
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_model_context_refuses_unavailable_network_workarounds() {
        assert!(model_context.contains(
            "Public web fetch, web search, and general outbound network access are unavailable"
        ));
        assert!(model_context.contains("Do not attempt curl, wget"));
        assert!(model_context.contains("locally installed fx provides the full tool suite"));
    }
}
