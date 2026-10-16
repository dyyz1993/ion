/// User-triggered actions dispatched to the host via RPC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Pause { run_id: String, reason: String },
    Resume { run_id: String },
    ToggleAutoResume { run_id: String, enabled: bool },
    Quit,
}

impl Action {
    /// Map a key press to an action for the selected run.
    pub fn from_key(key: &str, run_id: Option<&str>) -> Option<Action> {
        if key == "q" {
            return Some(Action::Quit);
        }
        let run_id = run_id?;
        match key {
            "p" => Some(Action::Pause {
                run_id: run_id.to_string(),
                reason: "tui".into(),
            }),
            "r" => Some(Action::Resume {
                run_id: run_id.to_string(),
            }),
            "a" => Some(Action::ToggleAutoResume {
                run_id: run_id.to_string(),
                enabled: true,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_actions_only_with_selection() {
        assert_eq!(
            Action::from_key("p", Some("run-1")),
            Some(Action::Pause {
                run_id: "run-1".into(),
                reason: "tui".into()
            })
        );
        assert_eq!(
            Action::from_key("r", Some("run-1")),
            Some(Action::Resume {
                run_id: "run-1".into()
            })
        );
        assert_eq!(
            Action::from_key("a", Some("run-1")),
            Some(Action::ToggleAutoResume {
                run_id: "run-1".into(),
                enabled: true
            })
        );
        assert_eq!(Action::from_key("q", None), Some(Action::Quit));
        assert_eq!(Action::from_key("p", None), None);
        assert_eq!(Action::from_key("x", Some("run-1")), None);
    }
}
