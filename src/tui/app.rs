use std::collections::{HashMap, HashSet, VecDeque};
use crossterm::event::{self as ce, Event, KeyCode, KeyEventKind};
use ratatui::{
    Terminal, backend::{CrosstermBackend, Backend},
};
use serde_json::Value;
use tokio::time::{interval, Duration};

use crate::tui::{
    manager_conn::ManagerConn,
    layout::{self, LayoutTier},
    view,
    state::TuiState,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeId {
    Project(String),
    Session(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    Tree,
    Kanban,
    Detail,
    Input,
}

pub struct AppState {
    // Data
    pub workers: Vec<Value>,
    pub projects: Vec<Value>,
    pub total_workers: usize,
    pub total_projects: usize,
    pub total_stale: usize,

    // State
    pub selected: Option<NodeId>,
    pub focused_panel: Panel,
    pub focus_mode: bool,
    pub collapsed: HashSet<String>,
    pub drafts: HashMap<String, String>,
    pub log_buffers: HashMap<String, VecDeque<String>>,

    // Input
    pub input_text: String,
    pub input_cursor: usize,

    // Connection
    pub conn: ManagerConn,
    pub connected: bool,

    // Layout
    pub layout_tier: LayoutTier,
    pub term_width: u16,
    pub term_height: u16,

    // Persistence
    pub tui_state: TuiState,

    // Active session (for sending chat)
    pub active_session: Option<String>,

    // Animation
    pub anim_frame: u8,

    // Send queue (buffered sends from keyboard handler)
    pending_sends: Vec<(String, String)>,

    // Tree navigation
    pub tree_items: Vec<NodeId>,  // ordered list of visible tree items
    pub tree_index: usize,        // currently highlighted tree index
    pub kanban_selected: Option<usize>,  // index into workers vec

    // Quit
    pub should_quit: bool,
}

impl AppState {
    pub fn new(conn: ManagerConn, overview: Value) -> Self {
        let tui_state = TuiState::load();
        let mut st = Self {
            workers: vec![],
            projects: vec![],
            total_workers: 0,
            total_projects: 0,
            total_stale: 0,
            selected: None,
            focused_panel: Panel::Tree,
            focus_mode: false,
            collapsed: tui_state.collapsed_projects.clone(),
            drafts: tui_state.drafts.clone(),
            log_buffers: HashMap::new(),
            input_text: tui_state.last_selected_session.as_ref()
                .and_then(|s| tui_state.drafts.get(s))
                .cloned()
                .unwrap_or_default(),
            input_cursor: 0,
            conn,
            connected: true,
            layout_tier: LayoutTier::Wide,
            term_width: 140,
            term_height: 40,
            tui_state,
            active_session: None,
            anim_frame: 0,
            pending_sends: vec![],
            tree_items: vec![],
            tree_index: 0,
            kanban_selected: None,
            should_quit: false,
        };
        st.apply_overview(overview);
        st
    }

    fn apply_overview(&mut self, data: Value) {
        if let Some(workers) = data.get("workers").and_then(|v| v.as_array()) {
            self.workers = workers.clone();
            self.total_workers = workers.len();
        }
        if let Some(projects) = data.get("projects").and_then(|v| v.as_array()) {
            self.projects = projects.clone();
            self.total_projects = projects.len();
        }
        if let Some(stale) = data.get("total_stale").and_then(|v| v.as_u64()) {
            self.total_stale = stale as usize;
        }
        // Rebuild tree items
        self.rebuild_tree();
    }

    fn rebuild_tree(&mut self) {
        self.tree_items.clear();
        for proj in &self.projects {
            let pname = proj.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            self.tree_items.push(NodeId::Project(pname.clone()));
            if !self.collapsed.contains(&pname) {
                for w in &self.workers {
                    let proj_name = w.get("project").and_then(|v| v.as_str()).unwrap_or("");
                    if proj_name != pname { continue; }
                    let sid = w.get("session_id").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                    self.tree_items.push(NodeId::Session(sid));
                }
            }
        }
    }

    /// Enter the TUI, run event loop
    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        use crossterm::terminal::{enable_raw_mode, disable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
        use crossterm::execute;
        use std::io::stdout;

        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

        // Handle terminal resize on start
        let (w, h) = crossterm::terminal::size()?;
        self.term_width = w;
        self.term_height = h;
        self.layout_tier = LayoutTier::from_width(w);

        let result = self.run_inner(&mut terminal).await;

        // Exit terminal
        let _ = execute!(stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();

        // Save state
        self.tui_state.collapsed_projects = self.collapsed.clone();
        self.tui_state.drafts = self.drafts.clone();
        self.tui_state.save();

        result
    }

    async fn run_inner<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<(), Box<dyn std::error::Error>> {
        let mut tick = interval(Duration::from_millis(250));
        loop {
            // Process pending sends
            if self.focused_panel == Panel::Input && !self.pending_sends.is_empty() {
                for (sid, text) in self.pending_sends.drain(..) {
                    self.drafts.insert(sid.clone(), text.clone());
                    let _ = self.conn.send_prompt(&sid, &text).await;
                }
            }

            tokio::select! {
                _ = tick.tick() => {
                    self.on_tick().await;
                }
                result = tokio::task::spawn_blocking(|| ce::read()) => {
                    match result {
                        Ok(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                            self.on_key(key);
                        }
                        Ok(Ok(Event::Resize(w, h))) => {
                            self.term_width = w;
                            self.term_height = h;
                            self.layout_tier = LayoutTier::from_width(w);
                        }
                        Ok(Ok(_)) => {}
                        _ => { self.should_quit = true; }
                    }
                }
            }

            if self.should_quit { break; }

            // Render
            terminal.draw(|f| {
                let l = layout::compute_layout(f.area(), self.layout_tier, self.focus_mode);
                view::render(f, self, &l);
            })?;
        }
        Ok(())
    }

    async fn on_tick(&mut self) {
        self.anim_frame = (self.anim_frame + 1) % 8;
        if let Ok(data) = self.conn.poll_overview().await {
            self.connected = true;
            let prev_count = self.workers.len();
            self.apply_overview(data);
            if self.workers.len() > prev_count {
                for w in &self.workers {
                    if let Some(sid) = w.get("session_id").and_then(|v| v.as_str()) {
                        self.log_buffers.entry(sid.to_string())
                            .or_insert_with(|| VecDeque::with_capacity(20));
                    }
                }
            }
        } else {
            self.connected = false;
        }
    }

    fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        if self.focused_panel == Panel::Input {
            self.handle_input_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') => { self.should_quit = true; }
            KeyCode::Tab => { self.focus_next(); }
            KeyCode::BackTab => { self.focus_prev(); }
            KeyCode::Char('d') => { self.toggle_focus(); }
            KeyCode::Esc => { self.focus_mode = false; self.focused_panel = Panel::Tree; }
            KeyCode::Enter => { self.on_enter(); }
            KeyCode::Up | KeyCode::Char('k') => { self.nav_up(); }
            KeyCode::Down | KeyCode::Char('j') => { self.nav_down(); }
            KeyCode::Char('z') => { self.toggle_collapse(); }
            KeyCode::Char('i') => { self.focused_panel = Panel::Input; } // Enter input mode
            _ => {}
        }
    }

    fn focus_next(&mut self) {
        self.focused_panel = match self.focused_panel {
            Panel::Tree => Panel::Kanban,
            Panel::Kanban => if self.focus_mode { Panel::Detail } else { Panel::Input },
            Panel::Detail => Panel::Input,
            Panel::Input => Panel::Tree,
        };
    }

    fn focus_prev(&mut self) {
        self.focused_panel = match self.focused_panel {
            Panel::Input => if self.focus_mode { Panel::Detail } else { Panel::Kanban },
            Panel::Detail => Panel::Kanban,
            Panel::Kanban => Panel::Tree,
            Panel::Tree => Panel::Input,
        };
    }

    fn toggle_focus(&mut self) {
        self.focus_mode = !self.focus_mode;
        if self.focus_mode {
            self.focused_panel = Panel::Detail;
        } else {
            self.focused_panel = Panel::Kanban;
        }
    }

    fn on_enter(&mut self) {
        match self.focused_panel {
            Panel::Tree => {
                if let Some(item) = self.tree_items.get(self.tree_index) {
                    if let NodeId::Session(sid) = item {
                        self.switch_session(sid.clone());
                        self.focus_mode = true;
                        self.focused_panel = Panel::Detail;
                    } else {
                        // Project — toggle collapse
                        self.toggle_collapse();
                    }
                }
            }
            Panel::Kanban => {
                if let Some(idx) = self.kanban_selected {
                    if let Some(w) = self.workers.get(idx) {
                        if let Some(sid) = w.get("session_id").and_then(|v| v.as_str()) {
                            self.switch_session(sid.to_string());
                            self.focus_mode = true;
                            self.focused_panel = Panel::Detail;
                        }
                    }
                }
            }
            Panel::Input => {
                // Send message
                if let Some(sid) = &self.active_session {
                    let text = self.input_text.clone();
                    if !text.is_empty() {
                        self.pending_sends.push((sid.clone(), text));
                        self.input_text.clear();
                        self.input_cursor = 0;
                    }
                }
            }
            _ => {}
        }
    }

    fn switch_session(&mut self, sid: String) {
        // Save current draft
        if let Some(old) = &self.active_session {
            self.drafts.insert(old.clone(), self.input_text.clone());
        }
        self.active_session = Some(sid.clone());
        // Load new draft
        self.input_text = self.drafts.get(&sid).cloned().unwrap_or_default();
        self.input_cursor = self.input_text.len();
    }

    fn nav_up(&mut self) {
        match self.focused_panel {
            Panel::Tree => {
                if self.tree_index > 0 { self.tree_index -= 1; }
            }
            Panel::Kanban => {
                let new_idx = self.kanban_selected.unwrap_or(0).saturating_sub(1);
                self.kanban_selected = Some(new_idx);
            }
            _ => {}
        }
    }

    fn nav_down(&mut self) {
        match self.focused_panel {
            Panel::Tree => {
                if self.tree_index + 1 < self.tree_items.len() { self.tree_index += 1; }
            }
            Panel::Kanban => {
                let max = self.workers.len().saturating_sub(1);
                let cur = self.kanban_selected.unwrap_or(0);
                if cur < max { self.kanban_selected = Some(cur + 1); }
            }
            _ => {}
        }
    }

    fn toggle_collapse(&mut self) {
        if let Some(item) = self.tree_items.get(self.tree_index) {
            if let NodeId::Project(name) = item {
                if !self.collapsed.remove(name.as_str()) {
                    self.collapsed.insert(name.clone());
                }
                self.rebuild_tree();
            }
        }
    }

    fn handle_input_key(&mut self, key: crossterm::event::KeyEvent) {
        match key.code {
            KeyCode::Char(c) => {
                self.input_text.insert(self.input_cursor, c);
                self.input_cursor += 1;
            }
            KeyCode::Backspace => {
                if self.input_cursor > 0 {
                    self.input_cursor -= 1;
                    self.input_text.remove(self.input_cursor);
                }
            }
            KeyCode::Delete => {
                if self.input_cursor < self.input_text.len() {
                    self.input_text.remove(self.input_cursor);
                }
            }
            KeyCode::Left => { self.input_cursor = self.input_cursor.saturating_sub(1); }
            KeyCode::Right => { self.input_cursor = self.input_cursor.min(self.input_text.len()); }
            KeyCode::Enter => {
                // Send (move focus to Input then Enter triggers send via on_enter)
                if let Some(sid) = &self.active_session {
                    let text = self.input_text.clone();
                    if !text.is_empty() {
                        self.pending_sends.push((sid.clone(), text));
                        self.input_text.clear();
                        self.input_cursor = 0;
                    }
                }
            }
            KeyCode::Tab | KeyCode::Esc => {
                self.focused_panel = Panel::Kanban;
            }
            _ => {}
        }
    }
}
