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

/// stage key → 表示ラベル
fn tmux_session_exists(name: &str) -> bool {
    std::process::Command::new("tmux")
        .args(["has-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn stage_label(key: &str) -> &str {
    match key {
        "plan" => "Plan",
        "implement" => "Impl",
        "check" => "Check",
        "codex-review" => "Codex",
        "fix" => "Fix",
        "pr" => "PR",
        "fixer" => "Fixer",
        "dig" => "Dig",
        "review-pr" => "Review",
        "analyst" => "Analyst",
        "slack-summary" => "Slack",
        "slack-reply" => "Reply",
        "create-issue" => "Issue",
        _ => key,
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
    runner: Option<String>,
    tmux_session: Option<String>,
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
    issues_created: Vec<String>,
    inbox_item: Option<workforme_common::InboxItem>,
    created_inbox_items: Vec<workforme_common::InboxItem>,
    review_posted: bool,
    worktree: Option<String>,
    dynamic_stages: Vec<DynStage>,
}

impl WorkformeTask {
    /// 最初の未完了ステージのインデックスを返す
    #[allow(dead_code)]
    fn current_stage_index(&self) -> Option<usize> {
        self.dynamic_stages
            .iter()
            .position(|s| !matches!(s.status, StageStatus::Done))
    }

    fn latest_session_id(&self) -> Option<&str> {
        self.dynamic_stages
            .iter()
            .rev()
            .find_map(|s| s.session_id.as_deref())
    }

    fn latest_runner(&self) -> Option<&str> {
        self.dynamic_stages
            .iter()
            .rev()
            .find_map(|s| s.session_id.as_ref().and_then(|_| s.runner.as_deref()))
    }

    fn latest_tmux_session(&self) -> Option<&str> {
        self.dynamic_stages
            .iter()
            .rev()
            .find_map(|s| s.tmux_session.as_deref())
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
                    let work_dir = {
                        let Ok(dir) = this.update(cx, |panel, _cx| {
                            panel.workforme_root.join(".work")
                        }) else {
                            break;
                        };
                        dir
                    };
                    let tasks = cx
                        .background_executor()
                        .spawn(async move {
                            let mut tasks = load_tasks_from_disk(&work_dir);
                            check_pr_states(&mut tasks, &work_dir);
                            tasks
                        })
                        .await;
                    let result = this.update(cx, |panel, cx| {
                        // ディスク上に存在しなくなった ID を deleting_ids から除去
                        panel.deleting_ids.retain(|id| {
                            tasks.iter().any(|t| &t.id == id)
                        });
                        let filtered: Vec<WorkformeTask> = tasks
                            .into_iter()
                            .filter(|t| !panel.deleting_ids.contains(&t.id))
                            .collect();
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
                                            for notif in &notifications {
                                                ws.show_toast(
                                                    Toast::new(
                                                        NotificationId::named(
                                                            format!("wfm-stage-{}", &notif.message[..20.min(notif.message.len())]).into(),
                                                        ),
                                                        notif.message.clone(),
                                                    )
                                                    .autohide(),
                                                    cx,
                                                );
                                            }
                                        });
                                    }
                                    for notif in &notifications {
                                        send_system_notification(notif, &panel.workforme_root);
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
        runner: Option<String>,
        tmux_session: Option<String>,
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
                let runner_for_term = runner.clone();
                let tmux_for_term = tmux_session.clone();
                menu = menu.custom_entry(
                    move |_window, _cx| Label::new("Open in Terminal").into_any_element(),
                    {
                        let sid = sid_clone;
                        let ws = ws.clone();
                        let cwd = repo_for_term.clone();
                        let runner = runner_for_term.clone();
                        let tmux = tmux_for_term.clone();
                        move |window, cx| {
                            let script = match runner.as_deref() {
                                Some("codex") => {
                                    let dir = cwd.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| ".".to_string());
                                    let tmux_name = tmux.as_deref().unwrap_or(&sid);
                                    if tmux_session_exists(tmux_name) {
                                        format!("tmux attach -t '{}'", tmux_name)
                                    } else {
                                        format!("codex resume --dangerously-bypass-approvals-and-sandbox -C '{}' {}", dir, sid)
                                    }
                                }
                                _ => {
                                    let short_id = sid[..8.min(sid.len())].to_string();
                                    format!(
                                        "claude attach {} 2>/dev/null || claude --resume {}",
                                        short_id, sid
                                    )
                                }
                            };
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
                .py_1()
                .mt_1()
                .border_b_1()
                .border_color(cx.theme().colors().border)
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
                        .color(ui::Color::Default),
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

fn send_system_notification(notif: &StageNotification, _workforme_root: &Path) {
    let message = &notif.message;

    // macOS notification
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

struct StageNotification {
    message: String,
}

fn detect_stage_completions(
    old_tasks: &[WorkformeTask],
    new_tasks: &[WorkformeTask],
) -> Vec<StageNotification> {
    let mut notifications = Vec::new();
    for new_task in new_tasks {
        let Some(old_task) = old_tasks.iter().find(|t| t.id == new_task.id) else {
            continue;
        };
        let repo = new_task.repo.clone().unwrap_or_default();
        let task_label = if let Some(pr) = new_task.pr_number {
            format!("{}#{}", repo, pr)
        } else {
            let number = new_task.id.rsplit('-').next().and_then(|s| s.parse::<u64>().ok());
            if let Some(n) = number {
                format!("{}#{}", repo, n)
            } else {
                new_task.id.clone()
            }
        };
        for (i, new_dyn) in new_task.dynamic_stages.iter().enumerate() {
            let old_dyn = old_task.dynamic_stages.get(i);
            let was_in_progress =
                old_dyn.map_or(false, |d| matches!(d.status, StageStatus::InProgress(_)));
            if was_in_progress {
                match new_dyn.status {
                    StageStatus::Done => {
                        notifications.push(StageNotification {
                            message: format!("✓ [{}] {} completed\n{}", task_label, new_dyn.label, new_task.title),
                        });
                    }
                    StageStatus::Failed => {
                        notifications.push(StageNotification {
                            message: format!("✗ [{}] {} failed\n{}", task_label, new_dyn.label, new_task.title),
                        });
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

fn check_pr_states(tasks: &mut [WorkformeTask], work_dir: &Path) {
    let inbox = workforme_common::load_inbox(&work_dir.parent().unwrap_or(work_dir));

    let mut inbox_map: HashMap<(String, u64), &workforme_common::InboxItem> = HashMap::new();
    for item in &inbox.items {
        if item.number > 0 {
            inbox_map.insert((item.repo.clone(), item.number), item);
        }
    }

    for task in tasks.iter_mut() {
        if task.inbox_item.as_ref().map(|i| i.status.as_str()) == Some("merged") {
            continue;
        }
        if let (Some(repo), Some(pr_number)) = (task.repo.as_deref(), task.pr_number) {
            if let Some(inbox_item) = inbox_map.get(&(repo.to_string(), pr_number)) {
                task.inbox_item = Some((*inbox_item).clone());
            }
        }
        // analyst task: issues_created を inbox_item として関連付ける
        if !task.issues_created.is_empty() {
            let mut created_items = Vec::new();
            for issue_url in &task.issues_created {
                if let Some(number) = issue_url.rsplit('/').next().and_then(|s| s.parse::<u64>().ok()) {
                    if let Some(repo) = task.repo.as_deref() {
                        if let Some(inbox_item) = inbox_map.get(&(repo.to_string(), number)) {
                            created_items.push((*inbox_item).clone());
                        }
                    }
                }
            }
            task.created_inbox_items = created_items;
        }
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
    let review_posted = state_value
        .get("review_posted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let issues_created = state_value
        .get("issues_created")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let mut dynamic_stages = Vec::new();

    if let Some(stages_obj) = state_value.get("stages").and_then(|v| v.as_object()) {
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
                let runner = stage_val
                    .get("runner")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let tmux_session = stage_val
                    .get("tmux_session")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let status = match status_str {
                    "done" => StageStatus::Done,
                    "failed" => StageStatus::Failed,
                    "running" => StageStatus::InProgress(session_id.clone()),
                    _ => StageStatus::NotStarted,
                };
                let label = stage_label(key).to_string();
                dynamic_stages.push(DynStage {
                    key: key.clone(),
                    label,
                    status,
                    session_id,
                    runner,
                    tmux_session,
                });
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
        issues_created,
        inbox_item: None,
        created_inbox_items: Vec::new(),
        review_posted,
        worktree,
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
    let session_id = task.latest_session_id().map(|s| s.to_string());
    let runner = task.latest_runner().map(|s| s.to_string());
    let tmux_session = task.latest_tmux_session().map(|s| s.to_string());
    let task_id = task.id.clone();
    let task_clone = task.clone();
    let workspace = workspace.clone();
    let panel = panel.clone();
    let workforme_root = workforme_root.to_path_buf();
    let cwd = task.worktree.as_ref().map(|w| PathBuf::from(w));
    let ctx_session_id = session_id.clone();
    let ctx_runner = runner.clone();
    let ctx_tmux = tmux_session.clone();
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
            let ctx_runner = ctx_runner.clone();
            let ctx_tmux = ctx_tmux.clone();
            let ctx_repo_path = ctx_repo_path.clone();
            let ctx_task_id = task_id.clone();
            move |event, window, cx| {
                if let Some(panel) = panel.upgrade() {
                    panel.update(cx, |panel, cx| {
                        panel.deploy_context_menu(
                            event.position,
                            ctx_session_id.clone(),
                            ctx_runner.clone(),
                            ctx_tmux.clone(),
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
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
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
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(render_dynamic_badges(&task.dynamic_stages, cx)),
                        ),
                )
                .when_some(task.inbox_item.as_ref(), |el, inbox_item| {
                    el.child(workforme_common::render_item_status(inbox_item, None))
                        .when(!inbox_item.review_requested_label && inbox_item.status != "merged" && inbox_item.status != "closed", |el| {
                            el.child(
                                Label::new("no-label")
                                    .size(ui::LabelSize::XSmall)
                                    .color(ui::Color::Warning)
                            )
                        })
                })
                .when(!task.created_inbox_items.is_empty(), |el| {
                    let open_count = task.created_inbox_items.iter().filter(|i| i.status == "open").count();
                    let closed_count = task.created_inbox_items.iter().filter(|i| i.status == "closed").count();
                    let other_count = task.created_inbox_items.len() - open_count - closed_count;
                    let mut parts = Vec::new();
                    if open_count > 0 { parts.push((format!("{} open", open_count), 0x238636u32)); }
                    if closed_count > 0 { parts.push((format!("{} closed", closed_count), 0xda3633u32)); }
                    if other_count > 0 { parts.push((format!("{} other", other_count), 0x848d97u32)); }
                    el.child(
                        h_flex().gap_1().children(parts.into_iter().map(|(label, color)| {
                            div()
                                .px_1()
                                .rounded_sm()
                                .bg(gpui::rgb(color))
                                .child(Label::new(label).size(ui::LabelSize::XSmall).color(ui::Color::Default))
                                .into_any_element()
                        }))
                    )
                }),
        )
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
            && self.dynamic_stages == other.dynamic_stages
            && self.inbox_item == other.inbox_item
            && self.review_posted == other.review_posted
            && self.issues_created == other.issues_created
            && self.created_inbox_items == other.created_inbox_items
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

#[derive(Debug, Clone, PartialEq)]
struct ReviewFindings {
    verdict: String,
    summary: String,
    items: Vec<ReviewFinding>,
}

#[derive(Debug, Clone, PartialEq)]
struct ReviewFinding {
    severity: String,
    file: String,
    line: u64,
    text: String,
    selected: bool,
    posted: bool,
}

fn submit_review(
    entity: &WeakEntity<WorkformeTaskView>,
    workforme_root: &PathBuf,
    repo: &str,
    pr_number: u64,
    event: &str,
    cx: &mut App,
) {
    let (task_id, selected_keys) = entity
        .upgrade()
        .map(|v| {
            let view = v.read(cx);
            let id = view.task.id.clone();
            let keys: Vec<String> = view.review_findings.as_ref()
                .map(|r| r.items.iter()
                    .filter(|f| f.selected)
                    .map(|f| format!("{}:{}", f.file, f.line))
                    .collect())
                .unwrap_or_default();
            (id, keys)
        })
        .unwrap_or_default();
    if let Some(view) = entity.upgrade() {
        view.update(cx, |view, cx| {
            view.busy_action = Some("submit_review".to_string());
            cx.notify();
        });
    }
    let root = workforme_root.clone();
    let repo = repo.to_string();
    let event = event.to_string();
    let (comments, summary) = entity
        .upgrade()
        .map(|view| {
            let v = view.read(cx);
            let items: Vec<(String, u64, String)> = v.review_findings
                .as_ref()
                .map(|r| {
                    r.items
                        .iter()
                        .filter(|f| f.selected)
                        .map(|f| (f.file.clone(), f.line, f.text.clone()))
                        .collect()
                })
                .unwrap_or_default();
            let sum = if v.summary_selected {
                v.review_findings.as_ref()
                    .map(|r| r.summary.clone())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            (items, sum)
        })
        .unwrap_or_default();
    std::thread::spawn(move || {
        let org = std::process::Command::new("python3")
            .arg(root.join("scripts/state.py"))
            .arg("config")
            .arg("github_org")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default()
            .trim()
            .to_string();
        let commit_id = std::process::Command::new("gh")
            .args([
                "pr", "view", &pr_number.to_string(),
                "--repo", &format!("{}/{}", org, repo),
                "--json", "headRefOid", "--jq", ".headRefOid",
            ])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default()
            .trim()
            .to_string();
        // Build review JSON body
        let inline_comments: Vec<serde_json::Value> = comments
            .iter()
            .filter(|(file, line, _)| !file.is_empty() && *line > 0)
            .map(|(file, line, text)| {
                serde_json::json!({"path": file, "line": *line, "body": text})
            })
            .collect();
        let body_comments: Vec<String> = comments
            .iter()
            .filter(|(file, line, _)| file.is_empty() || *line == 0)
            .map(|(_, _, text)| text.clone())
            .collect();
        let mut body_parts: Vec<String> = Vec::new();
        if !summary.is_empty() {
            body_parts.push(summary);
        }
        if !body_comments.is_empty() {
            body_parts.push(body_comments.join("\n\n"));
        }
        let body = body_parts.join("\n\n---\n\n");
        let mut review_json = serde_json::json!({
            "event": event,
            "commit_id": commit_id,
            "body": body,
        });
        if !inline_comments.is_empty() {
            review_json["comments"] = serde_json::Value::Array(inline_comments);
        }
        let json_str = serde_json::to_string(&review_json).unwrap_or_default();
        use std::io::Write;
        let mut success = false;
        let mut child = std::process::Command::new("gh")
            .args([
                "api",
                &format!("repos/{}/{}/pulls/{}/reviews", org, repo, pr_number),
                "--method", "POST",
                "--input", "-",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .ok();
        if let Some(ref mut c) = child {
            if let Some(ref mut stdin) = c.stdin.take() {
                stdin.write_all(json_str.as_bytes()).ok();
            }
            let stderr_output = c.stderr.take().and_then(|mut e| {
                let mut buf = String::new();
                use std::io::Read;
                e.read_to_string(&mut buf).ok()?;
                Some(buf)
            });
            if let Ok(status) = c.wait() {
                success = status.success();
                if !success {
                    log::error!("workforme: review post failed: {}", stderr_output.unwrap_or_default());
                }
            }
        }
        if !success {
            return;
        }
        // posted_findings を state.json に永続化
        let state_path = root.join(".work").join(&task_id).join("state.json");
        if let Ok(content) = std::fs::read_to_string(&state_path) {
            if let Ok(mut state) = serde_json::from_str::<serde_json::Value>(&content) {
                state["review_posted"] = serde_json::Value::Bool(true);
                let mut existing: Vec<String> = state.get("posted_findings")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();
                for key in &selected_keys {
                    if !existing.contains(key) {
                        existing.push(key.clone());
                    }
                }
                state["posted_findings"] = serde_json::Value::Array(
                    existing.into_iter().map(|s| serde_json::Value::String(s)).collect()
                );
                if let Ok(json) = serde_json::to_string_pretty(&state) {
                    std::fs::write(&state_path, json).ok();
                }
            }
        }
        trigger_sync_inbox(&root);
    });
}

fn trigger_sync_inbox(workforme_root: &Path) {
    let script = workforme_root.join("scripts/sync-inbox.sh");
    let root = workforme_root.to_path_buf();
    std::thread::spawn(move || {
        std::process::Command::new(&script)
            .current_dir(&root)
            .output()
            .ok();
    });
}

fn parse_review_findings(task_dir: &Path) -> Option<ReviewFindings> {
    let json_path = task_dir.join("review-pr-findings.json");
    let content = std::fs::read_to_string(&json_path).ok()?;
    let val = serde_json::from_str::<serde_json::Value>(&content).ok()?;

    // state.json から posted 済み finding を読み取る
    let posted_set: std::collections::HashSet<String> = std::fs::read_to_string(task_dir.join("state.json"))
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|s| s.get("posted_findings")?.as_array().cloned())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    let verdict = val.get("verdict").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let summary = val.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut items = Vec::new();
    for (severity, key) in [("critical", "critical"), ("informational", "informational")] {
        if let Some(arr) = val.get(key).and_then(|v| v.as_array()) {
            for item in arr {
                let file = item.get("file").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let line = item.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
                let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let finding_key = format!("{}:{}", file, line);
                items.push(ReviewFinding {
                    severity: severity.to_string(),
                    file,
                    line,
                    text,
                    selected: false,
                    posted: posted_set.contains(&finding_key),
                });
            }
        }
    }
    Some(ReviewFindings { verdict, summary, items })
}

pub struct WorkformeTaskView {
    focus_handle: FocusHandle,
    task: WorkformeTask,
    workforme_root: PathBuf,
    workspace: WeakEntity<Workspace>,
    busy_action: Option<String>,
    review_findings: Option<ReviewFindings>,
    expanded_findings: std::collections::HashSet<usize>,
    summary_selected: bool,
    summary_expanded: bool,
    review_submitted: bool,
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
                    let work_dir = workforme_root.join(".work");
                    let updated = cx
                        .background_executor()
                        .spawn(async move {
                            let mut task = parse_task_dir(&task_dir);
                            if let Some(ref mut t) = task {
                                let mut tasks = [t.clone()];
                                check_pr_states(&mut tasks, &work_dir);
                                t.inbox_item = tasks[0].inbox_item.clone();
                                t.created_inbox_items = tasks[0].created_inbox_items.clone();
                            }
                            task
                        })
                        .await;
                    let result = this.update(cx, |view, cx| {
                        if let Some(updated) = updated {
                            if view.task != updated {
                                if view.busy_action.is_some() {
                                    view.busy_action = None;
                                }
                                if updated.review_posted && !view.review_submitted {
                                    view.review_submitted = true;
                                }
                                view.task = updated;
                                cx.notify();
                            }
                            // findings は毎回読み直す（selected 状態は保持）
                            let new_findings = parse_review_findings(
                                &view.workforme_root.join(".work").join(&view.task.id),
                            );
                            if let Some(ref new_f) = new_findings {
                                if let Some(ref cur_f) = view.review_findings {
                                    if cur_f.summary == new_f.summary
                                        && cur_f.items.len() == new_f.items.len()
                                        && cur_f.items.iter().zip(new_f.items.iter()).all(|(a, b)| {
                                            a.file == b.file && a.line == b.line && a.text == b.text
                                        })
                                    {
                                        // 内容同じなら selected を保持
                                    } else {
                                        view.review_findings = new_findings;
                                        view.summary_selected = false;
                                        cx.notify();
                                    }
                                } else {
                                    view.review_findings = new_findings;
                                    cx.notify();
                                }
                            } else if view.review_findings.is_some() {
                                view.review_findings = None;
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

        let review_findings = parse_review_findings(
            &workforme_root.join(".work").join(&task.id),
        );

        let review_submitted = task.review_posted;
        Self {
            focus_handle: cx.focus_handle(),
            task,
            workforme_root,
            workspace,
            busy_action: None,
            review_findings,
            expanded_findings: std::collections::HashSet::new(),
            summary_selected: false,
            summary_expanded: false,
            review_submitted,
            _poll_task: poll_task,
        }
    }

    fn work_dir(&self) -> PathBuf {
        self.workforme_root.join(".work").join(&self.task.id)
    }

    fn next_stage_key(&self) -> Option<&str> {
        for stage in &self.task.dynamic_stages {
            match &stage.status {
                StageStatus::Done => continue,
                StageStatus::InProgress(_) => return None,
                StageStatus::Failed | StageStatus::NotStarted => {
                    return Some(&stage.key);
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
        let needs_redo = self.task.dynamic_stages.iter().any(|ds| {
            ds.key == stage_cmd
                && matches!(ds.status, StageStatus::Failed | StageStatus::InProgress(_))
        });

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
        session_id: &str,
        stage_label: &str,
        runner: Option<&str>,
        tmux_session: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = format!("[{}]{}", stage_label, &self.task.id);
        let (cmd, args) = match runner {
            Some("codex") => {
                let tmux_name = tmux_session.unwrap_or(session_id);
                if tmux_session_exists(tmux_name) {
                    ("tmux", vec!["attach".to_string(), "-t".to_string(), tmux_name.to_string()])
                } else {
                    let dir = self.repo_path().map(|p| p.display().to_string()).unwrap_or_else(|| ".".to_string());
                    ("codex", vec!["resume".to_string(), "--dangerously-bypass-approvals-and-sandbox".to_string(), "-C".to_string(), dir, session_id.to_string()])
                }
            }
            _ => (
                "claude",
                vec!["attach".to_string(), session_id[..8.min(session_id.len())].to_string()],
            ),
        };
        self.spawn_command(
            cmd,
            args,
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
        let next_stage_key = self.next_stage_key().map(|s| s.to_string());
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
                                        .when_some(self.task.inbox_item.as_ref(), |el, inbox_item| {
                                            el.child(workforme_common::render_item_status(inbox_item, None))
                                        })
                                )
                            })
                            // Created issues rows
                            .when(!self.task.issues_created.is_empty(), |el| {
                                el.children(self.task.issues_created.iter().enumerate().map(|(i, issue_url)| {
                                    let url_clone = issue_url.clone();
                                    let inbox_item = self.task.created_inbox_items.get(i);
                                    let status = inbox_item.map(|item| item.status.as_str()).unwrap_or("");
                                    h_flex()
                                        .gap_1()
                                        .child(div().w(px(60.)).child(
                                            Label::new(if i == 0 { "Created" } else { "" })
                                                .size(ui::LabelSize::XSmall)
                                                .color(ui::Color::Muted)
                                        ))
                                        .child(
                                            div()
                                                .id(gpui::ElementId::NamedInteger("wfm-created-issue".into(), i as u64))
                                                .cursor_pointer()
                                                .child(Label::new(issue_url.clone()).size(ui::LabelSize::XSmall).color(ui::Color::Accent))
                                                .on_click(move |_event, _window, cx| {
                                                    cx.open_url(&url_clone);
                                                })
                                        )
                                        .when(!status.is_empty(), |el| {
                                            el.child(
                                                div()
                                                    .flex_shrink_0()
                                                    .px_1()
                                                    .rounded_sm()
                                                    .bg(gpui::rgb(workforme_common::status_color(status)))
                                                    .child(
                                                        Label::new(status.to_string())
                                                            .size(ui::LabelSize::XSmall)
                                                            .color(ui::Color::Default)
                                                    )
                                            )
                                        })
                                        .into_any_element()
                                }))
                            })
                    ),
            )
            // Pipeline visualization
            .child(div().h(px(1.)).bg(cx.theme().colors().border))
            .child({
                let entity = entity.clone();
                let pr_is_done = self.task.dynamic_stages.iter().any(|s| s.key == "pr" && matches!(s.status, StageStatus::Done));
                v_flex()
                    .py_2()
                    .pl_2()
                    .children(self.task.dynamic_stages.iter().enumerate().flat_map(|(i, dyn_stage)| {
                        let is_next = next_stage_key.as_deref() == Some(dyn_stage.key.as_str());
                        let (indicator, color) = match &dyn_stage.status {
                            StageStatus::Done => ("●", ui::Color::Success),
                            StageStatus::Failed => ("✗", ui::Color::Error),
                            StageStatus::InProgress(_) => ("◐", ui::Color::Warning),
                            StageStatus::NotStarted if is_next => ("→", ui::Color::Accent),
                            StageStatus::NotStarted => ("○", ui::Color::Muted),
                        };
                        let status_label = match &dyn_stage.status {
                            StageStatus::Done => "done",
                            StageStatus::Failed => "failed",
                            StageStatus::InProgress(_) => "running",
                            StageStatus::NotStarted if is_next => "ready",
                            StageStatus::NotStarted => "—",
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
                        // Run button
                        if matches!(dyn_stage.status, StageStatus::NotStarted | StageStatus::Failed) {
                            let entity_for_run = entity.clone();
                            let stage_key_for_run = dyn_stage.key.clone();
                            row = row.child(
                                div()
                                    .id(gpui::ElementId::NamedInteger("wfm-run".into(), i as u64))
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
                        // Attach/Resume button
                        let is_codex = dyn_stage.runner.as_deref() == Some("codex");
                        let has_session = if is_codex {
                            dyn_stage.tmux_session.is_some()
                        } else {
                            dyn_stage.session_id.is_some()
                        };
                        if has_session {
                            let entity_for_attach = entity.clone();
                            let sid = dyn_stage.session_id.clone().unwrap_or_default();
                            let stage_runner = dyn_stage.runner.clone();
                            let stage_tmux = dyn_stage.tmux_session.clone();
                            let stage_label_for_attach = dyn_stage.label.clone();
                            let is_running = matches!(dyn_stage.status, StageStatus::InProgress(_));
                            let btn_label = if is_running { "⏎ Attach" } else { "⏎ Resume" };
                            let btn_color = if is_running { ui::Color::Warning } else { ui::Color::Default };
                            row = row.child(
                                div()
                                    .id(gpui::ElementId::NamedInteger("wfm-attach".into(), i as u64))
                                    .px_2()
                                    .py_0p5()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                                    .child(Label::new(btn_label).size(ui::LabelSize::XSmall).color(btn_color))
                                    .on_click(move |_event, window, cx| {
                                        if let Some(view) = entity_for_attach.upgrade() {
                                            let session_id = sid.clone();
                                            let stage_label = stage_label_for_attach.clone();
                                            let runner = stage_runner.clone();
                                            let tmux = stage_tmux.clone();
                                            view.update(cx, |view, cx| {
                                                view.open_session(&session_id, &stage_label, runner.as_deref(), tmux.as_deref(), window, cx);
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
                                                        ws.open_abs_path(output_path.clone(), workspace::OpenOptions::default(), window, cx).detach();
                                                    });
                                                }
                                            });
                                        }
                                    }),
                            );
                        }
                        // Redo button (for done stages; hidden for plan/pr)
                        if matches!(dyn_stage.status, StageStatus::Done)
                            && dyn_stage.key != "plan"
                            && dyn_stage.key != "pr"
                            && (dyn_stage.key == "fixer" || !pr_is_done)
                        {
                            let workforme_root_for_redo = self.workforme_root.clone();
                            let task_id_for_redo = self.task.id.clone();
                            let stage_key_for_redo = dyn_stage.key.clone();
                            row = row.child(
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
                                        std::process::Command::new("bash")
                                            .arg(&script)
                                            .arg(&task_dir)
                                            .arg(&stage_key_for_redo)
                                            .spawn()
                                            .ok();
                                    }),
                            );
                        }

                        connector.into_iter().chain(std::iter::once(row.into_any_element()))
                    }))
            })
            // Review findings (pr-review workflow only)
            .when(self.review_findings.as_ref().map_or(false, |r| !r.items.is_empty() || !r.summary.is_empty()), |el| {
                let review = self.review_findings.as_ref().unwrap();
                let entity = cx.entity().downgrade();
                let (verdict_label, verdict_color) = match review.verdict.as_str() {
                    "APPROVE" => ("APPROVE", ui::Color::Success),
                    "REQUEST_CHANGES" => ("REQUEST_CHANGES", ui::Color::Error),
                    _ => ("COMMENT", ui::Color::Warning),
                };
                let has_selected = self.summary_selected || review.items.iter().any(|f| f.selected);
                let submitted = self.review_submitted;
                el.child(div().h(px(1.)).bg(cx.theme().colors().border))
                    .child(
                        v_flex()
                            .py_2()
                            .px_2()
                            .gap_1()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        Label::new("Review Findings")
                                            .size(ui::LabelSize::Small)
                                            .color(ui::Color::Default),
                                    )
                                    .child(
                                        Label::new(verdict_label)
                                            .size(ui::LabelSize::XSmall)
                                            .color(verdict_color),
                                    ),
                            )
                            .when(!review.summary.is_empty(), |el| {
                                let summary_selected = self.summary_selected;
                                let summary_expanded = self.summary_expanded;
                                let summary_text = review.summary.clone();
                                let entity_for_summary_check = entity.clone();
                                let entity_for_summary_toggle = entity.clone();
                                el.child(
                                    v_flex()
                                        .py_0p5()
                                        .gap_0p5()
                                        .child(
                                            h_flex()
                                                .id("wfm-summary-row")
                                                .gap_1()
                                                .items_center()
                                                .cursor_pointer()
                                                .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                                                .on_click(move |_event, _window, cx| {
                                                    if let Some(view) = entity_for_summary_toggle.upgrade() {
                                                        view.update(cx, |view, cx| {
                                                            view.summary_expanded = !view.summary_expanded;
                                                            cx.notify();
                                                        });
                                                    }
                                                })
                                                .child(
                                                    div()
                                                        .id("wfm-summary-check")
                                                        .w(px(14.))
                                                        .h(px(14.))
                                                        .rounded_sm()
                                                        .border_1()
                                                        .border_color(cx.theme().colors().border)
                                                        .when(summary_selected, |el| el.bg(cx.theme().colors().element_selected))
                                                        .cursor_pointer()
                                                        .flex_none()
                                                        .items_center()
                                                        .justify_center()
                                                        .when(summary_selected, |el| {
                                                            el.child(
                                                                Label::new("✓")
                                                                    .size(ui::LabelSize::XSmall)
                                                                    .color(ui::Color::Default),
                                                            )
                                                        })
                                                        .on_click(move |_event, _window, cx| {
                                                            cx.stop_propagation();
                                                            if let Some(view) = entity_for_summary_check.upgrade() {
                                                                view.update(cx, |view, cx| {
                                                                    view.summary_selected = !view.summary_selected;
                                                                    cx.notify();
                                                                });
                                                            }
                                                        }),
                                                )
                                                .child(
                                                    Label::new(if summary_expanded { "▼" } else { "▶" })
                                                        .size(ui::LabelSize::XSmall)
                                                        .color(ui::Color::Muted),
                                                )
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .overflow_hidden()
                                                        .child(
                                                            Label::new("Summary")
                                                                .size(ui::LabelSize::XSmall)
                                                                .color(ui::Color::Default)
                                                                .single_line(),
                                                        ),
                                                ),
                                        )
                                        .when(summary_expanded, |el| {
                                            el.child(
                                                div()
                                                    .ml_4()
                                                    .px_2()
                                                    .py_1()
                                                    .rounded_md()
                                                    .bg(cx.theme().colors().surface_background)
                                                    .child(
                                                        Label::new(summary_text)
                                                            .size(ui::LabelSize::XSmall)
                                                            .color(ui::Color::Default),
                                                    ),
                                            )
                                        }),
                                )
                            })
                            .children(review.items.iter().enumerate().map(|(i, finding)| {
                                let severity_color = if finding.severity == "critical" {
                                    ui::Color::Error
                                } else {
                                    ui::Color::Warning
                                };
                                let severity_label = if finding.severity == "critical" {
                                    "C"
                                } else {
                                    "I"
                                };
                                let is_expanded = self.expanded_findings.contains(&i);
                                let finding_text = finding.text.clone();
                                let finding_file = finding.file.clone();
                                let finding_line = finding.line;
                                let is_selected = finding.selected;
                                let is_posted = finding.posted;
                                let entity_for_toggle = entity.clone();
                                let entity_for_check = entity.clone();

                                let summary_line = if !finding_file.is_empty() && finding_line > 0 {
                                    format!("[{}:{}] {}", finding_file, finding_line, finding_text.chars().take(40).collect::<String>())
                                } else if !finding_file.is_empty() {
                                    format!("[{}] {}", finding_file, finding_text.chars().take(40).collect::<String>())
                                } else {
                                    finding_text.chars().take(60).collect::<String>()
                                };

                                v_flex()
                                    .id(gpui::ElementId::NamedInteger("wfm-finding".into(), i as u64))
                                    .py_0p5()
                                    .gap_0p5()
                                    .child(
                                        h_flex()
                                            .id(gpui::ElementId::NamedInteger("wfm-finding-row".into(), i as u64))
                                            .gap_1()
                                            .items_center()
                                            .cursor_pointer()
                                            .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                                            .on_click(move |_event, _window, cx| {
                                                if let Some(view) = entity_for_toggle.upgrade() {
                                                    view.update(cx, |view, cx| {
                                                        if view.expanded_findings.contains(&i) {
                                                            view.expanded_findings.remove(&i);
                                                        } else {
                                                            view.expanded_findings.insert(i);
                                                        }
                                                        cx.notify();
                                                    });
                                                }
                                            })
                                            // Checkbox or Posted badge
                                            .when(is_posted, |el| {
                                                el.child(
                                                    Label::new("✓")
                                                        .size(ui::LabelSize::XSmall)
                                                        .color(ui::Color::Success),
                                                )
                                            })
                                            .when(!is_posted, |el| {
                                                el.child(
                                                    div()
                                                        .id(gpui::ElementId::NamedInteger("wfm-finding-check".into(), i as u64))
                                                        .w(px(14.))
                                                        .h(px(14.))
                                                        .rounded_sm()
                                                        .border_1()
                                                        .border_color(cx.theme().colors().border)
                                                        .when(is_selected, |el| el.bg(cx.theme().colors().element_selected))
                                                        .cursor_pointer()
                                                        .flex_none()
                                                        .items_center()
                                                        .justify_center()
                                                        .when(is_selected, |el| {
                                                            el.child(
                                                                Label::new("✓")
                                                                    .size(ui::LabelSize::XSmall)
                                                                    .color(ui::Color::Default),
                                                            )
                                                        })
                                                        .on_click(move |_event, _window, cx| {
                                                            cx.stop_propagation();
                                                            if let Some(view) = entity_for_check.upgrade() {
                                                                view.update(cx, |view, cx| {
                                                                    if let Some(ref mut r) = view.review_findings {
                                                                        if let Some(f) = r.items.get_mut(i) {
                                                                            f.selected = !f.selected;
                                                                        }
                                                                    }
                                                                    cx.notify();
                                                                });
                                                            }
                                                        }),
                                                )
                                            })
                                            .child(
                                                Label::new(if is_expanded { "▼" } else { "▶" })
                                                    .size(ui::LabelSize::XSmall)
                                                    .color(ui::Color::Muted),
                                            )
                                            .child(
                                                div()
                                                    .px_1()
                                                    .rounded_sm()
                                                    .child(
                                                        Label::new(severity_label)
                                                            .size(ui::LabelSize::XSmall)
                                                            .color(severity_color),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .overflow_hidden()
                                                    .child(
                                                        Label::new(summary_line)
                                                            .size(ui::LabelSize::XSmall)
                                                            .color(ui::Color::Default)
                                                            .single_line(),
                                                    ),
                                            ),
                                    )
                                    .when(is_expanded, |el| {
                                        el.child(
                                            div()
                                                .id(gpui::ElementId::NamedInteger("wfm-finding-body".into(), i as u64))
                                                .ml_4()
                                                .px_2()
                                                .py_1()
                                                .rounded_md()
                                                .bg(cx.theme().colors().surface_background)
                                                .max_h(px(120.))
                                                .overflow_y_scroll()
                                                .child(
                                                    Label::new(finding_text.clone())
                                                        .size(ui::LabelSize::XSmall)
                                                        .color(ui::Color::Default),
                                                ),
                                        )
                                    })
                                    .into_any_element()
                            }))
                            // Submit bar
                            .when(self.busy_action.as_deref() == Some("submit_review"), |el| {
                                el.child(
                                    Label::new("Submitting...")
                                        .size(ui::LabelSize::XSmall)
                                        .color(ui::Color::Muted),
                                )
                            })
                            .when(has_selected && !submitted && self.busy_action.as_deref() != Some("submit_review"), |el| {
                                let repo = self.task.repo.clone().unwrap_or_default();
                                let pr_number = self.task.pr_number.unwrap_or(0);
                                let workforme_root = self.workforme_root.clone();
                                let entity_comment = entity.clone();
                                let entity_changes = entity.clone();
                                let entity_approve = entity.clone();
                                el.child(
                                    h_flex()
                                        .gap_2()
                                        .py_1()
                                        .child(
                                            div()
                                                .id("wfm-submit-comment")
                                                .px_2().py_0p5().rounded_md()
                                                .cursor_pointer()
                                                .bg(cx.theme().colors().ghost_element_hover)
                                                .hover(|s| s.bg(cx.theme().colors().ghost_element_selected))
                                                .child(Label::new("Comment").size(ui::LabelSize::XSmall).color(ui::Color::Warning))
                                                .on_click({
                                                    let repo = repo.clone();
                                                    let root = workforme_root.clone();
                                                    move |_event, _window, cx| {
                                                        submit_review(&entity_comment, &root, &repo, pr_number, "COMMENT", cx);
                                                    }
                                                }),
                                        )
                                        .child(
                                            div()
                                                .id("wfm-submit-changes")
                                                .px_2().py_0p5().rounded_md()
                                                .cursor_pointer()
                                                .bg(cx.theme().colors().ghost_element_hover)
                                                .hover(|s| s.bg(cx.theme().colors().ghost_element_selected))
                                                .child(Label::new("Request Changes").size(ui::LabelSize::XSmall).color(ui::Color::Error))
                                                .on_click({
                                                    let repo = repo.clone();
                                                    let root = workforme_root.clone();
                                                    move |_event, _window, cx| {
                                                        submit_review(&entity_changes, &root, &repo, pr_number, "REQUEST_CHANGES", cx);
                                                    }
                                                }),
                                        )
                                        .child(
                                            div()
                                                .id("wfm-submit-approve")
                                                .px_2().py_0p5().rounded_md()
                                                .cursor_pointer()
                                                .bg(cx.theme().colors().ghost_element_hover)
                                                .hover(|s| s.bg(cx.theme().colors().ghost_element_selected))
                                                .child(Label::new("Approve").size(ui::LabelSize::XSmall).color(ui::Color::Success))
                                                .on_click({
                                                    let repo = repo.clone();
                                                    let root = workforme_root.clone();
                                                    move |_event, _window, cx| {
                                                        submit_review(&entity_approve, &root, &repo, pr_number, "APPROVE", cx);
                                                    }
                                                }),
                                        ),
                                )
                            })
                            .when(submitted, |el| {
                                el.child(
                                    Label::new("Review submitted ✓")
                                        .size(ui::LabelSize::XSmall)
                                        .color(ui::Color::Success),
                                )
                            }),
                    )
            })
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
                    // Rebase button (for issue-dev tasks with worktree)
                    .when(
                        self.task.worktree.is_some(),
                        |el| {
                            let workforme_root = self.workforme_root.clone();
                            let task_id = self.task.id.clone();
                            el.child(
                                div()
                                    .id("wfm-action-rebase")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_md()
                                    .bg(cx.theme().colors().ghost_element_hover)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(cx.theme().colors().ghost_element_selected))
                                    .child(Label::new("↑ Rebase").size(ui::LabelSize::Small))
                                    .on_click(move |_event, _window, _cx| {
                                        let script = workforme_root.join("scripts/run-stage.sh");
                                        let task_dir = workforme_root.join(".work").join(&task_id);
                                        std::process::Command::new("bash")
                                            .arg(&script)
                                            .arg(&task_dir)
                                            .arg("rebase")
                                            .spawn()
                                            .ok();
                                    }),
                            )
                        },
                    )
                    .when(
                        self.task.dynamic_stages.iter().any(|s| s.key == "implement" && matches!(s.status, StageStatus::Done))
                            && !self.task.dynamic_stages.iter().any(|s| s.key == "pr" && matches!(s.status, StageStatus::Done)),
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
                    // Conflict notify button
                    .when(
                        self.task.pr_number.is_some()
                            && self.task.repo.is_some()
                            && self.task.inbox_item.as_ref().map(|i| i.mergeable) == Some(Some(false))
                            && self.busy_action.as_deref() != Some("conflict_notify"),
                        |el| {
                            let repo = self.task.repo.clone().unwrap();
                            let pr_number = self.task.pr_number.unwrap();
                            let pr_author = self.task.inbox_item.as_ref().and_then(|i| i.author.clone()).unwrap_or_default();
                            let workforme_root = self.workforme_root.clone();
                            let entity = cx.entity().downgrade();
                            el.child(
                                div()
                                    .id("wfm-action-conflict-notify")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_md()
                                    .bg(cx.theme().colors().ghost_element_hover)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(cx.theme().colors().ghost_element_selected))
                                    .child(Label::new("⚠ Conflict").size(ui::LabelSize::Small).color(ui::Color::Warning))
                                    .on_click(move |_event, _window, cx| {
                                        if let Some(view) = entity.upgrade() {
                                            view.update(cx, |view, cx| {
                                                view.busy_action = Some("conflict_notify".to_string());
                                                cx.notify();
                                            });
                                        }
                                        let workforme_root_clone = workforme_root.clone();
                                        let repo_clone = repo.clone();
                                        let author = pr_author.clone();
                                        std::thread::spawn(move || {
                                            let org = std::process::Command::new("python3")
                                                .arg(workforme_root_clone.join("scripts/state.py"))
                                                .arg("config")
                                                .arg("github_org")
                                                .output()
                                                .ok()
                                                .and_then(|o| String::from_utf8(o.stdout).ok())
                                                .unwrap_or_default()
                                                .trim()
                                                .to_string();
                                            let body = if author.is_empty() {
                                                "この PR に conflict が発生しています。マージ前に解消をお願いします。".to_string()
                                            } else {
                                                format!("@{} この PR に conflict が発生しています。マージ前に解消をお願いします。", author)
                                            };
                                            let result = std::process::Command::new("gh")
                                                .args([
                                                    "pr", "comment", &pr_number.to_string(),
                                                    "--repo", &format!("{}/{}", org, repo_clone),
                                                    "--body", &body,
                                                ])
                                                .output();
                                            if let Ok(output) = result {
                                                if !output.status.success() {
                                                    log::error!("workforme: conflict notify failed: {}", String::from_utf8_lossy(&output.stderr));
                                                }
                                            }
                                        });
                                    }),
                            )
                        },
                    )
                    .when(self.busy_action.as_deref() == Some("conflict_notify"), |el| {
                        el.child(
                            div()
                                .id("wfm-action-conflict-busy")
                                .px_2()
                                .py_0p5()
                                .rounded_md()
                                .bg(cx.theme().colors().ghost_element_selected)
                                .child(Label::new("Notifying...").size(ui::LabelSize::Small).color(ui::Color::Muted)),
                        )
                    })
                    // Approve button (for review tasks with pr_number)
                    .when(
                        self.task.task_type.as_deref() == Some("review")
                            && self.task.pr_number.is_some()
                            && self.task.repo.is_some()
                            && self.task.inbox_item.as_ref().map(|i| i.status.as_str()) != Some("approved")
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
                                        let workforme_root_clone = workforme_root.clone();
                                        let repo_clone = repo.clone();
                                        std::thread::spawn(move || {
                                            let org = std::process::Command::new("python3")
                                                .arg(workforme_root_clone.join("scripts/state.py"))
                                                .arg("config")
                                                .arg("github_org")
                                                .output()
                                                .ok()
                                                .and_then(|o| String::from_utf8(o.stdout).ok())
                                                .unwrap_or_default()
                                                .trim()
                                                .to_string();
                                            let result = std::process::Command::new("gh")
                                                .args(["api", &format!("repos/{}/{}/pulls/{}/reviews", org, repo_clone, pr_number),
                                                       "--method", "POST", "-f", "event=APPROVE", "-f", "body=LGTM"])
                                                .output();
                                            if let Ok(output) = result {
                                                if output.status.success() {
                                                    trigger_sync_inbox(&workforme_root_clone);
                                                } else {
                                                    log::error!("workforme: approve failed: {}", String::from_utf8_lossy(&output.stderr));
                                                }
                                            }
                                        });
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
                            && self.task.inbox_item.as_ref().map(|i| i.status.as_str()) == Some("approved")
                            && self.busy_action.as_deref() != Some("merge"),
                        |el| {
                            let repo = self.task.repo.clone().unwrap();
                            let pr_number = self.task.pr_number.unwrap();
                            let workforme_root = self.workforme_root.clone();
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
                                        let workforme_root_clone = workforme_root.clone();
                                        let repo_clone = repo.clone();
                                        std::thread::spawn(move || {
                                            let result = std::process::Command::new("gh")
                                                .args([
                                                    "pr", "merge",
                                                    &pr_number.to_string(),
                                                    "--repo", &format!("ajainc/{}", repo_clone),
                                                    "--squash",
                                                ])
                                                .output();
                                            if let Ok(output) = result {
                                                if output.status.success() {
                                                    trigger_sync_inbox(&workforme_root_clone);
                                                } else {
                                                    log::error!("workforme: merge failed: {}", String::from_utf8_lossy(&output.stderr));
                                                }
                                            }
                                        });
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
