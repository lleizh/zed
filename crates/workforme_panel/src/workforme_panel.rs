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
    Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    item::{Item, ItemEvent},
    notifications::NotificationId,
};
use settings::{RegisterSetting, Settings, SettingsContent};
use zed_actions::RevealTarget;

#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct WorkformePanelSettings {
    pub workforme_root: String,
    pub github_owner: String,
    pub poll_interval_secs: u64,
    pub my_tasks_poll_interval_secs: u64,
    pub search_days: u64,
    pub notify_on_completion: bool,
}

impl Settings for WorkformePanelSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let panel = content.workforme_panel.as_ref().unwrap();
        Self {
            workforme_root: panel.workforme_root.clone().unwrap(),
            github_owner: panel.github_owner.clone().unwrap(),
            poll_interval_secs: panel.poll_interval_secs.unwrap(),
            my_tasks_poll_interval_secs: panel.my_tasks_poll_interval_secs.unwrap(),
            search_days: panel.search_days.unwrap(),
            notify_on_completion: panel.notify_on_completion.unwrap(),
        }
    }
}

const WORKFORME_PANEL_KEY: &str = "WorkformePanel";

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

#[derive(Debug, Clone, PartialEq)]
enum PrState {
    None,
    Open,
    Approved,
    ChangesRequested,
    Merged,
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
    pr_number: Option<u64>,
    pr_state: PrState,
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum PanelTab {
    IssueDev,
    PrReview,
    Analyst,
}

impl PanelTab {
    fn label(&self) -> &'static str {
        match self {
            PanelTab::IssueDev => "Dev",
            PanelTab::PrReview => "Review",
            PanelTab::Analyst => "Analyst",
        }
    }

}

pub struct WorkformePanel {
    focus_handle: FocusHandle,
    active_tab: PanelTab,
    tasks: Vec<WorkformeTask>,
    collapsed_repos: std::collections::HashSet<String>,
    deleting_ids: std::collections::HashSet<String>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    workforme_root: PathBuf,
    workspace: WeakEntity<Workspace>,
    _poll_task: Task<()>,
}

impl EventEmitter<PanelEvent> for WorkformePanel {}

impl WorkformePanel {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let settings = WorkformePanelSettings::get_global(cx);
        let raw_root = settings.workforme_root.clone();
        let expanded = shellexpand::tilde(&raw_root).to_string();
        let workforme_root = PathBuf::from(expanded);

        let poll_task = cx.spawn({
            async move |this: WeakEntity<Self>, cx| {
                loop {
                    let (work_dir, cached_states) = {
                        let Ok(result) = this.update(cx, |panel, _cx| {
                            let dir = panel.workforme_root.join(".work");
                            let states: HashMap<String, PrState> = panel
                                .tasks
                                .iter()
                                .filter(|t| t.pr_state != PrState::None)
                                .map(|t| (t.id.clone(), t.pr_state.clone()))
                                .collect();
                            (dir, states)
                        }) else {
                            break;
                        };
                        result
                    };
                    let tasks = cx
                        .background_executor()
                        .spawn(async move {
                            let mut tasks = load_tasks_from_disk(&work_dir);
                            for task in tasks.iter_mut() {
                                if let Some(state) = cached_states.get(&task.id) {
                                    task.pr_state = state.clone();
                                }
                            }
                            check_pr_states(&mut tasks);
                            tasks
                        })
                        .await;
                    let result = this.update(cx, |panel, cx| {
                        let filtered: Vec<WorkformeTask> = tasks
                            .into_iter()
                            .filter(|t| !panel.deleting_ids.contains(&t.id))
                            .collect();
                        // ID が磁盘上もう存在しなければ deleting_ids からも除去
                        panel.deleting_ids.retain(|id| {
                            filtered.iter().any(|t| &t.id == id)
                        });
                        if panel.tasks != filtered {
                            // stage 状態変化を検出して通知
                            let notifications =
                                detect_stage_completions(&panel.tasks, &filtered);
                            if !notifications.is_empty() {
                                let should_notify =
                                    WorkformePanelSettings::get_global(cx)
                                        .notify_on_completion;
                                if should_notify {
                                    if let Some(ws) = panel.workspace.upgrade() {
                                        ws.update(cx, |ws, cx| {
                                            for msg in &notifications {
                                                ws.show_toast(
                                                    Toast::new(
                                                        NotificationId::named(
                                                            format!("wfm-stage-{}", &msg[..20.min(msg.len())]).into(),
                                                        ),
                                                        msg.clone(),
                                                    )
                                                    .autohide(),
                                                    cx,
                                                );
                                            }
                                        });
                                    }
                                    for msg in &notifications {
                                        send_system_notification(msg);
                                    }
                                }
                            }
                            panel.tasks = filtered;
                            cx.notify();
                        }
                    });
                    if result.is_err() {
                        break;
                    }
                    let poll_secs = this
                        .update(cx, |_panel, cx| {
                            WorkformePanelSettings::get_global(cx).poll_interval_secs
                        })
                        .unwrap_or(3);
                    cx.background_executor()
                        .timer(Duration::from_secs(poll_secs))
                        .await;
                }
            }
        });

        let mut panel = Self {
            focus_handle: cx.focus_handle(),
            active_tab: PanelTab::IssueDev,
            tasks: Vec::new(),
            collapsed_repos: std::collections::HashSet::new(),
            deleting_ids: std::collections::HashSet::new(),
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
                    if let Some(panel) = entity_for_delete.upgrade() {
                        let tid = task_id_for_delete.clone();
                        panel.update(cx, |panel, cx| {
                            panel.deleting_ids.insert(tid.clone());
                            panel.tasks.retain(|t| t.id != tid);
                            cx.notify();
                        });
                    }
                    let task_dir = root_for_delete.join(".work").join(&task_id_for_delete);
                    let script = root_for_delete.join("scripts/delete-task.sh");
                    std::process::Command::new("bash")
                        .arg(&script)
                        .arg(&task_dir)
                        .spawn()
                        .ok();
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

impl WorkformePanel {
    fn filtered_tasks(&self) -> Vec<&WorkformeTask> {
        self.tasks
            .iter()
            .filter(|t| match self.active_tab {
                PanelTab::IssueDev => {
                    t.task_type.as_deref() != Some("review")
                        && t.task_type.as_deref() != Some("analyst")
                }
                PanelTab::PrReview => t.task_type.as_deref() == Some("review"),
                PanelTab::Analyst => t.task_type.as_deref() == Some("analyst"),
            })
            .collect()
    }
}

impl Render for WorkformePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = self.workspace.clone();
        let workspace_for_header = self.workspace.clone();
        let workforme_root = self.workforme_root.clone();
        let active_tab = self.active_tab;

        let filtered = self.filtered_tasks();

        // Group tasks by repository
        let mut repos: Vec<String> = filtered
            .iter()
            .map(|t| t.repo.clone().unwrap_or_else(|| "(no repo)".to_string()))
            .collect();
        repos.sort();
        repos.dedup();

        let collapsed = self.collapsed_repos.clone();
        let entity = cx.entity().downgrade();

        let mut date_sections: Vec<gpui::AnyElement> = Vec::new();
        let mut global_index = 0usize;

        for repo in &repos {
            let is_collapsed = collapsed.contains(repo);
            let repo_clone = repo.clone();
            let entity_clone = entity.clone();
            let task_count = filtered
                .iter()
                .filter(|t| {
                    t.repo.as_deref().unwrap_or("(no repo)") == repo.as_str()
                })
                .count();

            // Repo header
            let header = h_flex()
                .id(gpui::ElementId::Name(format!("wfm-repo-{}", repo).into()))
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
                    Label::new(format!(" {} ({})", repo, task_count))
                        .size(ui::LabelSize::Small)
                        .color(ui::Color::Muted),
                )
                .on_click(move |_event, _window, cx| {
                    if let Some(panel) = entity_clone.upgrade() {
                        panel.update(cx, |panel, cx| {
                            if panel.collapsed_repos.contains(&repo_clone) {
                                panel.collapsed_repos.remove(&repo_clone);
                            } else {
                                panel.collapsed_repos.insert(repo_clone.clone());
                            }
                            cx.notify();
                        });
                    }
                });

            date_sections.push(header.into_any_element());

            // Task rows (if not collapsed), sorted by date descending
            if !is_collapsed {
                let mut repo_tasks: Vec<&WorkformeTask> = filtered
                    .iter()
                    .filter(|t| {
                        t.repo.as_deref().unwrap_or("(no repo)") == repo.as_str()
                    })
                    .copied()
                    .collect();
                repo_tasks.sort_by(|a, b| {
                    b.date.cmp(&a.date).then_with(|| b.id.cmp(&a.id))
                });

                for task in repo_tasks {
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
                global_index += task_count;
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
                        h_flex()
                            .gap_2()
                            .children([PanelTab::IssueDev, PanelTab::PrReview, PanelTab::Analyst].iter().map(|tab| {
                                let tab = *tab;
                                let entity = cx.entity().downgrade();
                                div()
                                    .id(gpui::ElementId::Name(format!("wfm-tab-{}", tab.label()).into()))
                                    .cursor_pointer()
                                    .px_1()
                                    .when(active_tab == tab, |el| {
                                        el.border_b_2()
                                            .border_color(cx.theme().colors().text)
                                    })
                                    .child(
                                        Label::new(tab.label())
                                            .size(ui::LabelSize::Small)
                                            .color(if active_tab == tab {
                                                ui::Color::Default
                                            } else {
                                                ui::Color::Muted
                                            }),
                                    )
                                    .on_click(move |_event, _window, cx| {
                                        if let Some(panel) = entity.upgrade() {
                                            panel.update(cx, |panel, cx| {
                                                panel.active_tab = tab;
                                                cx.notify();
                                            });
                                        }
                                    })
                            }))
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child({
                                let entity = cx.entity().downgrade();
                                IconButton::new("wfm-refresh", IconName::ArrowCircle)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Refresh"))
                                    .on_click(move |_event, _window, cx| {
                                        if let Some(panel) = entity.upgrade() {
                                            panel.update(cx, |panel, cx| {
                                                panel.deleting_ids.clear();
                                                panel.reload(cx);
                                            });
                                        }
                                    })
                            })
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

fn send_system_notification(message: &str) {
    let escaped = message.replace('\\', "\\\\").replace('"', "\\\"");
    std::process::Command::new("osascript")
        .args([
            "-e",
            &format!(
                "display notification \"{}\" with title \"Workforme\"",
                escaped
            ),
        ])
        .spawn()
        .ok();
}

fn detect_stage_completions(
    old_tasks: &[WorkformeTask],
    new_tasks: &[WorkformeTask],
) -> Vec<String> {
    let mut notifications = Vec::new();
    for new_task in new_tasks {
        let Some(old_task) = old_tasks.iter().find(|t| t.id == new_task.id) else {
            continue;
        };
        let short_id = &new_task.id[..8.min(new_task.id.len())];
        // static stages
        for (stage, new_status) in &new_task.stages {
            let old_status = old_task.stages.get(stage);
            let was_in_progress = matches!(old_status, Some(StageStatus::InProgress(_)));
            if was_in_progress {
                match new_status {
                    StageStatus::Done => {
                        notifications.push(format!(
                            "✓ [{}] {} completed",
                            short_id,
                            stage.command()
                        ));
                    }
                    StageStatus::Failed => {
                        notifications.push(format!(
                            "✗ [{}] {} failed",
                            short_id,
                            stage.command()
                        ));
                    }
                    _ => {}
                }
            }
        }
        // dynamic stages
        for (i, new_dyn) in new_task.dynamic_stages.iter().enumerate() {
            let old_dyn = old_task.dynamic_stages.get(i);
            let was_in_progress =
                old_dyn.map_or(false, |d| matches!(d.status, StageStatus::InProgress(_)));
            if was_in_progress {
                match new_dyn.status {
                    StageStatus::Done => {
                        notifications
                            .push(format!("✓ [{}] {} completed", short_id, new_dyn.label));
                    }
                    StageStatus::Failed => {
                        notifications
                            .push(format!("✗ [{}] {} failed", short_id, new_dyn.label));
                    }
                    _ => {}
                }
            }
        }
    }
    notifications
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

fn check_pr_states(tasks: &mut [WorkformeTask]) {
    // repo/number → task index のマッピング
    let mut queries: Vec<(usize, String, u64)> = Vec::new();
    for (i, task) in tasks.iter().enumerate() {
        if task.pr_state == PrState::Merged {
            continue;
        }
        if let (Some(repo), Some(pr_number)) = (task.repo.as_deref(), task.pr_number) {
            queries.push((i, repo.to_string(), pr_number));
        }
    }
    if queries.is_empty() {
        return;
    }

    // GraphQL で一括取得
    let mut query_parts = Vec::new();
    for (idx, (_, repo, pr_number)) in queries.iter().enumerate() {
        query_parts.push(format!(
            "pr{idx}: repository(owner: \"ajainc\", name: \"{repo}\") {{ pullRequest(number: {pr_number}) {{ state reviewDecision }} }}"
        ));
    }
    let graphql_query = format!("{{ {} }}", query_parts.join(" "));

    let output = std::process::Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={}", graphql_query)])
        .output();

    let Ok(output) = output else {
        return;
    };
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return;
    };
    let Some(data) = parsed.get("data") else {
        return;
    };

    for (idx, (task_idx, _, _)) in queries.iter().enumerate() {
        let key = format!("pr{idx}");
        let Some(pr_data) = data.get(&key).and_then(|r| r.get("pullRequest")) else {
            continue;
        };
        let state = pr_data.get("state").and_then(|v| v.as_str()).unwrap_or("");
        let review_decision = pr_data.get("reviewDecision").and_then(|v| v.as_str()).unwrap_or("");
        tasks[*task_idx].pr_state = match state {
            "MERGED" => PrState::Merged,
            _ => match review_decision {
                "APPROVED" => PrState::Approved,
                "CHANGES_REQUESTED" => PrState::ChangesRequested,
                _ => PrState::Open,
            },
        };
    }
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
    let pr_number = state_value
        .get("pr_number")
        .and_then(|v| v.as_u64())
        .or_else(|| {
            pr_url
                .as_deref()?
                .rsplit('/')
                .next()?
                .parse::<u64>()
                .ok()
        });
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
        pr_number,
        pr_state: PrState::None,
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
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(
                            Label::new(task.title.clone())
                                .size(ui::LabelSize::Default)
                                .single_line(),
                        ),
                )
                .when(task.pr_state != PrState::None && task.pr_state != PrState::Open, |el| {
                    let (label, color) = match &task.pr_state {
                        PrState::Merged => ("Merged", gpui::rgb(0x8957e5)),
                        PrState::Approved => ("Approved", gpui::rgb(0x238636)),
                        PrState::ChangesRequested => ("Changes", gpui::rgb(0xd29922)),
                        _ => ("", gpui::rgb(0x000000)),
                    };
                    el.child(
                        div()
                            .flex_shrink_0()
                            .px_1()
                            .rounded_sm()
                            .bg(color)
                            .child(
                                Label::new(label)
                                    .size(ui::LabelSize::XSmall)
                                    .color(ui::Color::Default),
                            ),
                    )
                }),
        )
        .child(
            h_flex()
                .gap_1()
                .items_center()
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
            && self.pr_state == other.pr_state
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
    busy_action: Option<String>,
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
                        .spawn(async move {
                            let mut task = parse_task_dir(&task_dir);
                            if let Some(ref mut t) = task {
                                let mut tasks = [t.clone()];
                                check_pr_states(&mut tasks);
                                t.pr_state = tasks[0].pr_state.clone();
                            }
                            task
                        })
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
            busy_action: None,
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
        let toast_label = label.to_string();
        if let Some(ws) = self.workspace.upgrade() {
            ws.update(cx, |ws, cx| {
                let task = ws.spawn_in_terminal(spawn, window, cx);
                let ws_handle = cx.entity().downgrade();
                cx.spawn(async move |_this, cx| {
                    let result = task.await;
                    let message = match result {
                        Some(Ok(status)) if status.success() => {
                            format!("✓ {} completed", toast_label)
                        }
                        Some(Ok(status)) => {
                            format!("✗ {} failed (exit {})", toast_label, status)
                        }
                        Some(Err(e)) => {
                            format!("✗ {} error: {}", toast_label, e)
                        }
                        None => return,
                    };
                    ws_handle
                        .update(cx, |ws, cx| {
                            let should_notify =
                                WorkformePanelSettings::get_global(cx)
                                    .notify_on_completion;
                            if should_notify {
                                ws.show_toast(
                                    Toast::new(
                                        NotificationId::named(
                                            format!("workforme-done-{}", toast_label).into(),
                                        ),
                                        message.clone(),
                                    )
                                    .autohide(),
                                    cx,
                                );
                                send_system_notification(&message);
                            }
                        })
                        .ok();
                })
                .detach();
            });
        }
    }

    fn run_stage(&self, stage_cmd: &str, _window: &mut Window, _cx: &mut Context<Self>) {
        let task_dir = self.workforme_root.join(".work").join(&self.task.id);

        let stage_name = match stage_cmd {
            "impl" => "implement",
            other => other,
        };

        // failed/running は redo、それ以外は直接 run
        let needs_redo = if let Some(stage_enum) = Stage::all().iter().find(|s| s.command() == stage_cmd) {
            matches!(
                self.task.stages.get(stage_enum),
                Some(StageStatus::Failed | StageStatus::InProgress(_))
            )
        } else {
            self.task.dynamic_stages.iter().any(|ds| {
                ds.key == stage_cmd
                    && matches!(ds.status, StageStatus::Failed | StageStatus::InProgress(_))
            })
        };

        let script = if needs_redo {
            self.workforme_root.join("scripts/redo-stage.sh")
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
                                let (pr_badge_label, pr_badge_color) = match &self.task.pr_state {
                                    PrState::Merged => (Some("Merged"), gpui::rgb(0x8957e5)),
                                    PrState::Approved => (Some("Approved"), gpui::rgb(0x238636)),
                                    PrState::ChangesRequested => (Some("Changes Requested"), gpui::rgb(0xd29922)),
                                    _ => (None, gpui::rgb(0x000000)),
                                };
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
                                        .when_some(pr_badge_label, |el, label| {
                                            el.child(
                                                div()
                                                    .ml_2()
                                                    .px_1()
                                                    .rounded_sm()
                                                    .bg(pr_badge_color)
                                                    .child(
                                                        Label::new(label)
                                                            .size(ui::LabelSize::XSmall)
                                                            .color(ui::Color::Default),
                                                    ),
                                            )
                                        })
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
                            if matches!(dyn_stage.status, StageStatus::NotStarted | StageStatus::Failed) {
                                let entity_for_run = entity.clone();
                                let stage_key_for_run = dyn_stage.key.clone();
                                row = row.child(
                                    div()
                                        .id(gpui::ElementId::NamedInteger("wfm-dyn-run".into(), i as u64))
                                        .px_2()
                                        .py_0p5()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                                        .child(Label::new("▶ Run").size(ui::LabelSize::XSmall).color(ui::Color::Accent))
                                        .on_click(move |_event, window, cx| {
                                            if let Some(view) = entity_for_run.upgrade() {
                                                let stage_key = stage_key_for_run.clone();
                                                view.update(cx, |view, cx| {
                                                    view.run_stage(&stage_key, window, cx);
                                                });
                                            }
                                        }),
                                );
                            }
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
                            // Redo button (for done stages; hidden for plan/pr, and hidden after PR is done unless fixer)
                            .when(
                                matches!(status, StageStatus::Done)
                                    && !matches!(stage, Stage::Plan | Stage::Pr)
                                    && (matches!(stage, Stage::Fixer)
                                        || !matches!(self.task.stages.get(&Stage::Pr), Some(StageStatus::Done))),
                                |el| {
                                    let stage_cmd_for_redo = stage_cmd.to_string();
                                    let workforme_root_for_redo = self.workforme_root.clone();
                                    let task_id_for_redo = self.task.id.clone();
                                    el.child(
                                        div()
                                            .id(gpui::ElementId::NamedInteger("wfm-redo".into(), i as u64))
                                            .px_2()
                                            .py_0p5()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                                            .child(Label::new("↺ Redo").size(ui::LabelSize::XSmall).color(ui::Color::Muted))
                                            .on_click(move |_event, _window, _cx| {
                                                let script = workforme_root_for_redo.join("scripts/redo-stage.sh");
                                                let task_dir = workforme_root_for_redo.join(".work").join(&task_id_for_redo);
                                                let stage_name = match stage_cmd_for_redo.as_str() {
                                                    "impl" => "implement",
                                                    other => other,
                                                };
                                                std::process::Command::new("bash")
                                                    .arg(&script)
                                                    .arg(&task_dir)
                                                    .arg(stage_name)
                                                    .spawn()
                                                    .ok();
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
                    })
                    .when(
                        matches!(self.task.stages.get(&Stage::Implement), Some(StageStatus::Done))
                            && !matches!(self.task.stages.get(&Stage::Pr), Some(StageStatus::Done)),
                        |el| {
                            let autofix_running = self.work_dir().join(".autofix.pid").exists();
                            let workforme_root = self.workforme_root.clone();
                            let task_id = self.task.id.clone();
                            if autofix_running {
                                let workforme_root_stop = workforme_root.clone();
                                let task_id_stop = task_id.clone();
                                el.child(
                                    div()
                                        .id("wfm-action-stop-autofix")
                                        .px_2()
                                        .py_0p5()
                                        .rounded_md()
                                        .bg(cx.theme().colors().ghost_element_hover)
                                        .cursor_pointer()
                                        .hover(|s| s.bg(cx.theme().colors().ghost_element_selected))
                                        .child(Label::new("■ Stop").size(ui::LabelSize::Small).color(ui::Color::Error))
                                        .on_click(move |_event, _window, _cx| {
                                            let script = workforme_root_stop.join("scripts/stop-autofix.sh");
                                            let task_dir = workforme_root_stop.join(".work").join(&task_id_stop);
                                            std::process::Command::new("bash")
                                                .arg(&script)
                                                .arg(&task_dir)
                                                .spawn()
                                                .ok();
                                        }),
                                )
                            } else {
                                el.child(
                                    div()
                                        .id("wfm-action-autofix")
                                        .px_2()
                                        .py_0p5()
                                        .rounded_md()
                                        .bg(cx.theme().colors().ghost_element_hover)
                                        .cursor_pointer()
                                        .hover(|s| s.bg(cx.theme().colors().ghost_element_selected))
                                        .child(Label::new("Autofix").size(ui::LabelSize::Small).color(ui::Color::Accent))
                                        .on_click(move |_event, _window, _cx| {
                                            let script = workforme_root.join("scripts/autofix.sh");
                                            let task_dir = workforme_root.join(".work").join(&task_id);
                                            std::process::Command::new("bash")
                                                .arg(&script)
                                                .arg(&task_dir)
                                                .spawn()
                                                .ok();
                                        }),
                                )
                            }
                        },
                    )
                    // Approve button (for review tasks with pr_number)
                    .when(
                        self.task.task_type.as_deref() == Some("review")
                            && self.task.pr_number.is_some()
                            && self.task.repo.is_some()
                            && self.busy_action.as_deref() != Some("approve"),
                        |el| {
                            let repo = self.task.repo.clone().unwrap();
                            let pr_number = self.task.pr_number.unwrap();
                            let workforme_root = self.workforme_root.clone();
                            let entity = cx.entity().downgrade();
                            el.child(
                                div()
                                    .id("wfm-action-approve")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_md()
                                    .bg(cx.theme().colors().ghost_element_hover)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(cx.theme().colors().ghost_element_selected))
                                    .child(Label::new("✓ Approve").size(ui::LabelSize::Small).color(ui::Color::Success))
                                    .on_click(move |_event, _window, cx| {
                                        if let Some(view) = entity.upgrade() {
                                            view.update(cx, |view, cx| {
                                                view.busy_action = Some("approve".to_string());
                                                cx.notify();
                                            });
                                        }
                                        let org = std::process::Command::new("python3")
                                            .arg(workforme_root.join("scripts/state.py"))
                                            .arg("config")
                                            .arg("github_org")
                                            .output()
                                            .ok()
                                            .and_then(|o| String::from_utf8(o.stdout).ok())
                                            .unwrap_or_default()
                                            .trim()
                                            .to_string();
                                        std::process::Command::new("gh")
                                            .args(["api", &format!("repos/{}/{}/pulls/{}/reviews", org, repo, pr_number),
                                                   "--method", "POST", "-f", "event=APPROVE", "-f", "body=LGTM"])
                                            .spawn()
                                            .ok();
                                    }),
                            )
                        },
                    )
                    .when(self.busy_action.as_deref() == Some("approve"), |el| {
                        el.child(
                            div()
                                .id("wfm-action-approve-busy")
                                .px_2()
                                .py_0p5()
                                .rounded_md()
                                .bg(cx.theme().colors().ghost_element_selected)
                                .child(Label::new("Approving...").size(ui::LabelSize::Small).color(ui::Color::Muted)),
                        )
                    })
                    // Merge button (for non-review tasks with approved PR)
                    .when(
                        self.task.task_type.as_deref() != Some("review")
                            && self.task.pr_number.is_some()
                            && self.task.repo.is_some()
                            && self.task.pr_state == PrState::Approved
                            && self.busy_action.as_deref() != Some("merge"),
                        |el| {
                            let repo = self.task.repo.clone().unwrap();
                            let pr_number = self.task.pr_number.unwrap();
                            let entity = cx.entity().downgrade();
                            el.child(
                                div()
                                    .id("wfm-action-merge")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_md()
                                    .bg(cx.theme().colors().ghost_element_hover)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(cx.theme().colors().ghost_element_selected))
                                    .child(Label::new("⤵ Merge").size(ui::LabelSize::Small).color(ui::Color::Accent))
                                    .on_click(move |_event, _window, cx| {
                                        if let Some(view) = entity.upgrade() {
                                            view.update(cx, |view, cx| {
                                                view.busy_action = Some("merge".to_string());
                                                cx.notify();
                                            });
                                        }
                                        std::process::Command::new("gh")
                                            .args([
                                                "pr", "merge",
                                                &pr_number.to_string(),
                                                "--repo", &format!("ajainc/{}", repo),
                                                "--squash",
                                            ])
                                            .spawn()
                                            .ok();
                                    }),
                            )
                        },
                    )
                    .when(self.busy_action.as_deref() == Some("merge"), |el| {
                        el.child(
                            div()
                                .id("wfm-action-merge-busy")
                                .px_2()
                                .py_0p5()
                                .rounded_md()
                                .bg(cx.theme().colors().ghost_element_selected)
                                .child(Label::new("Merging...").size(ui::LabelSize::Small).color(ui::Color::Muted)),
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
