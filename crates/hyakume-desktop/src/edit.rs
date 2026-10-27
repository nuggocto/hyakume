//! Changing work from the window: the task form, the review shown when
//! someone else saved first, the admin dialogs, and the question before a
//! delete. The window owns these and sends the writes; this module holds
//! the drafts and draws them.

use std::collections::BTreeSet;

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, FocusHandle, FontWeight, Hsla, SharedString,
    Window, div, prelude::*, px,
};
use hyakume_core::api::{CreateRelationType, CreateRole};
use hyakume_core::ids::{IdentityId, ProjectId, StageId, TaskId, WorkspaceId};
use hyakume_core::model::{Priority, Task, TaskFields};
use hyakume_core::permission::Permission;
use time::Date;
use time::macros::format_description;

use crate::app::button;
use crate::inspector::clean;
use crate::scene::{NodeKey, Scene};
use crate::sources::SourceKey;
use crate::text_input::TextInput;
use crate::theme::Theme;

/// Where a change goes: which source, workspace, and project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub source: SourceKey,
    pub workspace: WorkspaceId,
    pub project: ProjectId,
    /// The column, for people's names and the project's stages.
    pub column: NodeKey,
}

impl Place {
    /// The place of web `cluster`, if its column is a workspace.
    pub fn of_cluster(scene: &Scene, cluster: usize) -> Option<Self> {
        let c = &scene.clusters[cluster];
        let column = &scene.columns[c.column];
        Some(Self {
            source: column.source.clone(),
            workspace: column.workspace?,
            project: c.project,
            column: column.key.clone(),
        })
    }
}

/// An edit of an existing task, or a task being created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// `base` is the version the draft started from; saving checks it.
    Edit {
        task: TaskId,
        base: i64,
    },
    Create,
}

/// A draft in the side panel. Stage, assignee, and priority are picked
/// from the palette; the rest are typed.
pub struct Editor {
    pub place: Place,
    pub target: Target,
    /// The eye being edited, to keep the panel on it.
    pub key: Option<NodeKey>,
    /// "WEB-1", or "WEB" for a new task.
    pub label: String,
    pub title: Entity<TextInput>,
    pub labels: Entity<TextInput>,
    pub due: Entity<TextInput>,
    pub description: Entity<TextInput>,
    pub stage: StageId,
    pub assignee: Option<IdentityId>,
    pub priority: Option<Priority>,
    /// The request number while a save is on its way.
    pub saving: Option<u64>,
    pub problem: Option<String>,
}

impl Editor {
    /// A draft holding `fields`, such as a task's current ones, or a draft
    /// brought back from a conflict.
    pub fn new(
        place: Place,
        target: Target,
        key: Option<NodeKey>,
        label: String,
        fields: &TaskFields,
        cx: &mut App,
    ) -> Self {
        let input = |text: &str, placeholder: &'static str, cx: &mut App| {
            let text = text.to_owned();
            cx.new(|cx| {
                let mut input = TextInput::new(placeholder, cx);
                input.set_text(&text, cx);
                input
            })
        };
        let multiline = |text: String, placeholder: &'static str, rows: f32, cx: &mut App| {
            cx.new(|cx| {
                let mut input = TextInput::multiline(placeholder, cx).with_rows(rows);
                input.set_text(&text, cx);
                input
            })
        };
        Self {
            title: input(&fields.title, "A short title", cx),
            labels: multiline(labels_text(&fields.labels), "One label per line", 3.0, cx),
            due: input(
                &fields.due_date.map(|d| d.to_string()).unwrap_or_default(),
                "YYYY-MM-DD",
                cx,
            ),
            description: multiline(
                fields.description.clone(),
                "What needs doing, and why",
                6.0,
                cx,
            ),
            place,
            target,
            key,
            label,
            stage: fields.stage_id,
            assignee: fields.assignee,
            priority: fields.priority,
            saving: None,
            problem: None,
        }
    }

    /// The draft as fields to save, or what's wrong with it.
    pub fn fields(&self, cx: &App) -> Result<TaskFields, String> {
        let title = self.title.read(cx).text().trim().to_owned();
        if title.is_empty() {
            return Err("A task needs a title.".into());
        }
        Ok(TaskFields {
            title,
            description: self.description.read(cx).text().to_owned(),
            stage_id: self.stage,
            assignee: self.assignee,
            labels: parse_labels(self.labels.read(cx).text()),
            priority: self.priority,
            due_date: parse_due(self.due.read(cx).text())?,
        })
    }

    pub fn inputs(&self) -> [&Entity<TextInput>; 4] {
        [&self.title, &self.labels, &self.due, &self.description]
    }
}

/// Labels as the form shows them: one per line. A label may hold a comma
/// but never a newline (the server refuses it), so this always splits back
/// into the same labels.
pub fn labels_text(labels: &[String]) -> String {
    labels.join("\n")
}

/// One label per line, blanks and repeats dropped, order kept.
pub fn parse_labels(text: &str) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    for label in text.lines().map(str::trim) {
        if !label.is_empty() && !labels.iter().any(|l| l == label) {
            labels.push(label.to_owned());
        }
    }
    labels
}

/// An empty field clears the date; anything else must be YYYY-MM-DD.
pub fn parse_due(text: &str) -> Result<Option<Date>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    Date::parse(text, format_description!("[year]-[month]-[day]"))
        .map(Some)
        .map_err(|_| format!("{text:?} isn't a date like 2026-11-14."))
}

/// A save turned down because the task changed since the draft began: the
/// draft and the task as it is now, side by side.
pub struct Conflict {
    pub place: Place,
    pub key: Option<NodeKey>,
    pub label: String,
    /// The draft as the review shows it, typing after Save included.
    pub draft: TaskFields,
    pub latest: Task,
    pub focus: FocusHandle,
    /// The form the save came from, kept whole for carrying on with it.
    /// None for a quick change, which had no form.
    pub editor: Option<Editor>,
}

/// The parts of a task the review compares, in the order it lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Title,
    Stage,
    Assignee,
    Priority,
    Labels,
    Due,
    Description,
}

impl Field {
    pub const ALL: [Field; 7] = [
        Self::Title,
        Self::Stage,
        Self::Assignee,
        Self::Priority,
        Self::Labels,
        Self::Due,
        Self::Description,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::Stage => "Stage",
            Self::Assignee => "Assignee",
            Self::Priority => "Priority",
            Self::Labels => "Labels",
            Self::Due => "Due",
            Self::Description => "Description",
        }
    }

    /// How a field reads, with stage and people named from `scene`.
    fn show(self, f: &TaskFields, scene: &Scene, place: &Place) -> String {
        match self {
            Self::Title => f.title.clone(),
            Self::Stage => stage_name(scene, place, f.stage_id),
            Self::Assignee => person(scene, place, f.assignee),
            Self::Priority => priority_name(f.priority).to_owned(),
            Self::Labels if f.labels.is_empty() => "None".into(),
            Self::Labels => labels_text(&f.labels),
            Self::Due => f.due_date.map_or("None".into(), |d| d.to_string()),
            Self::Description if f.description.trim().is_empty() => "None".into(),
            Self::Description => f.description.clone(),
        }
    }

    fn differs(self, a: &TaskFields, b: &TaskFields) -> bool {
        match self {
            Self::Title => a.title != b.title,
            Self::Stage => a.stage_id != b.stage_id,
            Self::Assignee => a.assignee != b.assignee,
            Self::Priority => a.priority != b.priority,
            Self::Labels => a.labels != b.labels,
            Self::Due => a.due_date != b.due_date,
            Self::Description => a.description != b.description,
        }
    }
}

pub fn stage_name(scene: &Scene, place: &Place, stage: StageId) -> String {
    scene
        .clusters
        .iter()
        .find(|c| c.project == place.project && scene.columns[c.column].key == place.column)
        .and_then(|c| c.stages.iter().find(|s| s.id == stage))
        .map_or_else(|| "Unknown stage".into(), |s| s.name.clone())
}

pub fn person(scene: &Scene, place: &Place, who: Option<IdentityId>) -> String {
    let Some(who) = who else {
        return "Unassigned".into();
    };
    scene
        .columns
        .iter()
        .find(|c| c.key == place.column)
        .and_then(|c| c.people.get(&who).cloned())
        .unwrap_or_else(|| "Someone".into())
}

pub fn priority_name(priority: Option<Priority>) -> &'static str {
    match priority {
        None => "None",
        Some(Priority::Low) => "Low",
        Some(Priority::Medium) => "Medium",
        Some(Priority::High) => "High",
        Some(Priority::Urgent) => "Urgent",
    }
}

/// A dialog over the canvas for setting up a workspace.
pub enum Dialog {
    RelationType {
        place: DialogPlace,
        name: Entity<TextInput>,
        directed: bool,
        reverse: Entity<TextInput>,
        saving: Option<u64>,
        problem: Option<String>,
        focus: FocusHandle,
    },
    Role {
        place: DialogPlace,
        name: Entity<TextInput>,
        permissions: BTreeSet<Permission>,
        saving: Option<u64>,
        problem: Option<String>,
        focus: FocusHandle,
    },
    /// Asks first: a deleted task can't come back.
    DeleteTask {
        place: DialogPlace,
        key: NodeKey,
        task: TaskId,
        /// "WEB-3 Fix the login", as the question names it.
        what: String,
        saving: Option<u64>,
        problem: Option<String>,
        focus: FocusHandle,
    },
}

/// A dialog's workspace. Types and roles span every project in it.
#[derive(Clone, Debug)]
pub struct DialogPlace {
    pub source: SourceKey,
    pub workspace: WorkspaceId,
    /// "a / Acme", for the title.
    pub name: String,
}

impl Dialog {
    pub fn relation_type(place: DialogPlace, cx: &mut App) -> Self {
        Self::RelationType {
            place,
            name: cx.new(|cx| TextInput::new("Such as \"depends on\"", cx)),
            directed: true,
            reverse: cx.new(|cx| TextInput::new("Read from the other end, \"needed by\"", cx)),
            saving: None,
            problem: None,
            focus: cx.focus_handle(),
        }
    }

    pub fn role(place: DialogPlace, cx: &mut App) -> Self {
        Self::Role {
            place,
            name: cx.new(|cx| TextInput::new("Such as \"Reviewer\"", cx)),
            permissions: [Permission::View].into(),
            saving: None,
            problem: None,
            focus: cx.focus_handle(),
        }
    }

    pub fn delete_task(place: DialogPlace, key: NodeKey, task: &Task, cx: &mut App) -> Self {
        Self::DeleteTask {
            place,
            key,
            task: task.id,
            what: format!("{} {}", task.reference, task.fields.title),
            saving: None,
            problem: None,
            focus: cx.focus_handle(),
        }
    }

    pub fn place(&self) -> &DialogPlace {
        match self {
            Self::RelationType { place, .. }
            | Self::Role { place, .. }
            | Self::DeleteTask { place, .. } => place,
        }
    }

    pub fn inputs(&self) -> Vec<&Entity<TextInput>> {
        match self {
            Self::RelationType { name, reverse, .. } => vec![name, reverse],
            Self::Role { name, .. } => vec![name],
            Self::DeleteTask { .. } => Vec::new(),
        }
    }

    /// Where focus starts: the first field, or the dialog itself when it
    /// only asks a question.
    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        match self.inputs().first() {
            Some(input) => input.read(cx).focus_handle().clone(),
            None => self.focus().clone(),
        }
    }

    pub fn saving(&mut self) -> &mut Option<u64> {
        match self {
            Self::RelationType { saving, .. }
            | Self::Role { saving, .. }
            | Self::DeleteTask { saving, .. } => saving,
        }
    }

    /// The request number of the save on its way, if one is.
    pub fn waiting_on(&self) -> Option<u64> {
        match self {
            Self::RelationType { saving, .. }
            | Self::Role { saving, .. }
            | Self::DeleteTask { saving, .. } => *saving,
        }
    }

    pub fn problem(&mut self) -> &mut Option<String> {
        match self {
            Self::RelationType { problem, .. }
            | Self::Role { problem, .. }
            | Self::DeleteTask { problem, .. } => problem,
        }
    }

    pub fn focus(&self) -> &FocusHandle {
        match self {
            Self::RelationType { focus, .. }
            | Self::Role { focus, .. }
            | Self::DeleteTask { focus, .. } => focus,
        }
    }
}

pub enum DialogBody {
    RelationType(CreateRelationType),
    Role(CreateRole),
    DeleteTask { key: NodeKey, task: TaskId },
}

impl Dialog {
    /// What the dialog would create, or what's missing.
    pub fn body(&self, cx: &App) -> Result<DialogBody, String> {
        match self {
            Self::RelationType {
                name,
                directed,
                reverse,
                ..
            } => {
                let name = name.read(cx).text().trim().to_owned();
                let reverse = reverse.read(cx).text().trim().to_owned();
                if name.is_empty() {
                    return Err("The type needs a name.".into());
                }
                if *directed && reverse.is_empty() {
                    return Err("A directed type needs a name for the other end too.".into());
                }
                Ok(DialogBody::RelationType(CreateRelationType {
                    name,
                    directed: *directed,
                    reverse_name: directed.then_some(reverse),
                }))
            }
            Self::Role {
                name, permissions, ..
            } => {
                let name = name.read(cx).text().trim().to_owned();
                if name.is_empty() {
                    return Err("The role needs a name.".into());
                }
                Ok(DialogBody::Role(CreateRole {
                    name,
                    permissions: permissions.iter().copied().collect(),
                }))
            }
            Self::DeleteTask { key, task, .. } => Ok(DialogBody::DeleteTask {
                key: key.clone(),
                task: *task,
            }),
        }
    }
}

pub fn permission_name(p: Permission) -> &'static str {
    match p {
        Permission::View => "View",
        Permission::EditTasks => "Create and edit tasks",
        Permission::DeleteTasks => "Delete tasks",
        Permission::Comment => "Comment",
        Permission::ManageRelations => "Link and unlink tasks",
        Permission::ConfigureProjects => "Set up projects and link types",
        Permission::ManageInvitations => "Invite people",
        Permission::ManageRoles => "Manage roles",
    }
}

/// What a click in a form asks the window to do.
#[derive(Clone, Copy, Debug)]
pub enum FormEvent {
    Save,
    Cancel,
    PickStage,
    PickAssignee,
    PickPriority,
    /// From the review: carry on editing the draft against the latest.
    Revise,
    Discard,
    ToggleDirected,
    TogglePermission(Permission),
}

pub type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
pub type Listen<'a> = &'a dyn Fn(FormEvent) -> OnClick;

/// What drawing a form needs from the window around it.
pub struct Ui<'a> {
    pub t: &'a Theme,
    /// The focus ring color, while the keyboard is in use.
    pub ring: Option<Hsla>,
    pub listen: Listen<'a>,
    pub window: &'a Window,
    pub cx: &'a App,
}

/// A bordered box around a text field, so it reads as something to type in.
fn field_box(
    input: &Entity<TextInput>,
    t: &Theme,
    ring: Option<Hsla>,
    window: &Window,
    cx: &App,
) -> AnyElement {
    let focused = input.read(cx).focus_handle().is_focused(window);
    div()
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(match (focused, ring) {
            (true, Some(ring)) => ring,
            (true, None) => t.accent.opacity(0.6),
            _ => t.line,
        })
        .bg(t.bg)
        .text_size(px(13.0))
        .line_height(px(19.0))
        .child(input.clone())
        .into_any_element()
}

fn row(name: &'static str, value: AnyElement, t: &Theme) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(div().text_size(px(12.0)).text_color(t.muted).child(name))
        .child(value)
        .into_any_element()
}

/// A button showing a picked value, which opens the palette to change it.
fn choice(
    id: &'static str,
    value: String,
    event: FormEvent,
    hint: Option<String>,
    t: &Theme,
    ring: Option<Hsla>,
    listen: Listen,
) -> AnyElement {
    button(t, id, ring)
        .justify_between()
        .gap(px(8.0))
        .border_1()
        .border_color(t.line)
        .rounded(px(8.0))
        .on_click(listen(event))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(t.ink)
                .child(clean(&value, false)),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_color(t.muted)
                .child(hint.map_or("Change".to_owned(), |h| format!("Change  {h}"))),
        )
        .into_any_element()
}

pub struct Hints {
    pub save: Option<String>,
    pub cancel: Option<String>,
    pub stage: Option<String>,
    pub assign: Option<String>,
}

/// The task form, shown in the side panel in place of the task's fields.
pub fn editor_form(editor: &Editor, scene: &Scene, hints: Hints, ui: Ui) -> AnyElement {
    let Ui {
        t,
        ring,
        listen,
        window,
        cx,
    } = ui;
    let place = &editor.place;
    let mut form = div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(row(
            "Title",
            field_box(&editor.title, t, ring, window, cx),
            t,
        ))
        .child(row(
            "Stage",
            choice(
                "edit-stage",
                stage_name(scene, place, editor.stage),
                FormEvent::PickStage,
                hints.stage,
                t,
                ring,
                listen,
            ),
            t,
        ))
        .child(row(
            "Assignee",
            choice(
                "edit-assignee",
                person(scene, place, editor.assignee),
                FormEvent::PickAssignee,
                hints.assign,
                t,
                ring,
                listen,
            ),
            t,
        ))
        .child(row(
            "Priority",
            choice(
                "edit-priority",
                priority_name(editor.priority).to_owned(),
                FormEvent::PickPriority,
                None,
                t,
                ring,
                listen,
            ),
            t,
        ))
        .child(row(
            "Labels",
            field_box(&editor.labels, t, ring, window, cx),
            t,
        ))
        .child(row("Due", field_box(&editor.due, t, ring, window, cx), t))
        .child(row(
            "Description",
            field_box(&editor.description, t, ring, window, cx),
            t,
        ));
    if let Some(problem) = &editor.problem {
        form = form.child(
            div()
                .px(px(12.0))
                .py(px(8.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(t.blocked)
                .bg(t.blocked_bg)
                .text_size(px(13.0))
                .child(clean(problem, false)),
        );
    }
    let saving = editor.saving.is_some();
    let save = match (editor.target, saving) {
        (_, true) => "Saving…",
        (Target::Edit { .. }, false) => "Save",
        (Target::Create, false) => "Create task",
    };
    let version = match editor.target {
        Target::Edit { base, .. } => format!("Editing version {base}"),
        Target::Create => String::new(),
    };
    form.child(
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                action(t, "edit-save", save, hints.save, true, ring)
                    .on_click(listen(FormEvent::Save)),
            )
            .child(
                action(t, "edit-cancel", "Cancel", hints.cancel, false, ring)
                    .on_click(listen(FormEvent::Cancel)),
            )
            .child(div().flex_1())
            .child(div().text_size(px(11.0)).text_color(t.muted).child(version)),
    )
    .into_any_element()
}

/// A form's main or secondary button, with its shortcut beside the label.
fn action(
    t: &Theme,
    id: &'static str,
    label: &'static str,
    hint: Option<String>,
    main: bool,
    ring: Option<Hsla>,
) -> gpui::Stateful<gpui::Div> {
    button(t, id, ring)
        .flex_shrink_0()
        .gap(px(8.0))
        .border_1()
        .border_color(if main { t.accent } else { t.line })
        .child(label)
        .children(hint.map(|h| div().text_color(t.muted).child(h)))
}

/// The review after a refused save: the task as it is now beside the
/// draft, changed fields marked, stacked when there's no room for two.
pub fn conflict_view(
    conflict: &Conflict,
    scene: &Scene,
    width: f32,
    hints: (Option<String>, Option<String>),
    t: &Theme,
    ring: Option<Hsla>,
    listen: Listen,
) -> AnyElement {
    let side_by_side = width >= 640.0;
    let place = &conflict.place;
    let latest = &conflict.latest.fields;
    let draft = &conflict.draft;
    let changed = |f: Field| f.differs(latest, draft);
    let panel = |heading: String, fields: &TaskFields| {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.muted)
                    .child(heading),
            )
            .children(Field::ALL.into_iter().map(|f| {
                let lit = changed(f);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px(px(10.0))
                    .py(px(6.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(if lit {
                        t.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .when(lit, |row| row.bg(t.raised))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.0))
                            .text_size(px(11.0))
                            .text_color(t.muted)
                            .child(f.name())
                            .when(lit, |name| {
                                name.child(div().text_color(t.accent).child("· differs"))
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .line_height(px(19.0))
                            .child(clean(&f.show(fields, scene, place), true)),
                    )
            }))
    };
    let (revise, discard) = hints;
    div()
        .w(px(width))
        .max_h(px(620.0))
        .flex()
        .flex_col()
        .gap(px(14.0))
        .p(px(20.0))
        .rounded(px(14.0))
        .border_1()
        .border_color(t.line)
        .bg(t.surface)
        .shadow_lg()
        .child(
            div()
                .text_size(px(16.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(clean(
                    &format!("{} changed while you were editing it", conflict.label),
                    false,
                )),
        )
        .child(div().text_size(px(13.0)).text_color(t.muted).child(
            "Nothing was overwritten. Your draft is kept: carry on from the latest \
             version, or let your draft go.",
        ))
        .child(
            div()
                .id("conflict-panels")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .when(side_by_side, |d| d.flex_row().gap(px(16.0)))
                .when(!side_by_side, |d| d.flex_col().gap(px(14.0)))
                .child(panel(
                    format!("Latest, version {}", conflict.latest.version),
                    latest,
                ))
                .child(panel("Your draft".into(), draft)),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .child(
                    action(
                        t,
                        "conflict-revise",
                        "Keep editing my draft",
                        revise,
                        true,
                        ring,
                    )
                    .on_click(listen(FormEvent::Revise)),
                )
                .child(
                    action(
                        t,
                        "conflict-discard",
                        "Discard my draft",
                        discard,
                        false,
                        ring,
                    )
                    .on_click(listen(FormEvent::Discard)),
                ),
        )
        .into_any_element()
}

/// A checkbox: a button that says whether it's on.
fn check(
    id: SharedString,
    label: &'static str,
    on: bool,
    event: FormEvent,
    t: &Theme,
    ring: Option<Hsla>,
    listen: Listen,
) -> AnyElement {
    button(t, id, ring)
        .justify_start()
        .gap(px(10.0))
        .on_click(listen(event))
        .child(
            div()
                .w(px(16.0))
                .h(px(16.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .border_1()
                .border_color(if on { t.accent } else { t.muted })
                .when(on, |b| b.bg(t.accent).text_color(t.bg).child("✓")),
        )
        .child(div().text_color(t.ink).child(label))
        .into_any_element()
}

pub fn dialog_view(
    dialog: &Dialog,
    width: f32,
    hints: (Option<String>, Option<String>),
    ui: Ui,
) -> AnyElement {
    let Ui {
        t,
        ring,
        listen,
        window,
        cx,
    } = ui;
    let (title, mut body) = match dialog {
        Dialog::RelationType {
            name,
            directed,
            reverse,
            ..
        } => {
            let mut body = div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(row("Name", field_box(name, t, ring, window, cx), t))
                .child(check(
                    "dialog-directed".into(),
                    "Directed: it reads differently from each end",
                    *directed,
                    FormEvent::ToggleDirected,
                    t,
                    ring,
                    listen,
                ));
            if *directed {
                body = body.child(row(
                    "Name from the other end",
                    field_box(reverse, t, ring, window, cx),
                    t,
                ));
            }
            ("New relationship type", body)
        }
        Dialog::Role {
            name, permissions, ..
        } => {
            let body = div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(row("Name", field_box(name, t, ring, window, cx), t))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(t.muted)
                        .child("What people with this role may do"),
                )
                .child(div().flex().flex_col().gap(px(2.0)).children(
                    Permission::ALL.into_iter().map(|p| {
                        check(
                            format!("dialog-permission-{}", p.as_str()).into(),
                            permission_name(p),
                            permissions.contains(&p),
                            FormEvent::TogglePermission(p),
                            t,
                            ring,
                            listen,
                        )
                    }),
                ));
            ("New role", body)
        }
        Dialog::DeleteTask { what, .. } => {
            let body = div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(div().text_color(t.ink).child(clean(what, false)))
                .child(div().text_size(px(13.0)).text_color(t.muted).child(
                    "It goes for everyone who can see it, along with its links \
                     and comments. There's no undo.",
                ));
            ("Delete this task?", body)
        }
    };
    let problem = match dialog {
        Dialog::RelationType { problem, .. }
        | Dialog::Role { problem, .. }
        | Dialog::DeleteTask { problem, .. } => problem.clone(),
    };
    if let Some(problem) = problem {
        body = body.child(
            div()
                .px(px(12.0))
                .py(px(8.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(t.blocked)
                .bg(t.blocked_bg)
                .text_size(px(13.0))
                .child(clean(&problem, false)),
        );
    }
    let saving = dialog.waiting_on().is_some();
    let (yes, no) = match (dialog, saving) {
        (Dialog::DeleteTask { .. }, false) => ("Delete", "Keep it"),
        (Dialog::DeleteTask { .. }, true) => ("Deleting…", "Keep it"),
        (_, false) => ("Create", "Cancel"),
        (_, true) => ("Creating…", "Cancel"),
    };
    let (save_hint, cancel_hint) = hints;
    div()
        .w(px(width))
        .flex()
        .flex_col()
        .gap(px(14.0))
        .p(px(20.0))
        .rounded(px(14.0))
        .border_1()
        .border_color(t.line)
        .bg(t.surface)
        .shadow_lg()
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(16.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(t.muted)
                        .child(clean(&format!("in {}", dialog.place().name), false)),
                ),
        )
        .child(body)
        .child(
            div()
                .flex()
                .gap(px(8.0))
                .child(
                    action(t, "dialog-save", yes, save_hint, true, ring)
                        .on_click(listen(FormEvent::Save)),
                )
                .child(
                    action(t, "dialog-cancel", no, cancel_hint, false, ring)
                        .on_click(listen(FormEvent::Cancel)),
                ),
        )
        .into_any_element()
}

/// Keeps the window's listener factory type in one place.
pub fn listener<'a, T: 'static>(
    cx: &'a Context<'a, T>,
    on: impl Fn(&mut T, FormEvent, &ClickEvent, &mut Window, &mut Context<T>) + Clone + 'static,
) -> impl Fn(FormEvent) -> OnClick + 'a {
    move |event| {
        let on = on.clone();
        Box::new(cx.listener(move |this, click: &ClickEvent, window, cx| {
            on(this, event, click, window, cx)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_come_back_from_the_form_as_they_went_in() {
        // The server takes commas in a label, so only a newline can separate them.
        let labels = vec!["design, research".to_owned(), "ux".to_owned()];
        assert_eq!(parse_labels(&labels_text(&labels)), labels);
        // Typed ones: blanks and repeats go, order stays.
        assert_eq!(
            parse_labels(" launch\n\nmarketing \nlaunch\n"),
            ["launch", "marketing"]
        );
        assert!(parse_labels("  ").is_empty());
    }

    #[test]
    fn a_due_date_is_a_calendar_date_or_nothing() {
        use time::macros::date;
        assert_eq!(parse_due(" 2026-11-14 "), Ok(Some(date!(2026 - 11 - 14))));
        assert_eq!(parse_due(""), Ok(None));
        assert!(parse_due("14/11/2026").is_err());
        assert!(parse_due("2026-02-30").is_err(), "no such day");
    }
}
