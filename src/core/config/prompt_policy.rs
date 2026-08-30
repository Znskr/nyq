//! Prompt policy and model prompt overlays.
//!
//! Mirrors `zig-reference/src/core/config/prompt_policy.zig`.

pub type ModelPromptOverlayFn = for<'a> fn(&'a str) -> Option<&'a str>;

pub struct Policy<'a> {
    pub system_prompt: &'a str,
    pub model_prompt_overlay_fn: Option<ModelPromptOverlayFn>,
}

impl<'a> Default for Policy<'a> {
    fn default() -> Self {
        Policy {
            system_prompt: "",
            model_prompt_overlay_fn: None,
        }
    }
}

impl<'a> Policy<'a> {
    pub fn model_prompt_overlay<'m>(&self, model: &'m str) -> Option<&'m str> {
        let overlay_fn = self.model_prompt_overlay_fn?;
        overlay_fn(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_policy_resolves_the_supplied_model_overlay() {
        fn overlay(model: &str) -> Option<&str> {
            if !model.is_empty() {
                Some("overlay")
            } else {
                None
            }
        }

        let policy = Policy {
            system_prompt: "system",
            model_prompt_overlay_fn: Some(overlay),
        };

        assert_eq!(Some("overlay"), policy.model_prompt_overlay("model"));
        assert_eq!(None, policy.model_prompt_overlay(""));
    }
}
