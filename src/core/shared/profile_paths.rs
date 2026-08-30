pub const root_dir_name: &str = ".fx";
pub const auth_file_name: &str = "auth.json";
pub const chatgpt_auth_file_name: &str = "chatgpt-auth.json";
pub const grok_auth_file_name: &str = "grok-auth.json";
pub const api_key_file_name: &str = "api-key";
pub const sessions_dir_name: &str = "sessions";
pub const prompt_history_file_name: &str = "history.jsonl";
pub const usage_file_name: &str = "usage.jsonl";
pub const usage_recovery_dir_name: &str = "usage-recovery";
pub const backups_dir_name: &str = "backups";
pub const mcp_credentials_dir_name: &str = "mcp-credentials";
pub const mcp_credentials_file_name: &str = "credentials.json";

const settings_file_name: &str = "settings.json";
const mcp_config_file_name: &str = "mcp.json";
const managed_skills_dir_name: &str = "skills";
const memories_file_name: &str = "memories.json";
const logs_dir_name: &str = "logs";
const trace_log_file_name: &str = "trace.log";
const recordings_dir_name: &str = "recordings";

fn join_path(parts: &[&str]) -> String {
    parts.join("/")
}

pub fn root_dir(home: &str) -> String {
    join_path(&[home, root_dir_name])
}

pub fn settings_path(home: &str) -> String {
    join_path(&[home, root_dir_name, settings_file_name])
}

pub fn mcp_config_path(home: &str) -> String {
    join_path(&[home, root_dir_name, mcp_config_file_name])
}

pub fn mcp_credentials_dir(home: &str) -> String {
    join_path(&[home, root_dir_name, mcp_credentials_dir_name])
}

pub fn mcp_credentials_path(home: &str) -> String {
    join_path(&[
        home,
        root_dir_name,
        mcp_credentials_dir_name,
        mcp_credentials_file_name,
    ])
}

pub fn managed_skills_dir(home: &str) -> String {
    join_path(&[home, root_dir_name, managed_skills_dir_name])
}

pub fn auth_path(home: &str) -> String {
    join_path(&[home, root_dir_name, auth_file_name])
}

pub fn chatgpt_auth_path(home: &str) -> String {
    join_path(&[home, root_dir_name, chatgpt_auth_file_name])
}

pub fn grok_auth_path(home: &str) -> String {
    join_path(&[home, root_dir_name, grok_auth_file_name])
}

pub fn api_key_path(home: &str) -> String {
    join_path(&[home, root_dir_name, api_key_file_name])
}

pub fn sessions_dir(home: &str) -> String {
    join_path(&[home, root_dir_name, sessions_dir_name])
}

pub fn prompt_history_path(home: &str) -> String {
    join_path(&[home, root_dir_name, prompt_history_file_name])
}

pub fn memories_path(home: &str) -> String {
    join_path(&[home, root_dir_name, memories_file_name])
}

pub fn backups_dir(home: &str) -> String {
    join_path(&[home, root_dir_name, backups_dir_name])
}

pub fn logs_dir(home: &str) -> String {
    join_path(&[home, root_dir_name, logs_dir_name])
}

pub fn trace_log_path(home: &str) -> String {
    join_path(&[home, root_dir_name, logs_dir_name, trace_log_file_name])
}

pub fn recordings_dir(home: &str) -> String {
    join_path(&[home, root_dir_name, recordings_dir_name])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_path_helpers_preserve_current_default_locations() {
        assert_eq!("/tmp/fake-home/.fx", root_dir("/tmp/fake-home"));
        assert_eq!("/tmp/fake-home/.fx/settings.json", settings_path("/tmp/fake-home"));
        assert_eq!("/tmp/fake-home/.fx/mcp.json", mcp_config_path("/tmp/fake-home"));
        assert_eq!("/tmp/fake-home/.fx/mcp-credentials", mcp_credentials_dir("/tmp/fake-home"));
        assert_eq!(
            "/tmp/fake-home/.fx/mcp-credentials/credentials.json",
            mcp_credentials_path("/tmp/fake-home")
        );
        assert_eq!("/tmp/fake-home/.fx/skills", managed_skills_dir("/tmp/fake-home"));
        assert_eq!("/tmp/fake-home/.fx/auth.json", auth_path("/tmp/fake-home"));
        assert_eq!("/tmp/fake-home/.fx/chatgpt-auth.json", chatgpt_auth_path("/tmp/fake-home"));
        assert_eq!("/tmp/fake-home/.fx/api-key", api_key_path("/tmp/fake-home"));
        assert_eq!("/tmp/fake-home/.fx/sessions", sessions_dir("/tmp/fake-home"));
        assert_eq!("/tmp/fake-home/.fx/history.jsonl", prompt_history_path("/tmp/fake-home"));
        assert_eq!("/tmp/fake-home/.fx/memories.json", memories_path("/tmp/fake-home"));
        assert_eq!("/tmp/fake-home/.fx/backups", backups_dir("/tmp/fake-home"));
        assert_eq!("/tmp/fake-home/.fx/logs", logs_dir("/tmp/fake-home"));
        assert_eq!("/tmp/fake-home/.fx/logs/trace.log", trace_log_path("/tmp/fake-home"));
        assert_eq!("/tmp/fake-home/.fx/recordings", recordings_dir("/tmp/fake-home"));
    }
}
