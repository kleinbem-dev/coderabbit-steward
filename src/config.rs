use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Observe,
    OnDemand,
    Full,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Observe => "observe",
            Mode::OnDemand => "on-demand",
            Mode::Full => "full",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "observe" => Ok(Mode::Observe),
            "on-demand" => Ok(Mode::OnDemand),
            "full" => Ok(Mode::Full),
            other => anyhow::bail!("invalid mode: {other} (expected observe, on-demand, or full)"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: String,
    pub database_url: String,
    pub web_dir: Option<PathBuf>,
    pub owners: Vec<String>,
    pub mode: Mode,
    pub poll_interval_secs: u64,
    pub github_app_id: Option<u64>,
    pub github_private_key_path: Option<PathBuf>,
    pub github_private_key_pem: Option<String>,
    pub github_token: Option<String>,
    pub github_user_token: Option<String>,
    pub github_webhook_secret: Option<String>,
    pub api_token: Option<String>,
    pub github_api_base: String,
    pub ntfy_url: Option<String>,
    pub ntfy_token: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8484".to_string(),
            database_url: "sqlite://coderabbit-steward.db?mode=rwc".to_string(),
            web_dir: None,
            owners: vec!["kleinbem".to_string(), "kleinbem-dev".to_string()],
            mode: Mode::Observe,
            poll_interval_secs: 60,
            github_app_id: None,
            github_private_key_path: None,
            github_private_key_pem: None,
            github_token: None,
            github_user_token: None,
            github_webhook_secret: None,
            api_token: None,
            github_api_base: "https://api.github.com".to_string(),
            ntfy_url: None,
            ntfy_token: None,
        }
    }
}

/// Resolve the static web directory from explicit config, binary-adjacent share dir, or `./web/build`.
pub fn resolve_web_dir(configured: Option<&PathBuf>) -> Option<PathBuf> {
    if let Some(dir) = configured
        && dir.is_dir()
    {
        return Some(dir.clone());
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(share_web) = exe
            .parent()
            .and_then(|p| p.parent())
            .map(|prefix| prefix.join("share/coderabbit-steward/web"))
        && share_web.is_dir()
    {
        return Some(share_web);
    }
    let local = PathBuf::from("web/build");
    if local.is_dir() {
        return Some(local);
    }
    None
}
