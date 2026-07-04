use std::collections::{HashMap, HashSet, VecDeque};
use crossterm::event::{self as ce, Event, KeyCode, KeyEventKind};
use ratatui::{
    Terminal, backend::{CrosstermBackend, Backend},
};
use serde_json::Value;
use tokio::sync::mpsc;
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

/// 创建 Worker 的模态表单字段
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateField { Path, Agent }

/// 按 n 键弹出的创建 Worker 模态
pub struct CreateModal {
    pub field: CreateField,
    pub path: String,
    pub agent: String,
    pub error: Option<String>,
}

impl CreateModal {
    pub fn new() -> Self {
        Self {
            field: CreateField::Path,
            path: std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            agent: "build".to_string(),
            error: None,
        }
    }

    pub fn current_text(&self) -> &str {
        match self.field {
            CreateField::Path => &self.path,
            CreateField::Agent => &self.agent,
        }
    }

    pub fn current_text_mut(&mut self) -> &mut String {
        match self.field {
            CreateField::Path => &mut self.path,
            CreateField::Agent => &mut self.agent,
        }
    }
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

    // ── 创建 Worker 模态 ──
    /// 弹出层：None = 不显示，Some = 显示表单
    pub create_modal: Option<CreateModal>,

    /// 键盘事件接收器（专用线程推送）
    kb_rx: mpsc::UnboundedReceiver<ce::Event>,

    // Send queue (buffered sends from keyboard handler)
    pending_sends: Vec<(String, String)>,
    // Create queue (buffered create_session requests from modal)
    pending_creates: Vec<(String, String)>,

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
            create_modal: None,
            kb_rx: mpsc::unbounded_channel().1, // placeholder, replaced in run()
            pending_sends: vec![],
            pending_creates: vec![],
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

        // ── 专用键盘线程（解决 spawn_blocking 泄漏问题）──
        let (kb_tx, kb_rx) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            loop {
                match ce::read() {
                    Ok(event) => {
                        if kb_tx.send(event).is_err() {
                            break; // receiver dropped
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        self.kb_rx = kb_rx;

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
                Some(event) = self.kb_rx.recv() => {
                    match event {
                        ce::Event::Key(key) if key.kind == KeyEventKind::Press => {
                            self.on_key(key);
                        }
                        ce::Event::Resize(w, h) => {
                            self.term_width = w;
                            self.term_height = h;
                            self.layout_tier = LayoutTier::from_width(w);
                        }
                        _ => {}
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
        // 处理排队的聊天发送
        while let Some((sid, text)) = self.pending_sends.pop() {
            self.drafts.insert(sid.clone(), text.clone());
            let _ = self.conn.send_prompt(&sid, &text).await;
        }
        // 处理排队的创建请求
        while let Some((path, agent)) = self.pending_creates.pop() {
            eprintln!("[tui] creating session: path={path} agent={agent}");
            match self.conn.create_session(&path, &agent).await {
                Ok(v) => eprintln!("[tui] create_session response: {v}"),
                Err(e) => eprintln!("[tui] create_session error: {e}"),
            }
        }
        self.anim_frame = (self.anim_frame + 1) % 8;
        match self.conn.poll_overview().await {
            Ok(data) => {
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
            }
            Err(e) => {
                // Log the error so we can debug connection issues
                self.connected = false;
                // Log error (visible when running in tmux)
                eprintln!("[tui] poll_overview error: {e}");
            }
        }
    }

    fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        // 模态打开时拦截所有键盘事件
        if self.create_modal.is_some() {
            self.handle_create_modal_key(key);
            return;
        }
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
            KeyCode::Char('n') => { self.create_modal = Some(CreateModal::new()); }
            _ => {}
        }
    }

    /// 处理创建模态的键盘事件
    fn handle_create_modal_key(&mut self, key: crossterm::event::KeyEvent) {
        let modal = self.create_modal.as_mut().unwrap();
        match key.code {
            KeyCode::Esc => {
                // 关闭模态
                self.create_modal = None;
            }
            KeyCode::Tab => {
                // 切换字段
                modal.field = match modal.field {
                    CreateField::Path => CreateField::Agent,
                    CreateField::Agent => CreateField::Path,
                };
            }
            KeyCode::Enter => {
                // 提交创建请求（异步，队列化）
                let path = modal.path.clone();
                let agent = modal.agent.clone();
                if path.is_empty() {
                    modal.error = Some("项目路径不能为空".into());
                    return;
                }
                // 用 create_session RPC（Manager 会自动 spawn worker）
                // 入队，等 tick 异步处理
                self.pending_creates.push((path, agent));
                self.create_modal = None;
            }
            KeyCode::Backspace => {
                modal.current_text_mut().pop();
                modal.error = None;
            }
            KeyCode::Char(c) => {
                modal.current_text_mut().push(c);
                modal.error = None;
            }
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
