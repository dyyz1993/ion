use std::collections::{HashMap, HashSet};

/// Persisted TUI state (~/.ion/tui-state.json)
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct TuiState {
    pub collapsed_projects: HashSet<String>,
    pub drafts: HashMap<String, String>,
    pub last_selected_session: Option<String>,
}

fn state_path() -> std::path::PathBuf {
    let base = crate::paths::root();
    base.join("tui-state.json")
}

impl TuiState {
    pub fn load() -> Self {
        let path = state_path();
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(state_path(), s);
        }
    }
}
