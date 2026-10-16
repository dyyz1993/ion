/// Session tree model built from host RPC data (list_all_sessions + list_sessions).
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct SessionInfo {
    #[serde(alias = "sessionId")]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, alias = "firstMessage")]
    pub first_message: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default, alias = "parentSession")]
    pub parent_session: Option<String>,
    #[serde(default, alias = "parentType")]
    pub parent_type: Option<String>,
    #[serde(default, alias = "updatedAt")]
    pub updated_at: i64,
    #[serde(default, alias = "messageCount")]
    #[allow(dead_code)]
    pub message_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionKind {
    Root,
    Child,
    Peer,
    System,
}

/// One rendered row of the sessions tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRow {
    pub session_id: String,
    pub label: String,
    pub indent: usize,
    pub kind: SessionKind,
    pub busy: bool,
    pub durable_active: bool,
}

fn project_of(s: &SessionInfo) -> String {
    s.project
        .clone()
        .or_else(|| s.name.clone())
        .unwrap_or_else(|| "(no project)".into())
}

fn label_of(s: &SessionInfo) -> String {
    // 显示优先级：用户/Agent 生成标题(name) → 首条消息摘要 → id
    if let Some(n) = s.name.as_deref().filter(|n| !n.is_empty()) {
        return n.to_string();
    }
    if let Some(f) = s.first_message.as_deref().filter(|f| !f.is_empty()) {
        let t = truncate_label(f);
        return t;
    }
    s.id.clone()
}

fn truncate_label(s: &str) -> String {
    let cleaned = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() <= 40 {
        cleaned
    } else {
        let cut: String = cleaned.chars().take(40).collect();
        format!("{cut}…")
    }
}

fn kind_of(s: &SessionInfo) -> SessionKind {
    if s.id.contains("memory_agent")
        || s.name
            .as_deref()
            .is_some_and(|n| n.contains("memory_agent"))
    {
        SessionKind::System
    } else {
        match s.parent_type.as_deref() {
            Some("peer") => SessionKind::Peer,
            Some("child") | Some("fork") => SessionKind::Child,
            _ => SessionKind::Root,
        }
    }
}

/// Build tree rows: group by project (stable by first-seen), roots first then
/// children (indent 1) then peers (indent 2). Orphan children fall into an
/// "(orphan)" group. System sessions (memory-agent etc) go to a trailing
/// "(system)" group, sorted last within the project.
pub fn build_tree(
    sessions: &[SessionInfo],
    busy_ids: &std::collections::HashSet<String>,
    durable_active_ids: &std::collections::HashSet<String>,
) -> Vec<TreeRow> {
    let mut groups: Vec<(String, Vec<&SessionInfo>)> = Vec::new();
    let mut orphans: Vec<&SessionInfo> = Vec::new();
    let mut index_by_id: std::collections::HashMap<String, usize> = Default::default();
    for (i, s) in sessions.iter().enumerate() {
        index_by_id.insert(s.id.clone(), i);
    }
    for s in sessions {
        if kind_of(s) == SessionKind::System {
            continue;
        }
        let Some(parent) = s.parent_session.as_deref() else {
            let key = project_of(s);
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, v)) => v.push(s),
                None => groups.push((key, vec![s])),
            }
            continue;
        };
        let parent_known = sessions.iter().any(|p| p.id == parent);
        if parent_known {
            let parent = sessions.iter().find(|p| p.id == parent).unwrap();
            let key = project_of(parent);
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, v)) => v.push(s),
                None => groups.push((key, vec![s])),
            }
        } else {
            orphans.push(s);
        }
    }

    let mut rows = Vec::new();
    for (project, members) in &groups {
        rows.push(TreeRow {
            session_id: format!("group:{project}"),
            label: project.clone(),
            indent: 0,
            kind: SessionKind::Root,
            busy: false,
            durable_active: false,
        });
        let mut roots: Vec<&&SessionInfo> = members
            .iter()
            .filter(|s| kind_of(s) == SessionKind::Root)
            .collect();
        roots.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        for root in roots {
            rows.push(render_session(root, 1, busy_ids, durable_active_ids));
            let mut children: Vec<&&SessionInfo> = members
                .iter()
                .filter(|s| s.parent_session.as_deref() == Some(root.id.as_str()))
                .collect();
            children.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
            for child in children {
                rows.push(render_session(child, 2, busy_ids, durable_active_ids));
            }
        }
        // Children whose parent is in the same project but not a rendered root
        // (e.g. parent is another project's session) — render at indent 1.
        for s in members.iter().filter(|s| kind_of(s) != SessionKind::Root) {
            if !rows.iter().any(|r| r.session_id == s.id) {
                rows.push(render_session(s, 1, busy_ids, durable_active_ids));
            }
        }
    }
    if !orphans.is_empty() {
        rows.push(TreeRow {
            session_id: "group:(orphan)".into(),
            label: "(orphan)".into(),
            indent: 0,
            kind: SessionKind::Root,
            busy: false,
            durable_active: false,
        });
        for s in &orphans {
            rows.push(render_session(s, 1, busy_ids, durable_active_ids));
        }
    }
    let mut systems: Vec<&SessionInfo> = sessions
        .iter()
        .filter(|s| kind_of(s) == SessionKind::System)
        .collect();
    if !systems.is_empty() {
        systems.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        rows.push(TreeRow {
            session_id: "group:(system)".into(),
            label: "(system)".into(),
            indent: 0,
            kind: SessionKind::Root,
            busy: false,
            durable_active: false,
        });
        for s in systems {
            rows.push(render_session(s, 1, busy_ids, durable_active_ids));
        }
    }
    rows
}

fn render_session(
    s: &SessionInfo,
    indent: usize,
    busy_ids: &std::collections::HashSet<String>,
    durable_active_ids: &std::collections::HashSet<String>,
) -> TreeRow {
    let bullet = match kind_of(s) {
        SessionKind::Peer => "◦",
        SessionKind::Child => "•",
        _ => "",
    };
    let label = if bullet.is_empty() {
        label_of(s)
    } else {
        format!("{bullet} {}", label_of(s))
    };
    TreeRow {
        session_id: s.id.clone(),
        label,
        indent,
        kind: kind_of(s),
        busy: busy_ids.contains(&s.id),
        durable_active: durable_active_ids.contains(&s.id),
    }
}

pub fn parse_sessions(response: &serde_json::Value) -> Vec<SessionInfo> {
    response
        .get("data")
        .and_then(|d| d.get("sessions"))
        .and_then(|s| s.as_array())
        .map(|arr| serde_json::from_value(arr.clone().into()).unwrap_or_default())
        .unwrap_or_default()
}

pub fn parse_busy_ids(list_sessions_resp: &serde_json::Value) -> std::collections::HashSet<String> {
    list_sessions_resp
        .get("data")
        .and_then(|d| d.get("sessions"))
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|w| w.get("status").and_then(|s| s.as_str()) == Some("Busy"))
                .filter_map(|w| w.get("session_id").and_then(|s| s.as_str()))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(id: &str, project: &str, parent: Option<&str>, ptype: Option<&str>) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            name: Some(id.into()),
            first_message: None,
            project: Some(project.into()),
            parent_session: parent.map(String::from),
            parent_type: ptype.map(String::from),
            updated_at: 1,
            message_count: 0,
        }
    }

    #[test]
    fn label_prefers_name_then_first_message_then_id() {
        let with_name = SessionInfo {
            name: Some("修复登录超时".into()),
            first_message: Some("请帮我看看登录超时的问题很奇怪".into()),
            ..sess("s1", "/p", None, None)
        };
        assert_eq!(label_of(&with_name), "修复登录超时");
        let untitled = SessionInfo {
            name: None,
            first_message: Some(
                "这是一个超过四十个字符宽度的首条消息样例，用于验证截断逻辑是否会在四十个字符处停下并添加省略号，确实非常长非常长非常长。".into(),
            ),
            ..sess("s2", "/p", None, None)
        };
        let l = label_of(&untitled);
        assert!(
            l.ends_with('…') && l.chars().count() == 41,
            "got: {l} len={}",
            l.chars().count()
        );
        let bare = SessionInfo {
            name: None,
            first_message: None,
            ..sess("s3", "/p", None, None)
        };
        assert_eq!(label_of(&bare), "s3");
    }

    #[test]
    fn parse_maps_first_message_alias() {
        let resp = serde_json::json!({"data": {"sessions": [
            {"id": "s1", "firstMessage": "帮我写个解析器", "name": null}
        ]}});
        let sessions = parse_sessions(&resp);
        assert_eq!(sessions[0].first_message.as_deref(), Some("帮我写个解析器"));
    }

    #[test]
    fn tree_groups_by_project_and_indents_child_peer() {
        let sessions = vec![
            sess("root-a", "/p/a", None, None),
            sess("child-a", "/p/a", Some("root-a"), Some("child")),
            sess("peer-a", "/p/a", Some("root-a"), Some("peer")),
            sess("root-b", "/p/b", None, None),
        ];
        let rows = build_tree(&sessions, &Default::default(), &Default::default());
        let ids: Vec<&str> = rows.iter().map(|r| r.session_id.as_str()).collect();
        assert!(ids.contains(&"group:/p/a"));
        assert!(ids.contains(&"group:/p/b"));
        let child = rows.iter().find(|r| r.session_id == "child-a").unwrap();
        let peer = rows.iter().find(|r| r.session_id == "peer-a").unwrap();
        let root = rows.iter().find(|r| r.session_id == "root-a").unwrap();
        assert_eq!(root.indent, 1);
        assert_eq!(child.indent, 2);
        assert_eq!(peer.indent, 2);
        assert_eq!(child.kind, SessionKind::Child);
        assert_eq!(peer.kind, SessionKind::Peer);
    }

    #[test]
    fn orphan_child_lands_in_orphan_group() {
        let sessions = vec![sess(
            "orphan",
            "/p/x",
            Some("missing-parent"),
            Some("child"),
        )];
        let rows = build_tree(&sessions, &Default::default(), &Default::default());
        assert!(rows.iter().any(|r| r.session_id == "group:(orphan)"));
        let orphan = rows.iter().find(|r| r.session_id == "orphan").unwrap();
        assert_eq!(orphan.indent, 1);
    }

    #[test]
    fn system_sessions_grouped_last() {
        let sessions = [
            sess("mem-agent-1", "/p/a", None, None),
            sess("root-a", "/p/a", None, None),
        ];
        let sessions = vec![
            SessionInfo {
                name: Some("memory_agent".into()),
                ..sess("mem-agent-1", "/p/a", None, None)
            },
            sessions[1].clone(),
        ];
        let rows = build_tree(&sessions, &Default::default(), &Default::default());
        let last = rows.last().unwrap();
        assert_eq!(last.session_id, "mem-agent-1");
        assert!(rows.iter().any(|r| r.session_id == "group:(system)"));
    }

    #[test]
    fn badges_from_busy_and_durable_sets() {
        let sessions = vec![sess("s1", "/p", None, None)];
        let mut busy = std::collections::HashSet::new();
        busy.insert("s1".to_string());
        let mut durable = std::collections::HashSet::new();
        durable.insert("s1".to_string());
        let rows = build_tree(&sessions, &busy, &durable);
        let row = rows.iter().find(|r| r.session_id == "s1").unwrap();
        assert!(row.busy && row.durable_active);
    }

    #[test]
    fn parse_sessions_and_busy_ids() {
        let resp = serde_json::json!({
            "data": {"sessions": [
                {"id": "s1", "project": "/p", "parentSession": null, "parentType": null, "updatedAt": 1},
                {"id": "s2", "project": "/p", "parentSession": "s1", "parentType": "child", "updatedAt": 2}
            ]}
        });
        let sessions = parse_sessions(&resp);
        assert_eq!(sessions.len(), 2);
        let ls = serde_json::json!({"data":{"sessions":[
            {"session_id":"s1","status":"Busy"},
            {"session_id":"s2","status":"Idle"}
        ]}});
        let busy = parse_busy_ids(&ls);
        assert!(busy.contains("s1"));
        assert!(!busy.contains("s2"));
    }
}
