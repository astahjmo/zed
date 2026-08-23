use agent_client_protocol::schema::v1 as acp;
use agent_ui::AgentPanel;
use anyhow::Result;
use gpui::{
    App, AsyncWindowContext, Context, ElementId, Entity, EventEmitter, Focusable, FocusHandle,
    InteractiveElement as _, IntoElement, ParentElement, Pixels, Render, ScrollHandle, Styled,
    Task, WeakEntity, Window, actions, div, px,
};
use serde_json::Value;
use std::{
    io::{BufRead as _, BufReader},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use ui::{
    Color, Icon, IconButton, IconButtonShape, IconName, IconSize, Label, LabelSize, Tooltip,
    h_flex, prelude::*, v_flex,
};
use util::{ResultExt as _, paths::home_dir};
use workspace::{
    MultiWorkspace, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

const PANEL_KEY: &str = "ClaudeHistoryPanel";

actions!(claude_code_ide, [ToggleHistoryPanel]);

pub fn init_history_panel(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleHistoryPanel, window, cx| {
            workspace.toggle_panel_focus::<ClaudeHistoryPanel>(window, cx);
        });
    })
    .detach();
}

pub struct ClaudeHistoryPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    sessions: Vec<SessionEntry>,
    available_projects: Vec<ProjectInfo>,
    current_project_dir: Option<String>,
    workspace_project_dir: Option<String>,
    is_loading: bool,
    is_loading_projects: bool,
    is_picking_project: bool,
    scroll_handle: ScrollHandle,
    _load_task: Option<Task<()>>,
    _projects_task: Option<Task<()>>,
}

#[derive(Clone)]
struct SessionEntry {
    session_id: String,
    title: String,
    modified: SystemTime,
}

struct ProjectInfo {
    dir_name: String,
    cwd: Option<PathBuf>,
}

impl ProjectInfo {
    fn short_name(&self) -> &str {
        self.cwd
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or(&self.dir_name)
    }

    fn display_path(&self) -> String {
        if let Some(cwd) = &self.cwd {
            let home = home_dir();
            cwd.strip_prefix(&home)
                .map(|rel| format!("~/{}", rel.display()))
                .unwrap_or_else(|_| cwd.display().to_string())
        } else {
            self.dir_name.clone()
        }
    }
}

impl ClaudeHistoryPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            Self::new(workspace, window, cx)
        })
    }

    fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_handle = cx.entity().downgrade();

        let workspace_root = workspace
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|wt| wt.read(cx).abs_path().to_path_buf());

        let workspace_project_dir = workspace_root
            .as_ref()
            .map(|root| sanitize_path(&root.to_string_lossy()));

        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let mut panel = ClaudeHistoryPanel {
                workspace: workspace_handle,
                focus_handle,
                sessions: Vec::new(),
                available_projects: Vec::new(),
                current_project_dir: workspace_project_dir.clone(),
                workspace_project_dir,
                is_loading: false,
                is_loading_projects: false,
                is_picking_project: false,
                scroll_handle: ScrollHandle::new(),
                _load_task: None,
                _projects_task: None,
            };
            if let Some(dir) = panel.current_project_dir.clone() {
                panel.refresh(dir, cx);
            }
            panel.load_projects(cx);
            panel
        })
    }

    fn refresh(&mut self, project_dir: String, cx: &mut Context<Self>) {
        self.is_loading = true;
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let sessions = cx
                .background_executor()
                .spawn(async move { load_sessions(&project_dir) })
                .await;
            this.update(cx, |this, cx| {
                this.sessions = sessions;
                this.is_loading = false;
                cx.notify();
            })
            .log_err();
        }));
    }

    fn load_projects(&mut self, cx: &mut Context<Self>) {
        self.is_loading_projects = true;
        self._projects_task = Some(cx.spawn(async move |this, cx| {
            let projects = cx
                .background_executor()
                .spawn(async move { load_project_list() })
                .await;
            this.update(cx, |this, cx| {
                this.available_projects = projects;
                this.is_loading_projects = false;
                cx.notify();
            })
            .log_err();
        }));
    }

    fn select_project(&mut self, dir_name: String, cx: &mut Context<Self>) {
        self.current_project_dir = Some(dir_name.clone());
        self.is_picking_project = false;
        self.sessions = Vec::new();
        self.refresh(dir_name, cx);
    }

    fn open_in_agent_panel(
        &mut self,
        session: SessionEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session_id = acp::SessionId::new(session.session_id);
        let title = SharedString::from(session.title);
        let workspace = self.workspace.clone();
        // Capture the window handle so we can use it in the deferred closure.
        let window_handle = window.window_handle();

        // Defer everything: focus_panel calls activate_panel which calls set_active on
        // every dock panel including this one, which panics if ClaudeHistoryPanel is
        // already being updated inside the click handler.
        cx.defer(move |cx| {
            let Some(multi_workspace_window) = window_handle.downcast::<MultiWorkspace>() else {
                log::warn!("claude_history: window root is not MultiWorkspace");
                return;
            };
            multi_workspace_window
                .update(cx, |multi_workspace, window, cx| {
                    let workspace = multi_workspace.workspace().clone();
                    workspace.update(cx, |workspace, cx| {
                        workspace.reveal_panel::<AgentPanel>(window, cx);
                        if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                            log::info!(
                                "claude_history: opening session {:?} in AgentPanel",
                                session_id
                            );
                            panel.update(cx, |panel, cx| {
                                panel.open_thread(session_id, None, Some(title), window, cx);
                            });
                        } else {
                            log::warn!("claude_history: AgentPanel not found in workspace");
                        }
                        workspace.focus_panel::<AgentPanel>(window, cx);
                    });
                })
                .log_err();
        });
    }

    fn current_project_short_name(&self) -> String {
        let Some(dir) = &self.current_project_dir else {
            return "No project".to_owned();
        };
        self.available_projects
            .iter()
            .find(|p| &p.dir_name == dir)
            .map(|p| p.short_name().to_owned())
            .unwrap_or_else(|| dir.clone())
    }
}

fn claude_config_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".claude"))
}

fn sanitize_path(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect()
}

fn load_project_list() -> Vec<ProjectInfo> {
    let projects_dir = claude_config_dir().join("projects");
    let Ok(entries) = std::fs::read_dir(&projects_dir) else {
        return Vec::new();
    };

    let mut projects: Vec<ProjectInfo> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }
            let dir_name = entry.file_name().to_str()?.to_owned();
            let cwd = read_project_cwd(&entry.path());
            Some(ProjectInfo { dir_name, cwd })
        })
        .collect();

    projects.sort_by(|a, b| a.short_name().cmp(b.short_name()));
    projects
}

fn read_project_cwd(project_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(project_dir).ok()?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if file_name.starts_with("agent-") {
            continue;
        }
        if let Some(cwd) = extract_cwd_from_file(&path) {
            return Some(cwd);
        }
    }
    None
}

fn extract_cwd_from_file(path: &Path) -> Option<PathBuf> {
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(cwd) = obj.get("cwd").and_then(Value::as_str) {
            return Some(PathBuf::from(cwd));
        }
    }
    None
}

fn load_sessions(project_dir: &str) -> Vec<SessionEntry> {
    let sessions_dir = claude_config_dir().join("projects").join(project_dir);

    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return Vec::new();
    };

    let mut sessions: Vec<SessionEntry> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()?.to_str() != Some("jsonl") {
                return None;
            }
            let file_name = path.file_name()?.to_str()?;
            if file_name.starts_with("agent-") {
                return None;
            }
            parse_session_file(&path)
        })
        .collect();

    sessions.sort_by_key(|session| std::cmp::Reverse(session.modified));
    sessions
}

fn parse_session_file(path: &Path) -> Option<SessionEntry> {
    let file = std::fs::File::open(path).ok()?;
    let modified = file.metadata().ok()?.modified().ok()?;
    let reader = BufReader::new(file);

    let mut session_id: Option<String> = None;
    let mut custom_title: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut first_user_text: Option<String> = None;

    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        if session_id.is_none() {
            session_id = obj
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }

        match obj.get("type").and_then(Value::as_str) {
            Some("custom-title") => {
                custom_title = obj
                    .get("customTitle")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                break;
            }
            Some("ai-title") => {
                if ai_title.is_none() {
                    ai_title = obj
                        .get("aiTitle")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
            Some("summary") => {
                if ai_title.is_none() {
                    ai_title = obj
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
            Some("user") if first_user_text.is_none() => {
                let content = obj.get("message").and_then(|m| m.get("content"));
                first_user_text = content
                    .and_then(Value::as_str)
                    .and_then(|s| s.lines().next())
                    .map(str::to_owned)
                    .or_else(|| {
                        content
                            .and_then(Value::as_array)
                            .and_then(|blocks| blocks.first())
                            .and_then(|b| b.get("text"))
                            .and_then(Value::as_str)
                            .and_then(|s| s.lines().next())
                            .map(str::to_owned)
                    });
            }
            _ => {}
        }
    }

    let session_id =
        session_id.or_else(|| path.file_stem()?.to_str().map(str::to_owned))?;

    let title = custom_title
        .or(ai_title)
        .or(first_user_text)
        .unwrap_or_else(|| "Untitled Session".to_owned());

    Some(SessionEntry {
        session_id,
        title,
        modified,
    })
}

fn format_age(modified: SystemTime) -> String {
    let elapsed = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);

    let secs = elapsed.as_secs();
    if secs < 3600 {
        format!("{}m", (secs / 60).max(1))
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else if secs < 86400 * 30 {
        format!("{}d", secs / 86400)
    } else {
        format!("{}mo", secs / (86400 * 30))
    }
}

impl Render for ClaudeHistoryPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sessions = self.sessions.clone();
        let is_loading = self.is_loading;
        let is_loading_projects = self.is_loading_projects;
        let is_picking = self.is_picking_project;
        let current_name = self.current_project_short_name();
        let current_project_dir = self.current_project_dir.clone();
        let workspace_project_dir = self.workspace_project_dir.clone();

        let projects: Vec<(usize, String, String, String, bool)> = self
            .available_projects
            .iter()
            .enumerate()
            .map(|(ix, project)| {
                let is_selected =
                    current_project_dir.as_deref() == Some(project.dir_name.as_str());
                (
                    ix,
                    project.dir_name.clone(),
                    project.short_name().to_owned(),
                    project.display_path(),
                    is_selected,
                )
            })
            .collect();

        let header_left = if is_picking {
            h_flex()
                .gap_1()
                .child(
                    IconButton::new("claude-history-back", IconName::ChevronLeft)
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Back to sessions"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.is_picking_project = false;
                            cx.notify();
                        })),
                )
                .child(
                    Label::new("Select Project")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else {
            div()
                .id("claude-project-toggle")
                .flex()
                .items_center()
                .gap_1()
                .px_1()
                .rounded_md()
                .hover(|s| s.bg(cx.theme().colors().element_hover))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.is_picking_project = true;
                    cx.notify();
                }))
                .child(Label::new(current_name).size(LabelSize::Small))
                .child(
                    Icon::new(IconName::ChevronDown)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element()
        };

        let refresh_action: Box<dyn Fn(&mut ClaudeHistoryPanel, &mut Context<ClaudeHistoryPanel>)> =
            if is_picking {
                Box::new(|this, cx| this.load_projects(cx))
            } else {
                Box::new(|this, cx| {
                    if let Some(dir) = this.current_project_dir.clone() {
                        this.refresh(dir, cx);
                    }
                })
            };

        v_flex()
            .id("claude-history-panel")
            .size_full()
            .overflow_hidden()
            .bg(cx.theme().colors().panel_background)
            .track_focus(&self.focus_handle)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(header_left)
                    .child(
                        IconButton::new("claude-history-refresh", IconName::HistoryRerun)
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                refresh_action(this, cx);
                            })),
                    ),
            )
            // Project picker
            .when(is_picking, |panel| {
                panel.child(
                    div()
                        .id("claude-project-scroll")
                        .flex_1()
                        .overflow_y_scroll()
                        .when(is_loading_projects, |this| {
                            this.child(
                                div().p_2().child(
                                    Label::new("Loading…")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                            )
                        })
                        .when(!is_loading_projects && projects.is_empty(), |this| {
                            this.child(
                                div().p_2().child(
                                    Label::new("No projects found")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                            )
                        })
                        .when(!is_loading_projects && !projects.is_empty(), |this| {
                            this.children(projects.into_iter().map(
                                |(ix, dir_name, short_name, display_path, is_selected)| {
                                    let is_workspace = workspace_project_dir
                                        .as_deref()
                                        == Some(dir_name.as_str());
                                    h_flex()
                                        .id(ElementId::from(("proj", ix)))
                                        .w_full()
                                        .px_2()
                                        .py_1()
                                        .gap_2()
                                        .hover(|s| s.bg(cx.theme().colors().element_hover))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.select_project(dir_name.clone(), cx);
                                        }))
                                        .child(
                                            v_flex()
                                                .min_w_0()
                                                .flex_1()
                                                .child(
                                                    h_flex()
                                                        .gap_1()
                                                        .child(
                                                            Label::new(short_name)
                                                                .size(LabelSize::Small)
                                                                .truncate(),
                                                        )
                                                        .when(is_workspace, |row| {
                                                            row.child(
                                                                Label::new("current")
                                                                    .size(LabelSize::XSmall)
                                                                    .color(Color::Muted),
                                                            )
                                                        }),
                                                )
                                                .child(
                                                    Label::new(display_path)
                                                        .size(LabelSize::XSmall)
                                                        .color(Color::Muted)
                                                        .truncate(),
                                                ),
                                        )
                                        .when(is_selected, |row| {
                                            row.child(
                                                Icon::new(IconName::Check)
                                                    .size(IconSize::XSmall)
                                                    .color(Color::Accent),
                                            )
                                        })
                                },
                            ))
                        }),
                )
            })
            // Session list
            .when(!is_picking, |panel| {
                panel
                    .when(is_loading, |this| {
                        this.child(
                            div().p_2().child(
                                Label::new("Loading…")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                    })
                    .when(!is_loading && sessions.is_empty(), |this| {
                        this.child(
                            div().p_2().child(
                                Label::new("No sessions found for this project")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                    })
                    .when(!is_loading && !sessions.is_empty(), |this| {
                        this.child(
                            div()
                                .id("claude-history-scroll")
                                .flex_1()
                                .overflow_y_scroll()
                                .track_scroll(&self.scroll_handle)
                                .children(sessions.into_iter().enumerate().map(
                                    |(ix, session)| {
                                        let age = format_age(session.modified);
                                        let session_for_click = session.clone();
                                        h_flex()
                                            .id(ElementId::from(ix))
                                            .w_full()
                                            .px_2()
                                            .py_1()
                                            .justify_between()
                                            .gap_2()
                                            .hover(|s| {
                                                s.bg(cx.theme().colors().element_hover)
                                            })
                                            .tooltip(Tooltip::text("Open session in agent panel"))
                                            .on_click(cx.listener(
                                                move |this, _, window, cx| {
                                                    this.open_in_agent_panel(
                                                        session_for_click.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                },
                                            ))
                                            .child(
                                                v_flex()
                                                    .min_w_0()
                                                    .flex_1()
                                                    .child(
                                                        Label::new(session.title)
                                                            .size(LabelSize::Small)
                                                            .truncate(),
                                                    )
                                                    .child(
                                                        Label::new(age)
                                                            .size(LabelSize::XSmall)
                                                            .color(Color::Muted),
                                                    ),
                                            )
                                            .child(
                                                Icon::new(IconName::Chat)
                                                    .size(IconSize::Small)
                                                    .color(Color::Muted),
                                            )
                                    },
                                )),
                        )
                    })
            })
    }
}

impl Focusable for ClaudeHistoryPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ClaudeHistoryPanel {}

impl Panel for ClaudeHistoryPanel {
    fn persistent_name() -> &'static str {
        "ClaudeHistoryPanel"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(280.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::HistoryRerun)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Claude History")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleHistoryPanel)
    }

    fn activation_priority(&self) -> u32 {
        7
    }
}
