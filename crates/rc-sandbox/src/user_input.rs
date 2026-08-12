//! User input hook used by the `ask_user` tool during entropy-reduction
//! planning. Interactive frontends supply a real prompt; headless servers
//! fall back to a conservative default so runs never deadlock.

use async_trait::async_trait;

#[async_trait]
pub trait UserInputHook: Send + Sync {
    async fn ask(&self, question: &str) -> String;
}

pub struct AutoUserHook {
    pub default_response: String,
}

impl Default for AutoUserHook {
    fn default() -> Self {
        Self {
            default_response: "No interactive user is attached. Proceed with your best judgment and state any remaining assumptions in the plan.".to_string(),
        }
    }
}

#[async_trait]
impl UserInputHook for AutoUserHook {
    async fn ask(&self, _question: &str) -> String {
        self.default_response.clone()
    }
}

pub struct PromptUserHook<F> {
    f: F,
}

impl<F> PromptUserHook<F>
where
    F: Fn(&str) -> String + Send + Sync,
{
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

#[async_trait]
impl<F> UserInputHook for PromptUserHook<F>
where
    F: Fn(&str) -> String + Send + Sync,
{
    async fn ask(&self, question: &str) -> String {
        (self.f)(question)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auto_hook_returns_default() {
        let hook = AutoUserHook::default();
        assert!(hook.ask("question?").await.contains("best judgment"));
    }

    #[tokio::test]
    async fn prompt_hook_forwards_question() {
        let hook = PromptUserHook::new(|q| format!("answered: {q}"));
        assert_eq!(hook.ask("really?").await, "answered: really?");
    }
}
