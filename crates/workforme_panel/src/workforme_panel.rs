use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json;

use anyhow::Context as _;
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels,
    Point, Render, StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window,
    actions, anchored, deferred, div, px,
};
use task::{HideStrategy, RevealStrategy, SaveStrategy, SpawnInTerminal, TaskId};
use ui::{
    ActiveTheme, ButtonCommon, Clickable, ContextMenu, FluentBuilder, IconButton, IconName,
    IconSize, Label, LabelCommon, Tooltip, h_flex, v_flex,
};
use util::shell::Shell;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    item::{Item, ItemEvent},
};
use zed_actions::RevealTarget;

const WORKFORME_PANEL_KEY: &str = "WorkformePanel";
const POLL_INTERVAL: Duration = Duration::from_secs(3);

actions!(workforme_panel, [ToggleFocus]);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Stage {
    Plan,
    Implement,
    Check,
    CodexReview,
    Fix,
    Pr,
    Fixer,
}

impl Stage {
    fn all() -> &'static [Stage] {
        &[
            Stage::Plan,
            Stage::Implement,
            Stage::Check,
            Stage::CodexReview,
            Stage::Fix,
            Stage::Pr,
            Stage::Fixer,
        ]
    }

    fn label(&self) -> &'static str {
        match self {
            Stage::Plan => "Plan",
            Stage::Implement => "Impl",
            Stage::Check => "Check",
            Stage::CodexReview => "Codex",
            Stage::Pr => "PR",
            Stage::Fix => "Fix",
            Stage::Fixer => "Fixer",
        }
    }

    fn command(&self) -> &'static str {
        match self {
            Stage::Plan => "plan",
            Stage::Implement => "impl",
            Stage::Check => "check",
            Stage::CodexReview => "codex-review",
            Stage::Pr => "pr",
            Stage::Fix => "fix",
            Stage::Fixer => "fixer",
        }
    }

    fn state_key(&self) -> &'static str {
        match self {
            Stage::Plan => "plan",
            Stage::Implement => "implement",
            Stage::Check => "check",
            Stage::CodexReview => "codex-review",
            Stage::Fix => "fix",
            Stage::Pr => "pr",
            Stage::Fixer => "fixer",
        }
    }

    fn output_filename(&self) -> &'static str {
        match self {
            Stage::Plan => "plan.md",
            Stage::Implement => "implement.md",
            Stage::Check => "check.md",
            Stage::CodexReview => "codex-review.md",
            Stage::Pr => "pr.md",
            Stage::Fix => "fix.md",
            Stage::Fixer => "fixer.md",
        }
    }

    fn is_optional(&self) -> bool {
        matches!(self, Stage::CodexReview | Stage::Fix | Stage::Fixer)
    }

    fn prerequisite(&self) -> Option<&Stage> {
        match self {
            Stage::Plan => None,
            Stage::Implement => Some(&Stage::Plan),
            Stage::Check => Some(&Stage::Implement),
            Stage::CodexReview => Some(&Stage::Implement),
            Stage::Fix => Some(&Stage::Check),
            Stage::Pr => Some(&Stage::Check),
            Stage::Fixer => Some(&Stage::Pr),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum StageStatus {
    NotStarted,
    InProgress(Option<String>),
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
struct DynStage {
    key: String,
    label: String,
    status: StageStatus,
    session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkformeTask {
    id: String,
    date: String,
    repo: Option<String>,
    title: String,
    branch: Option<String>,
    task_type: Option<String>,
    url: Option<String>,
    pr_url: Option<String>,
    worktree: Option<String>,
    stages: HashMap<Stage, StageStatus>,
    sessions: HashMap<Stage, String>,
    dynamic_stages: Vec<DynStage>,
}

impl WorkformeTask {
    fn uses_dynamic_stages(&self) -> bool {
        matches!(self.task_type.as_deref(), Some("review" | "analyst"))
    }

    fn current_stage(&self) -> &Stage {
        for stage in Stage::all() {
            match self.stages.get(stage) {
                Some(StageStatus::Done) => continue,
                _ => return stage,
            }
        }
        &Stage::Pr
    }

    fn latest_session_id(&self) -> Option<&str> {
        if self.uses_dynamic_stages() {
            return self
                .dynamic_stages
                .iter()
                .rev()
                .find_map(|s| s.session_id.as_deref());
        }
        for stage in Stage::all().iter().rev() {
            if let Some(id) = self.sessions.get(stage) {
                return Some(id.as_str());
            }
        }
        None
    }
}

pub struct WorkformePanel {
    focus_handle: FocusHandle,
    tasks: Vec<WorkformeTask>,
    collapsed_dates: std::collections::HashSet<String>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    workforme_root: PathBuf,
    workspace: WeakEntity<Workspace>,
    _poll_task: Task<()>,
}

impl EventEmitter<PanelEvent> for WorkformePanel {}

impl WorkformePanel {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        let workforme_root = home
            .join("go")
            .join("src")
            .join("github.com")
            .join("ajainc")
            .join("workforme");

        let poll_task = cx.spawn({
            async move |this: WeakEntity<Self>, cx| {
                loop {
                    cx.background_executor().timer(POLL_INTERVAL).await;
                    let work_dir = {
                        let Ok(result) =
                            this.update(cx, |panel, _cx| panel.workforme_root.join(".work"))
                        else {
                            break;
                        };
                        result
                    };
                    let tasks = cx
                        .background_executor()
                        .spawn(async move { load_tasks_from_disk(&work_dir) })
                        .await;
                    let result = this.update(cx, |panel, cx| {
                        if panel.tasks != tasks {
                            panel.tasks = tasks;
                            cx.notify();
                        }
                    });
                    if result.is_err() {
                        break;
                    }
                }
            }
        });

        let mut panel = Self {
            focus_handle: cx.focus_handle(),
            tasks: Vec::new(),
            collapsed_dates: std::collections::HashSet::new(),
            context_menu: None,
            workforme_root,
            workspace,
            _poll_task: poll_task,
        };
        panel.load_tasks();
        panel
    }

    fn load_tasks(&mut self) {
        let work_dir = self.workforme_root.join(".work");
        self.tasks = load_tasks_from_disk(&work_dir);
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load_tasks();
        cx.notify();
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        session_id: Option<String>,
        repo_path: Option<PathBuf>,
        task_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        let workforme_root = self.workforme_root.clone();
        let entity = cx.entity().downgrade();

        let context_menu = ContextMenu::build(window, cx, |menu, _, _cx| {
            let mut menu = menu;

            if let Some(sid) = session_id {
                let ws = workspace.clone();
                let sid_clone = sid.clone();
                let repo_for_term = repo_path.clone();
                menu = menu.custom_entry(
                    move |_window, _cx| Label::new("Open in Terminal").into_any_element(),
                    {
                        let sid = sid_clone;
                        let ws = ws.clone();
                        let cwd = repo_for_term.clone();
                        move |window, cx| {
                            let short_id = sid[..8.min(sid.len())].to_string();
                            let script = format!(
                                "claude attach {} 2>/dev/null || claude --resume {}",
                                short_id, sid
                            );
                            let spawn = SpawnInTerminal {
                                id: TaskId("workforme-resume".to_string()),
                                full_label: "workforme: session".to_string(),
                                label: "workforme: session".to_string(),
                                command: Some("bash".to_string()),
                                args: vec!["-c".to_string(), script],
                                command_label: "session".to_string(),
                                cwd: cwd.clone(),
                                env: HashMap::default(),
                                use_new_terminal: true,
                                allow_concurrent_runs: true,
                                reveal: RevealStrategy::Always,
                                reveal_target: RevealTarget::Dock,
                                hide: HideStrategy::Never,
                                shell: Shell::System,
                                show_summary: false,
                                show_command: false,
                                show_rerun: false,
                                save: SaveStrategy::default(),
                            };
                            if let Some(ws) = ws.upgrade() {
                                ws.update(cx, |ws, cx| {
                                    ws.spawn_in_terminal(spawn, window, cx).detach();
                                });
                            }
                        }
                    },
                );
            }

            if let Some(path) = repo_path {
                menu = menu.custom_entry(
                    move |_window, _cx| Label::new("Open in New Window").into_any_element(),
                    {
                        let path = path.clone();
                        move |_window, _cx| {
                            std::process::Command::new("/Applications/Zed.app/Contents/MacOS/cli")
                                .arg("-n")
                                .arg(&path)
                                .spawn()
                                .ok();
                        }
                    },
                );
            }

            // Delete task
            let task_id_for_delete = task_id.clone();
            let root_for_delete = workforme_root.clone();
            let entity_for_delete = entity.clone();
            menu = menu.separator().custom_entry(
                move |_window, _cx| {
                    Label::new("Delete Task")
                        .color(ui::Color::Error)
                        .into_any_element()
                },
                move |_window, cx| {
                    let task_dir = root_for_delete.join(".work").join(&task_id_for_delete);
                    let script = root_for_delete.join("scripts/delete-task.sh");
                    std::process::Command::new("bash")
                        .arg(&script)
                        .arg(&task_dir)
                        .spawn()
                        .ok();
                    if let Some(panel) = entity_for_delete.upgrade() {
                        panel.update(cx, |panel, cx| {
                            panel.load_tasks();
                            cx.notify();
                        });
                    }
                },
            );

            menu
        });

        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&context_menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }
}

impl Focusable for WorkformePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkformePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = self.workspace.clone();
        let workspace_for_header = self.workspace.clone();
        let workforme_root = self.workforme_root.clone();

        // Group tasks by date (reversed so newest first)
        let mut dates: Vec<String> = self.tasks.iter().map(|t| t.date.clone()).collect();
        dates.sort();
        dates.dedup();
        dates.reverse();

        let collapsed = self.collapsed_dates.clone();
        let entity = cx.entity().downgrade();

        let mut date_sections: Vec<gpui::AnyElement> = Vec::new();
        let mut global_index = 0usize;

        for date in &dates {
            let is_collapsed = collapsed.contains(date);
            let date_clone = date.clone();
            let entity_clone = entity.clone();

            // Date header
            let header = h_flex()
                .id(gpui::ElementId::Name(format!("wfm-date-{}", date).into()))
                .px_2()
                .py_0p5()
                .cursor_pointer()
                .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                .child(
                    Label::new(if is_collapsed { "▶" } else { "▼" })
                        .size(ui::LabelSize::XSmall)
                        .color(ui::Color::Muted),
                )
                .child(
                    Label::new(format!(" {}", date))
                        .size(ui::LabelSize::Small)
                        .color(ui::Color::Muted),
                )
                .on_click(move |_event, _window, cx| {
                    if let Some(panel) = entity_clone.upgrade() {
                        panel.update(cx, |panel, cx| {
                            if panel.collapsed_dates.contains(&date_clone) {
                                panel.collapsed_dates.remove(&date_clone);
                            } else {
                                panel.collapsed_dates.insert(date_clone.clone());
                            }
                            cx.notify();
                        });
                    }
                });

            date_sections.push(header.into_any_element());

            // Task rows (if not collapsed)
            if !is_collapsed {
                for task in self.tasks.iter().filter(|t| &t.date == date) {
                    let row = render_task_row(
                        task,
                        global_index,
                        &workspace,
                        &entity,
                        &workforme_root,
                        &*cx,
                    );
                    date_sections.push(row.into_any_element());
                    global_index += 1;
                }
            } else {
                global_index += self.tasks.iter().filter(|t| &t.date == date).count();
            }
        }

        div()
            .id("workforme-panel")
            .flex()
            .flex_col()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .justify_between()
                    .child(
                        Label::new("Workforme")
                            .size(ui::LabelSize::Small)
                            .color(ui::Color::Muted),
                    )
                    .child(
                        IconButton::new("wfm-add-task", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New Task"))
                            .on_click(move |_event, window, cx| {
                                let spawn = SpawnInTerminal {
                                    id: TaskId("workforme-new".to_string()),
                                    full_label: "workforme".to_string(),
                                    label: "workforme".to_string(),
                                    command: Some("claude".to_string()),
                                    args: vec![],
                                    command_label: "claude".to_string(),
                                    cwd: Some(workforme_root.clone()),
                                    env: HashMap::default(),
                                    use_new_terminal: true,
                                    allow_concurrent_runs: true,
                                    reveal: RevealStrategy::Always,
                                    reveal_target: RevealTarget::Dock,
                                    hide: HideStrategy::Never,
                                    shell: Shell::System,
                                    show_summary: false,
                                    show_command: false,
                                    show_rerun: false,
                                    save: SaveStrategy::default(),
                                };
                                if let Some(ws) = workspace_for_header.upgrade() {
                                    ws.update(cx, |ws, cx| {
                                        ws.spawn_in_terminal(spawn, window, cx).detach();
                                    });
                                }
                            }),
                    ),
            )
            .child(
                div()
                    .id("wfm-task-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .when(date_sections.is_empty(), |el| {
                        el.child(
                            v_flex()
                                .p_4()
                                .gap_2()
                                .items_center()
                                .child(
                                    Label::new("No tasks yet")
                                        .size(ui::LabelSize::Small)
                                        .color(ui::Color::Muted),
                                )
                                .child(
                                    Label::new("Click + to start a new task")
                                        .size(ui::LabelSize::XSmall)
                                        .color(ui::Color::Muted),
                                ),
                        )
                    })
                    .children(date_sections),
            )
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(3)
            }))
    }
}

fn load_tasks_from_disk(work_dir: &Path) -> Vec<WorkformeTask> {
    let Ok(entries) = std::fs::read_dir(work_dir) else {
        return Vec::new();
    };

    let mut tasks = Vec::new();
    for entry in entries.flatten() {
        let task_dir = entry.path();
        if !task_dir.is_dir() {
            continue;
        }
        if let Some(task) = parse_task_dir(&task_dir) {
            tasks.push(task);
        }
    }

    tasks.sort_by(|a, b| b.date.cmp(&a.date));
    tasks
}

fn parse_task_dir(task_dir: &Path) -> Option<WorkformeTask> {
    let id = task_dir.file_name()?.to_str()?.to_string();

    let state_content = std::fs::read_to_string(task_dir.join("state.json")).ok()?;
    let state_value = serde_json::from_str::<serde_json::Value>(&state_content).ok()?;

    let repo = state_value
        .get("repo")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let title = state_value
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or(&id)
        .to_string();
    let branch = state_value
        .get("branch")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let task_type = state_value
        .get("type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let url = state_value
        .get("url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let pr_url = state_value
        .get("pr_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let date = state_value
        .get("created")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let worktree = state_value
        .get("worktree")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let mut stages = HashMap::new();
    let mut sessions = HashMap::new();
    let mut dynamic_stages = Vec::new();

    let uses_dynamic = matches!(task_type.as_deref(), Some("review" | "analyst"));

    if let Some(stages_obj) = state_value.get("stages").and_then(|v| v.as_object()) {
        if uses_dynamic {
            for (key, val) in stages_obj {
                if let Some(stage_val) = val.as_object() {
                    let status_str = stage_val
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("not_started");
                    let session_id = stage_val
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());
                    let status = match status_str {
                        "done" => StageStatus::Done,
                        "failed" => StageStatus::Failed,
                        "running" => StageStatus::InProgress(session_id.clone()),
                        _ => StageStatus::NotStarted,
                    };
                    let label = key[..1].to_uppercase() + &key[1..];
                    dynamic_stages.push(DynStage {
                        key: key.clone(),
                        label,
                        status,
                        session_id,
                    });
                }
            }
        } else {
            for stage in Stage::all() {
                let stage_key = stage.state_key();
                if let Some(stage_val) = stages_obj.get(stage_key).and_then(|v| v.as_object()) {
                    let status_str = stage_val
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("not_started");
                    let session_id = stage_val
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());

                    if let Some(ref sid) = session_id {
                        sessions.insert(stage.clone(), sid.clone());
                    }

                    let status = match status_str {
                        "done" => StageStatus::Done,
                        "failed" => StageStatus::Failed,
                        "running" => StageStatus::InProgress(session_id),
                        _ => StageStatus::NotStarted,
                    };
                    stages.insert(stage.clone(), status);
                } else {
                    stages.insert(stage.clone(), StageStatus::NotStarted);
                }
            }
        }
    }

    Some(WorkformeTask {
        id,
        date,
        repo,
        title,
        branch,
        task_type,
        url,
        pr_url,
        worktree,
        stages,
        sessions,
        dynamic_stages,
    })
}

fn render_task_row(
    task: &WorkformeTask,
    index: usize,
    workspace: &WeakEntity<Workspace>,
    panel: &WeakEntity<WorkformePanel>,
    workforme_root: &Path,
    cx: &App,
) -> impl IntoElement {
    let current_stage = task.current_stage();
    let session_id = task.latest_session_id().map(|s| s.to_string());
    let task_id = task.id.clone();
    let task_clone = task.clone();
    let workspace = workspace.clone();
    let panel = panel.clone();
    let workforme_root = workforme_root.to_path_buf();
    let cwd = task.worktree.as_ref().map(|w| PathBuf::from(w));
    let ctx_session_id = session_id.clone();
    let ctx_repo_path = cwd.clone();

    v_flex()
        .id(gpui::ElementId::NamedInteger(
            "workforme-task".into(),
            index as u64,
        ))
        .px_2()
        .py_1()
        .gap_0p5()
        .border_b_1()
        .border_color(cx.theme().colors().border)
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
        .on_mouse_down(MouseButton::Right, {
            let panel = panel.clone();
            let ctx_session_id = ctx_session_id.clone();
            let ctx_repo_path = ctx_repo_path.clone();
            let ctx_task_id = task_id.clone();
            move |event, window, cx| {
                if let Some(panel) = panel.upgrade() {
                    panel.update(cx, |panel, cx| {
                        panel.deploy_context_menu(
                            event.position,
                            ctx_session_id.clone(),
                            ctx_repo_path.clone(),
                            ctx_task_id.clone(),
                            window,
                            cx,
                        );
                    });
                }
            }
        })
        .on_click({
            let task_for_view = task_clone.clone();
            let task_id_for_click = task_id.clone();
            let workspace = workspace.clone();
            let workforme_root = workforme_root.clone();
            move |_event, window, cx| {
                if let Some(ws_entity) = workspace.upgrade() {
                    let task = task_for_view.clone();
                    let root = workforme_root.clone();
                    let weak_ws = workspace.clone();
                    let tid = task_id_for_click.clone();
                    ws_entity.update(cx, |workspace, cx| {
                        // 既存の view を探す（index で取得し borrow を解放）
                        let pane = workspace.active_pane();
                        let existing_idx = pane
                            .read(cx)
                            .items()
                            .position(|item| item.tab_content_text(0, cx).as_ref() == tid.as_str());
                        if let Some(idx) = existing_idx {
                            pane.update(cx, |pane, cx| {
                                pane.activate_item(idx, true, true, window, cx);
                            });
                        } else {
                            let view = cx.new(|cx| WorkformeTaskView::new(task, root, weak_ws, cx));
                            workspace.add_item_to_active_pane(
                                Box::new(view),
                                None,
                                true,
                                window,
                                cx,
                            );
                        }
                    });
                }
            }
        })
        .child(
            h_flex()
                .gap_1()
                .items_center()
                .child(Label::new(task.title.clone()).size(ui::LabelSize::Default))
                .when_some(task.task_type.clone(), |el, task_type| {
                    el.child(
                        div()
                            .px_1()
                            .rounded_sm()
                            .bg(cx.theme().colors().element_background)
                            .child(
                                Label::new(task_type)
                                    .size(ui::LabelSize::XSmall)
                                    .color(ui::Color::Muted),
                            ),
                    )
                }),
        )
        .child(
            h_flex()
                .gap_1()
                .items_center()
                .when_some(task.repo.clone(), |el, repo| {
                    el.child(
                        Label::new(repo)
                            .size(ui::LabelSize::XSmall)
                            .color(ui::Color::Muted),
                    )
                })
                .when(task.uses_dynamic_stages(), |el| {
                    el.child(render_dynamic_badges(&task.dynamic_stages, cx))
                })
                .when(!task.uses_dynamic_stages(), |el| {
                    el.child(render_stage_badges(task, current_stage, cx))
                }),
        )
}

fn render_stage_badges(task: &WorkformeTask, current_stage: &Stage, cx: &App) -> impl IntoElement {
    h_flex()
        .gap_0p5()
        .children(Stage::all().iter().map(|stage| {
            let status = task
                .stages
                .get(stage)
                .cloned()
                .unwrap_or(StageStatus::NotStarted);
            let is_current = stage == current_stage;

            let (color, text_color) = match &status {
                StageStatus::Done => (
                    cx.theme().colors().ghost_element_selected,
                    ui::Color::Success,
                ),
                StageStatus::Failed => {
                    (cx.theme().colors().ghost_element_selected, ui::Color::Error)
                }
                StageStatus::InProgress(_) => {
                    (cx.theme().colors().ghost_element_hover, ui::Color::Warning)
                }
                StageStatus::NotStarted => {
                    if is_current {
                        (cx.theme().colors().ghost_element_hover, ui::Color::Default)
                    } else {
                        (
                            cx.theme().colors().ghost_element_background,
                            ui::Color::Muted,
                        )
                    }
                }
            };

            div().px_1().rounded_sm().bg(color).child(
                Label::new(stage.label())
                    .size(ui::LabelSize::XSmall)
                    .color(text_color),
            )
        }))
}

fn render_dynamic_badges(stages: &[DynStage], cx: &App) -> impl IntoElement {
    h_flex().gap_0p5().children(stages.iter().map(|stage| {
        let (color, text_color) = match &stage.status {
            StageStatus::Done => (
                cx.theme().colors().ghost_element_selected,
                ui::Color::Success,
            ),
            StageStatus::Failed => (cx.theme().colors().ghost_element_selected, ui::Color::Error),
            StageStatus::InProgress(_) => {
                (cx.theme().colors().ghost_element_hover, ui::Color::Warning)
            }
            StageStatus::NotStarted => (
                cx.theme().colors().ghost_element_background,
                ui::Color::Muted,
            ),
        };

        div().px_1().rounded_sm().bg(color).child(
            Label::new(stage.label.clone())
                .size(ui::LabelSize::XSmall)
                .color(text_color),
        )
    }))
}

impl PartialEq for WorkformeTask {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.stages == other.stages
            && self.dynamic_stages == other.dynamic_stages
    }
}

impl Panel for WorkformePanel {
    fn persistent_name() -> &'static str {
        "WorkformePanel"
    }

    fn panel_key() -> &'static str {
        WORKFORME_PANEL_KEY
    }

    fn position(&self, _: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _position: DockPosition, _: &mut Window, _cx: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _cx: &App) -> Pixels {
        px(280.)
    }

    fn icon(&self, _: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _: &Window, _cx: &App) -> Option<&'static str> {
        Some("Workforme Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        5
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<WorkformePanel>(window, cx);
        });
    })
    .detach();
}

pub async fn load(
    workspace: WeakEntity<Workspace>,
    mut cx: AsyncWindowContext,
) -> anyhow::Result<Entity<WorkformePanel>> {
    let weak = workspace.clone();
    workspace
        .update_in(&mut cx, |_workspace, _window, cx| {
            cx.new(|cx| WorkformePanel::new(weak, cx))
        })
        .context("failed to create WorkformePanel")
}

// --- WorkformeTaskView: Rich view for task details ---

pub struct WorkformeTaskView {
    focus_handle: FocusHandle,
    task: WorkformeTask,
    workforme_root: PathBuf,
    workspace: WeakEntity<Workspace>,
    _poll_task: Task<()>,
}

impl WorkformeTaskView {
    pub fn new(
        task: WorkformeTask,
        workforme_root: PathBuf,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let poll_task = cx.spawn({
            let workforme_root = workforme_root.clone();
            let task_id = task.id.clone();
            async move |this: WeakEntity<Self>, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(3)).await;
                    let task_dir = workforme_root.join(".work").join(&task_id);
                    let updated = cx
                        .background_executor()
                        .spawn(async move { parse_task_dir(&task_dir) })
                        .await;
                    let result = this.update(cx, |view, cx| {
                        if let Some(updated) = updated {
                            if view.task != updated {
                                view.task = updated;
                                cx.notify();
                            }
                        }
                    });
                    if result.is_err() {
                        break;
                    }
                }
            }
        });

        Self {
            focus_handle: cx.focus_handle(),
            task,
            workforme_root,
            workspace,
            _poll_task: poll_task,
        }
    }

    fn work_dir(&self) -> PathBuf {
        self.workforme_root.join(".work").join(&self.task.id)
    }

    fn next_stage(&self) -> Option<&'static str> {
        let stages = Stage::all();
        for stage in stages {
            let status = self
                .task
                .stages
                .get(stage)
                .cloned()
                .unwrap_or(StageStatus::NotStarted);
            match status {
                StageStatus::Done => continue,
                StageStatus::InProgress(_) => return None,
                StageStatus::Failed | StageStatus::NotStarted => {
                    return Some(stage.command());
                }
            }
        }
        None
    }

    fn repo_path(&self) -> Option<PathBuf> {
        self.task.worktree.as_ref().map(|w| PathBuf::from(w))
    }

    fn spawn_command(
        &self,
        command: &str,
        args: Vec<String>,
        label: &str,
        cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let spawn = SpawnInTerminal {
            id: TaskId(format!("workforme-{}", label)),
            full_label: label.to_string(),
            label: label.to_string(),
            command: Some(command.to_string()),
            args,
            command_label: label.to_string(),
            cwd,
            env: HashMap::default(),
            use_new_terminal: true,
            allow_concurrent_runs: true,
            reveal: RevealStrategy::Always,
            reveal_target: RevealTarget::Dock,
            hide: HideStrategy::Never,
            shell: Shell::System,
            show_summary: false,
            show_command: false,
            show_rerun: false,
            save: SaveStrategy::default(),
        };
        if let Some(ws) = self.workspace.upgrade() {
            ws.update(cx, |ws, cx| {
                ws.spawn_in_terminal(spawn, window, cx).detach();
            });
        }
    }

    fn run_stage(&self, stage_cmd: &str, _window: &mut Window, _cx: &mut Context<Self>) {
        let task_dir = self.workforme_root.join(".work").join(&self.task.id);

        let stage_name = match stage_cmd {
            "impl" => "implement",
            other => other,
        };

        // failed/running は retry（stop-agents + reset + run）、それ以外は直接 run
        let stage_enum = Stage::all().iter().find(|s| s.command() == stage_cmd);
        let needs_retry = stage_enum.map_or(false, |s| {
            matches!(
                self.task.stages.get(s),
                Some(StageStatus::Failed | StageStatus::InProgress(_))
            )
        });

        let script = if needs_retry {
            self.workforme_root.join("scripts/retry-stage.sh")
        } else {
            self.workforme_root.join("scripts/run-stage.sh")
        };

        std::process::Command::new("bash")
            .arg(&script)
            .arg(&task_dir)
            .arg(stage_name)
            .spawn()
            .ok();
    }

    fn open_session(
        &self,
        short_id: &str,
        stage_label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = format!("[{}]{}", stage_label, &self.task.id);
        self.spawn_command(
            "claude",
            vec!["attach".to_string(), short_id.to_string()],
            &label,
            self.repo_path(),
            window,
            cx,
        );
    }
}

impl EventEmitter<ItemEvent> for WorkformeTaskView {}

impl Focusable for WorkformeTaskView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkformeTaskView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let repo_info = self.task.repo.clone().unwrap_or_else(|| "—".to_string());
        let next_stage = self.next_stage().map(|s| s.to_string());
        let repo_path = self.repo_path();
        let entity = cx.entity().downgrade();

        div()
            .id("workforme-task-view")
            .flex()
            .flex_col()
            .size_full()
            .p_4()
            .gap_3()
            .overflow_y_scroll()
            .track_focus(&self.focus_handle)
            // Title + meta table
            .child(
                v_flex()
                    .gap_1()
                    .child(Label::new(self.task.title.clone()).size(ui::LabelSize::Large))
                    .child(
                        v_flex()
                            .gap_0p5()
                            .py_1()
                            // Repo row
                            .child(
                                h_flex()
                                    .child(div().w(px(60.)).child(Label::new("Repo").size(ui::LabelSize::XSmall).color(ui::Color::Muted)))
                                    .child(Label::new(repo_info.clone()).size(ui::LabelSize::XSmall))
                                    .when_some(self.task.task_type.clone(), |el, t| {
                                        el.child(
                                            div().ml_2().px_1().rounded_sm().bg(cx.theme().colors().element_background)
                                                .child(Label::new(t).size(ui::LabelSize::XSmall).color(ui::Color::Accent))
                                        )
                                    })
                            )
                            // Branch row (clickable → open repo)
                            .when_some(self.task.branch.clone(), |el, branch| {
                                let worktree = self.task.worktree.clone();
                                el.child(
                                    h_flex()
                                        .child(div().w(px(60.)).child(Label::new("Branch").size(ui::LabelSize::XSmall).color(ui::Color::Muted)))
                                        .child(
                                            div()
                                                .id("wfm-open-repo")
                                                .cursor_pointer()
                                                .child(Label::new(format!("{}  ↗", branch)).size(ui::LabelSize::XSmall).color(ui::Color::Accent))
                                                .on_click(move |_event, _window, _cx| {
                                                    if let Some(ref path) = worktree {
                                                        std::process::Command::new("/Applications/Zed.app/Contents/MacOS/cli")
                                                            .arg("-n")
                                                            .arg(path)
                                                            .spawn()
                                                            .ok();
                                                    }
                                                })
                                        )
                                )
                            })
                            // Issue row (clickable → open in browser)
                            .when_some(self.task.url.clone(), |el, url| {
                                let url_clone = url.clone();
                                el.child(
                                    h_flex()
                                        .child(div().w(px(60.)).child(Label::new("Issue").size(ui::LabelSize::XSmall).color(ui::Color::Muted)))
                                        .child(
                                            div()
                                                .id("wfm-issue-url")
                                                .cursor_pointer()
                                                .child(Label::new(url).size(ui::LabelSize::XSmall).color(ui::Color::Accent))
                                                .on_click(move |_event, _window, cx| {
                                                    cx.open_url(&url_clone);
                                                })
                                        )
                                )
                            })
                            // PR row (clickable → open in browser)
                            .when_some(self.task.pr_url.clone(), |el, pr_url| {
                                let pr_url_clone = pr_url.clone();
                                el.child(
                                    h_flex()
                                        .child(div().w(px(60.)).child(Label::new("PR").size(ui::LabelSize::XSmall).color(ui::Color::Muted)))
                                        .child(
                                            div()
                                                .id("wfm-pr-url")
                                                .cursor_pointer()
                                                .child(Label::new(pr_url).size(ui::LabelSize::XSmall).color(ui::Color::Accent))
                                                .on_click(move |_event, _window, cx| {
                                                    cx.open_url(&pr_url_clone);
                                                })
                                        )
                                )
                            })
                    ),
            )
            // Pipeline visualization
            .child(div().h(px(1.)).bg(cx.theme().colors().border))
            .when(self.task.uses_dynamic_stages(), |el| {
                let entity = entity.clone();
                el.child(
                    v_flex()
                        .py_2()
                        .pl_2()
                        .children(self.task.dynamic_stages.iter().enumerate().map(|(i, dyn_stage)| {
                            let (indicator, color) = match &dyn_stage.status {
                                StageStatus::Done => ("●", ui::Color::Success),
                                StageStatus::Failed => ("✗", ui::Color::Error),
                                StageStatus::InProgress(_) => ("◐", ui::Color::Warning),
                                StageStatus::NotStarted => ("○", ui::Color::Muted),
                            };
                            let status_label = match &dyn_stage.status {
                                StageStatus::Done => "done",
                                StageStatus::Failed => "failed",
                                StageStatus::InProgress(_) => "running",
                                StageStatus::NotStarted => "—",
                            };
                            let mut row = h_flex()
                                .gap_2()
                                .py_0p5()
                                .items_center()
                                .child(Label::new(indicator).size(ui::LabelSize::Default).color(color))
                                .child(
                                    div().w(px(80.)).child(
                                        Label::new(dyn_stage.label.clone()).size(ui::LabelSize::Small).color(color)
                                    )
                                )
                                .child(
                                    div().w(px(60.)).child(
                                        Label::new(status_label).size(ui::LabelSize::XSmall).color(ui::Color::Muted)
                                    )
                                );
                            if let Some(ref sid) = dyn_stage.session_id {
                                let entity_for_attach = entity.clone();
                                let sid = sid.clone();
                                let stage_label_for_attach = dyn_stage.label.clone();
                                let is_running = matches!(dyn_stage.status, StageStatus::InProgress(_));
                                let btn_label = if is_running { "⏎ Attach" } else { "⏎ Resume" };
                                let btn_color = if is_running { ui::Color::Warning } else { ui::Color::Default };
                                row = row.child(
                                    div()
                                        .id(gpui::ElementId::NamedInteger("wfm-dyn-attach".into(), i as u64))
                                        .px_2()
                                        .py_0p5()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                                        .child(Label::new(btn_label).size(ui::LabelSize::XSmall).color(btn_color))
                                        .on_click(move |_event, window, cx| {
                                            if let Some(view) = entity_for_attach.upgrade() {
                                                let short_id = sid[..8.min(sid.len())].to_string();
                                                let stage_label = stage_label_for_attach.clone();
                                                view.update(cx, |view, cx| {
                                                    view.open_session(&short_id, &stage_label, window, cx);
                                                });
                                            }
                                        }),
                                );
                            }
                            // View button for output file ({key}.md or {key}-*.md)
                            let work = self.work_dir();
                            let output_path = {
                                let p = work.join(format!("{}.md", dyn_stage.key));
                                if p.exists() {
                                    Some(p)
                                } else {
                                    std::fs::read_dir(&work).ok()
                                        .and_then(|entries| entries
                                            .flatten()
                                            .map(|e| e.path())
                                            .find(|p| {
                                                p.file_name()
                                                    .and_then(|n| n.to_str())
                                                    .map_or(false, |n| n.starts_with(&dyn_stage.key) && n.ends_with(".md"))
                                            }))
                                }
                            };
                            if let Some(output_path) = output_path {
                                let entity_for_view = entity.clone();
                                row = row.child(
                                    div()
                                        .id(gpui::ElementId::NamedInteger("wfm-dyn-view".into(), i as u64))
                                        .px_2()
                                        .py_0p5()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                                        .child(Label::new("View").size(ui::LabelSize::XSmall).color(ui::Color::Default))
                                        .on_click(move |_event, window, cx| {
                                            if let Some(view) = entity_for_view.upgrade() {
                                                view.update(cx, |view, cx| {
                                                    if let Some(ws) = view.workspace.upgrade() {
                                                        ws.update(cx, |ws, cx| {
                                                            ws.open_abs_path(output_path.clone(), workspace::OpenOptions::default(), window, cx).detach();
                                                        });
                                                    }
                                                });
                                            }
                                        }),
                                );
                            }
                            row.into_any_element()
                        })),
                )
            })
            .when(!self.task.uses_dynamic_stages(), |el| el.child(
                v_flex()
                    .py_2()
                    .pl_2()
                    .children(Stage::all().iter().enumerate().flat_map(|(i, stage)| {
                        let status = self.task.stages.get(stage).cloned().unwrap_or(StageStatus::NotStarted);
                        let is_next = next_stage.as_deref() == Some(stage.command());
                        let stage_cmd = stage.command();
                        let file_path = {
                            let p = self.work_dir().join(stage.output_filename());
                            if p.exists() { Some(p) } else { None }
                        };
                        let optional = stage.is_optional();

                        let (indicator, status_label, color) = match &status {
                            StageStatus::Done => ("●", "done", ui::Color::Success),
                            StageStatus::Failed => ("✗", "failed", ui::Color::Error),
                            StageStatus::InProgress(_) => ("◐", "running", ui::Color::Warning),
                            StageStatus::NotStarted if is_next => ("→", "ready", ui::Color::Accent),
                            StageStatus::NotStarted if optional => ("◇", "optional", ui::Color::Muted),
                            StageStatus::NotStarted => ("○", "—", ui::Color::Muted),
                        };

                        let connector: Option<gpui::AnyElement> = if i > 0 {
                            Some(
                                div()
                                    .ml(px(7.))
                                    .w(px(1.))
                                    .h(px(8.))
                                    .bg(cx.theme().colors().border)
                                    .into_any_element()
                            )
                        } else {
                            None
                        };

                        let row = h_flex()
                            .gap_2()
                            .py_0p5()
                            .items_center()
                            .child(Label::new(indicator).size(ui::LabelSize::Default).color(color))
                            .child(
                                div().w(px(80.)).child(
                                    Label::new(stage.label()).size(ui::LabelSize::Small).color(color)
                                )
                            )
                            .child(
                                div().w(px(60.)).child(
                                    Label::new(status_label).size(ui::LabelSize::XSmall).color(ui::Color::Muted)
                                )
                            )
                            // Run button: prerequisite が done のときのみ表示
                            .when({
                                let can_run = matches!(status, StageStatus::NotStarted | StageStatus::Failed) && stage.prerequisite().map_or(true, |pre| {
                                    self.task.stages.get(pre).map_or(false, |s| matches!(s, StageStatus::Done))
                                });
                                can_run
                            }, |el| {
                                let entity_for_run2 = entity.clone();
                                let stage_cmd_str2 = stage_cmd.to_string();
                                el.child(
                                    div()
                                        .id(gpui::ElementId::NamedInteger("wfm-run".into(), i as u64))
                                        .px_2()
                                        .py_0p5()
                                        .rounded_md()
                                        .bg(cx.theme().colors().ghost_element_hover)
                                        .cursor_pointer()
                                        .hover(|s| s.bg(cx.theme().colors().ghost_element_selected))
                                        .child(Label::new("▶ Run").size(ui::LabelSize::XSmall).color(ui::Color::Accent))
                                        .on_click(move |_event, window, cx| {
                                            if let Some(view) = entity_for_run2.upgrade() {
                                                view.update(cx, |view, cx| {
                                                    view.run_stage(&stage_cmd_str2, window, cx);
                                                });
                                            }
                                        }),
                                )
                            })
                            // Attach/Resume button (for any stage with a session)
                            .when_some(self.task.sessions.get(stage).cloned(), |el, sid| {
                                let entity_for_attach = entity.clone();
                                let stage_label_for_attach = stage.label().to_string();
                                let is_running = matches!(status, StageStatus::InProgress(_));
                                let label = if is_running { "⏎ Attach" } else { "⏎ Resume" };
                                let color = if is_running { ui::Color::Warning } else { ui::Color::Default };
                                el.child(
                                    div()
                                        .id(gpui::ElementId::NamedInteger("wfm-attach".into(), i as u64))
                                        .px_2()
                                        .py_0p5()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                                        .child(Label::new(label).size(ui::LabelSize::XSmall).color(color))
                                        .on_click(move |_event, window, cx| {
                                            if let Some(view) = entity_for_attach.upgrade() {
                                                let short_id = sid[..8.min(sid.len())].to_string();
                                                let stage_label = stage_label_for_attach.clone();
                                                view.update(cx, |view, cx| {
                                                    view.open_session(&short_id, &stage_label, window, cx);
                                                });
                                            }
                                        }),
                                )
                            })
                            // View file button (if output exists)
                            .when_some(file_path, |el, path| {
                                let entity_for_view = entity.clone();
                                el.child(
                                    div()
                                        .id(gpui::ElementId::NamedInteger("wfm-view".into(), i as u64))
                                        .px_2()
                                        .py_0p5()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                                        .child(Label::new("View").size(ui::LabelSize::XSmall).color(ui::Color::Default))
                                        .on_click(move |_event, window, cx| {
                                            if let Some(view) = entity_for_view.upgrade() {
                                                view.update(cx, |view, cx| {
                                                    if let Some(ws) = view.workspace.upgrade() {
                                                        ws.update(cx, |ws, cx| {
                                                            ws.open_abs_path(path.clone(), workspace::OpenOptions::default(), window, cx).detach();
                                                        });
                                                    }
                                                });
                                            }
                                        }),
                                )
                            })
                            // Fix button (for check/codex-review when done)
                            .when(
                                matches!(stage, Stage::Check | Stage::CodexReview) && matches!(status, StageStatus::Done),
                                |el| {
                                    let entity_for_fix = entity.clone();
                                    el.child(
                                        div()
                                            .id(gpui::ElementId::NamedInteger("wfm-fix".into(), i as u64))
                                            .px_2()
                                            .py_0p5()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                                            .child(Label::new("Fix").size(ui::LabelSize::XSmall).color(ui::Color::Warning))
                                            .on_click(move |_event, window, cx| {
                                                if let Some(view) = entity_for_fix.upgrade() {
                                                    view.update(cx, |view, cx| {
                                                        view.run_stage("fix", window, cx);
                                                    });
                                                }
                                            }),
                                    )
                                },
                            )
                            .into_any_element();

                        connector.into_iter().chain(std::iter::once(row))
                    })),
            ))
            // Divider
            .child(div().h(px(1.)).bg(cx.theme().colors().border))
            // Quick actions
            .child(
                h_flex()
                    .gap_2()
                    .py_1()
                    .when_some(repo_path, |el, path| {
                        el.child(
                            div()
                                .id("wfm-action-window")
                                .px_2()
                                .py_0p5()
                                .rounded_md()
                                .bg(cx.theme().colors().ghost_element_hover)
                                .cursor_pointer()
                                .hover(|s| s.bg(cx.theme().colors().ghost_element_selected))
                                .child(Label::new("Open Repo").size(ui::LabelSize::Small))
                                .on_click(move |_event, _window, _cx| {
                                    std::process::Command::new("/Applications/Zed.app/Contents/MacOS/cli")
                                        .arg("-n")
                                        .arg(&path)
                                        .spawn()
                                        .ok();
                                }),
                        )
                    }),
            )
    }
}

impl Item for WorkformeTaskView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> gpui::SharedString {
        self.task.id.clone().into()
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}
