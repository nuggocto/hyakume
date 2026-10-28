//! Everything the window changes: task edits and quick moves, new and
//! deleted tasks, comments, links, positions, roles, and the admin dialogs.
//! Each write goes to the I/O thread and comes back as `Update::Wrote`,
//! matched by request number.

use std::time::Duration;

use gpui::{
    AnyElement, ClickEvent, Context, Entity, FocusHandle, Hsla, MouseButton, MouseDownEvent,
    Window, div, prelude::*, px,
};
use hyakume_core::api::{CreateRelation, CreateTask, LayoutNode, UpdateTask};
use hyakume_core::model::{Task, TaskFields};
use hyakume_core::permission::Permission;

use super::{HEADER_H, Hyakume, button};
use crate::edit::{
    self, Conflict, Dialog, DialogBody, DialogPlace, Editor, FormEvent, Hints, Place, Target,
};
use crate::keys::{self, *};
use crate::layout::Vec2;
use crate::picker::{Mode, Pick};
use crate::runtime::{Command, Write, WriteError, Wrote};
use crate::scene::NodeKey;
use crate::sources::{SourceKey, Status};
use crate::text_input::TextInput;

/// What a write in flight was for, so its answer lands in the right place.
pub(super) enum Pending {
    /// A save from the form, or a quick change of stage, assignee, or
    /// priority. The draft is kept for the review if someone saved first.
    Save {
        place: Place,
        key: Option<NodeKey>,
        label: String,
        draft: Box<TaskFields>,
    },
    /// What was sent, to tell whether the form changed while it was saving.
    Create(Box<TaskFields>),
    /// The task, and the text sent, so only that text is cleared.
    Comment(NodeKey, String),
    /// What the dialog creates ("role"), for the notice once it's done.
    Dialog(&'static str),
    /// The task, and its reference for the notice.
    Delete(NodeKey, String),
    /// Anything else: on success, this notice.
    Done(String),
}

/// How far Alt+arrows nudge an eye or web, in world units.
const NUDGE: f32 = 16.0;

impl Hyakume {
    fn send_write(
        &mut self,
        source: SourceKey,
        workspace: hyakume_core::ids::WorkspaceId,
        write: Write,
        pending: Pending,
    ) -> u64 {
        self.write_request += 1;
        let request = self.write_request;
        self.pending.insert(request, pending);
        self.runtime.send(Command::Write {
            source,
            workspace,
            request,
            write,
        });
        request
    }

    /// Why `permission` can't be used on web `cluster` from here, if it can't.
    /// Offline comes first: it says the change will work once back.
    fn refusal(&self, cluster: usize, permission: Permission) -> Option<String> {
        let c = &self.scene.clusters[cluster];
        let column = &self.scene.columns[c.column];
        if !matches!(column.status, Status::Live) {
            return Some(format!(
                "{} isn't connected, so its work is read-only for now",
                column.chip
            ));
        }
        (!column.allows(permission, Some(c.project))).then(|| {
            let what = match permission {
                Permission::EditTasks => "change tasks",
                Permission::DeleteTasks => "delete tasks",
                Permission::Comment => "comment",
                Permission::ManageRelations => "link tasks",
                Permission::ConfigureProjects => "add relationship types",
                Permission::ManageRoles => "manage roles",
                _ => "do that",
            };
            format!("Your role can't {what} in {}", c.project_key)
        })
    }

    /// Why the inspector's task can't be edited, as a chip: offline, or
    /// a role that only views.
    pub(super) fn read_only(&self, eye: usize) -> Option<&'static str> {
        let cluster = self.scene.eyes[eye].cluster;
        let column = &self.scene.columns[self.scene.clusters[cluster].column];
        if !matches!(column.status, Status::Live) {
            Some("Offline, read-only")
        } else if self.refusal(cluster, Permission::EditTasks).is_some() {
            Some("Read-only")
        } else {
            None
        }
    }

    fn selected_eye(&self) -> Option<usize> {
        self.selected.as_ref().and_then(|k| self.scene.eye(k))
    }

    /// The web an action without a task applies to: the open project, the
    /// selected task's, or the one the arrows are on.
    fn context_cluster(&self) -> Option<usize> {
        self.focus
            .as_ref()
            .and_then(|k| self.scene.cluster(k))
            .or(self.selected_eye().map(|e| self.scene.eyes[e].cluster))
            .or(self.web_cursor.as_ref().and_then(|k| self.scene.cluster(k)))
    }

    pub(super) fn start_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(eye) = self.selected_eye() else {
            self.note("Select a task first, then E edits it");
            return;
        };
        let cluster = self.scene.eyes[eye].cluster;
        if let Some(why) = self.refusal(cluster, Permission::EditTasks) {
            self.note(why);
            return;
        }
        let Some(place) = Place::of_cluster(&self.scene, cluster) else {
            return;
        };
        let e = &self.scene.eyes[eye];
        // Coming back to a draft left open keeps it.
        if self
            .editor
            .as_ref()
            .is_none_or(|ed| ed.key.as_ref() != Some(&e.key))
        {
            self.editor = Some(Editor::new(
                place,
                Target::Edit {
                    task: e.task.id,
                    base: e.task.version,
                },
                Some(e.key.clone()),
                e.task.reference.clone(),
                &e.task.fields,
                cx,
            ));
        }
        if !self.inspecting {
            self.inspecting = true;
            self.load_detail();
        }
        self.focus_title(window, cx);
    }

    /// The form's title takes focus at the top of the panel. The panel may
    /// still be scrolled down to the comment box, and typing into a title
    /// nobody can see would change it unnoticed.
    fn focus_title(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = &self.editor {
            window.focus(editor.title.read(cx).focus_handle());
            self.details_scroll
                .set_offset(gpui::point(px(0.0), px(0.0)));
        }
    }

    pub(super) fn start_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cluster) = self.context_cluster() else {
            self.note("Open a project first: a new task goes in the project you're in");
            return;
        };
        if let Some(why) = self.refusal(cluster, Permission::EditTasks) {
            self.note(why);
            return;
        }
        let Some(place) = Place::of_cluster(&self.scene, cluster) else {
            return;
        };
        let c = &self.scene.clusters[cluster];
        let Some(first) = c.stages.first() else {
            self.note(format!("{} has no stages to put a task in", c.project_key));
            return;
        };
        let fields = TaskFields {
            title: String::new(),
            description: String::new(),
            stage_id: first.id,
            assignee: None,
            labels: Vec::new(),
            priority: None,
            due_date: None,
        };
        let editor = Editor::new(
            place,
            Target::Create,
            None,
            c.project_key.clone(),
            &fields,
            cx,
        );
        window.focus(editor.title.read(cx).focus_handle());
        self.editor = Some(editor);
    }

    pub(super) fn save_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else { return };
        if editor.saving.is_some() {
            return;
        }
        let fields = editor.fields(cx);
        let cluster = self.scene.clusters.iter().position(|c| {
            c.project == editor.place.project
                && self.scene.columns[c.column].key == editor.place.column
        });
        let refused = match cluster {
            Some(c) => self.refusal(c, Permission::EditTasks),
            None => Some("That project isn't loaded any more".into()),
        };
        let Some(editor) = &mut self.editor else {
            return;
        };
        let fields = match (fields, refused) {
            (Err(problem), _) | (_, Some(problem)) => {
                // The draft stays as it is, ready for another try.
                editor.problem = Some(problem);
                return;
            }
            (Ok(fields), None) => fields,
        };
        editor.problem = None;
        let place = editor.place.clone();
        let (write, pending) = match editor.target {
            Target::Edit { task, base } => (
                Write::SaveTask {
                    task,
                    update: UpdateTask {
                        expected_version: base,
                        fields: fields.clone(),
                    },
                },
                Pending::Save {
                    place: place.clone(),
                    key: editor.key.clone(),
                    label: editor.label.clone(),
                    draft: Box::new(fields),
                },
            ),
            Target::Create => (
                Write::CreateTask {
                    project: place.project,
                    body: CreateTask {
                        title: fields.title.clone(),
                        description: fields.description.clone(),
                        stage_id: Some(fields.stage_id),
                        assignee: fields.assignee,
                        labels: fields.labels.clone(),
                        priority: fields.priority,
                        due_date: fields.due_date,
                    },
                },
                Pending::Create(Box::new(fields)),
            ),
        };
        let request = self.send_write(place.source, place.workspace, write, pending);
        if let Some(editor) = &mut self.editor {
            editor.saving = Some(request);
        }
    }

    /// The form was typed into while its save was on the way: the save
    /// went through, and the form stays open on top of it with the rest.
    fn keep_typing(&mut self, saved: &Task, key: Option<NodeKey>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        editor.saving = None;
        editor.target = Target::Edit {
            task: saved.id,
            base: saved.version,
        };
        if let Some(key) = key {
            editor.key = Some(key);
            editor.label = saved.reference.clone();
        }
    }

    pub(super) fn cancel_editor(&mut self, window: &mut Window) {
        self.editor = None;
        window.focus(&self.focus_handle);
    }

    /// Whether the form is the one on screen: a new task's, or an edit of
    /// the task the panel shows. A draft left on another task waits there,
    /// and changes picked now aren't meant for it.
    fn editor_in_view(&self) -> bool {
        self.editor.as_ref().is_some_and(|e| match e.target {
            Target::Create => true,
            Target::Edit { .. } => self.inspecting && e.key.is_some() && e.key == self.selected,
        })
    }

    /// Stage, assignee, and priority: into the draft while its form is on
    /// screen, else saved straight away against the version on screen.
    fn quick_change(
        &mut self,
        change: impl FnOnce(&mut TaskFields),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor_in_view()
            && let Some(editor) = &mut self.editor
        {
            let mut fields = TaskFields {
                title: String::new(),
                description: String::new(),
                stage_id: editor.stage,
                assignee: editor.assignee,
                labels: Vec::new(),
                priority: editor.priority,
                due_date: None,
            };
            change(&mut fields);
            editor.stage = fields.stage_id;
            editor.assignee = fields.assignee;
            editor.priority = fields.priority;
            // Not the button that opened the palette: Enter's key-up would
            // click it and open the palette again.
            self.focus_title(window, cx);
            return;
        }
        let Some(eye) = self.selected_eye() else {
            return;
        };
        let cluster = self.scene.eyes[eye].cluster;
        if let Some(why) = self.refusal(cluster, Permission::EditTasks) {
            self.note(why);
            return;
        }
        let Some(place) = Place::of_cluster(&self.scene, cluster) else {
            return;
        };
        let e = &self.scene.eyes[eye];
        // Its version on screen is the one the save in flight replaces, so a
        // second change now would only conflict with the first.
        let saving = |p: &Pending| matches!(p, Pending::Save { key: Some(k), .. } if *k == e.key);
        if self.pending.values().any(saving) {
            self.note(format!("Still saving {}", e.task.reference));
            return;
        }
        let mut draft = e.task.fields.clone();
        change(&mut draft);
        if draft == e.task.fields {
            return;
        }
        let write = Write::SaveTask {
            task: e.task.id,
            update: UpdateTask {
                expected_version: e.task.version,
                fields: draft.clone(),
            },
        };
        let pending = Pending::Save {
            place: place.clone(),
            key: Some(e.key.clone()),
            label: e.task.reference.clone(),
            draft: Box::new(draft),
        };
        self.send_write(place.source, place.workspace, write, pending);
    }

    /// Opens a palette for changing the selected task (or the draft), after
    /// checking it can be changed at all.
    pub(super) fn open_change(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editor_in_view() {
            let Some(eye) = self.selected_eye() else {
                self.note("Select a task first");
                return;
            };
            let permission = match mode {
                Mode::LinkType | Mode::Unlink => Permission::ManageRelations,
                _ => Permission::EditTasks,
            };
            if let Some(why) = self.refusal(self.scene.eyes[eye].cluster, permission) {
                self.note(why);
                return;
            }
        } else if matches!(mode, Mode::LinkType | Mode::Unlink) {
            self.note("Save or cancel the edit first, then link");
            return;
        }
        let restore = window.focused(cx);
        self.open_picker(mode, restore, window, cx);
    }

    /// The palette's rows for the modes that change a task.
    pub(super) fn change_items(&self, mode: Mode) -> Vec<crate::picker::Item> {
        use crate::picker;
        // The draft's project when its form is on screen, else the selected task's.
        let editor = self.editor.as_ref().filter(|_| self.editor_in_view());
        let (cluster, eye) = match editor {
            Some(editor) => (
                self.scene.clusters.iter().position(|c| {
                    c.project == editor.place.project
                        && self.scene.columns[c.column].key == editor.place.column
                }),
                editor.key.as_ref().and_then(|k| self.scene.eye(k)),
            ),
            None => {
                let eye = self.selected_eye();
                (eye.map(|e| self.scene.eyes[e].cluster), eye)
            }
        };
        let Some(cluster) = cluster else {
            return Vec::new();
        };
        let column = self.scene.clusters[cluster].column;
        let current = eye.map(|e| self.scene.eyes[e].task.fields.clone());
        let (stage, assignee, priority) = match (editor, &current) {
            (Some(ed), _) => (ed.stage, ed.assignee, ed.priority),
            (None, Some(f)) => (f.stage_id, f.assignee, f.priority),
            (None, None) => return Vec::new(),
        };
        match mode {
            Mode::Stage => picker::stage_items(&self.scene, cluster, stage),
            Mode::Assignee => picker::assignee_items(&self.scene, column, assignee),
            Mode::Priority => picker::priority_items(priority),
            Mode::LinkType => picker::link_type_items(&self.scene, column),
            Mode::LinkTarget => eye
                .map(|e| picker::link_target_items(&self.scene, e, |o| !self.is_hidden(o)))
                .unwrap_or_default(),
            Mode::Unlink => eye
                .map(|e| picker::unlink_items(&self.scene, e))
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// What a pick from a changing palette does. False if it isn't one.
    pub(super) fn apply_change(
        &mut self,
        mode: Mode,
        pick: &Pick,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match pick {
            Pick::SetStage(stage) => {
                let stage = *stage;
                self.quick_change(|f| f.stage_id = stage, window, cx);
            }
            Pick::SetAssignee(who) => {
                let who = *who;
                self.quick_change(|f| f.assignee = who, window, cx);
            }
            Pick::SetPriority(priority) => {
                let priority = *priority;
                self.quick_change(|f| f.priority = priority, window, cx);
            }
            Pick::LinkType { id, forward, name } => {
                self.link_type = Some((*id, *forward, name.clone()));
                self.open_picker(Mode::LinkTarget, None, window, cx);
            }
            Pick::Task(other) if mode == Mode::LinkTarget => {
                let other = other.clone();
                self.link_to(&other);
            }
            Pick::Roles(column) => {
                let column = column.clone();
                self.open_roles(Some(column), window, cx);
            }
            Pick::Member(id) => {
                if let Some((_, member)) = &mut self.roles_for {
                    *member = Some(*id);
                    self.open_picker(Mode::MemberRoles, None, window, cx);
                }
            }
            Pick::ToggleRole(role) => self.toggle_role(*role),
            Pick::Unlink(relation) => {
                let Some(eye) = self.selected_eye() else {
                    return true;
                };
                let Some(place) = Place::of_cluster(&self.scene, self.scene.eyes[eye].cluster)
                else {
                    return true;
                };
                let reference = self.scene.eyes[eye].task.reference.clone();
                self.send_write(
                    place.source,
                    place.workspace,
                    Write::Unlink(*relation),
                    Pending::Done(format!("Removed a link from {reference}")),
                );
            }
            _ => return false,
        }
        true
    }

    fn link_to(&mut self, other: &NodeKey) {
        let (Some((type_id, forward, name)), Some(eye), Some(target)) = (
            self.link_type.take(),
            self.selected_eye(),
            self.scene.eye(other),
        ) else {
            return;
        };
        let Some(place) = Place::of_cluster(&self.scene, self.scene.eyes[eye].cluster) else {
            return;
        };
        let (this, that) = (&self.scene.eyes[eye].task, &self.scene.eyes[target].task);
        let (from, to) = if forward {
            (this.id, that.id)
        } else {
            (that.id, this.id)
        };
        let done = format!("{} {name} {}", this.reference, that.reference);
        self.send_write(
            place.source,
            place.workspace,
            Write::Link(CreateRelation { type_id, from, to }),
            Pending::Done(done),
        );
    }

    /// The comment box under the inspected task's comments, made fresh
    /// for each task so a half-written comment never lands on another.
    pub(super) fn composer_for(
        &mut self,
        eye: usize,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TextInput>> {
        let cluster = self.scene.eyes[eye].cluster;
        if self.refusal(cluster, Permission::Comment).is_some() {
            return None;
        }
        let key = self.scene.eyes[eye].key.clone();
        match &self.composer {
            Some((k, input)) if *k == key => Some(input.clone()),
            _ => {
                let input = cx.new(|cx| TextInput::multiline("Add a comment", cx));
                self.composer = Some((key, input.clone()));
                Some(input)
            }
        }
    }

    pub(super) fn post_comment(&mut self, cx: &mut Context<Self>) {
        let Some((key, input)) = self.composer.clone() else {
            return;
        };
        let body = input.read(cx).text().trim().to_owned();
        if body.is_empty() {
            return;
        }
        // The box keeps its text until the post is answered; a second
        // Ctrl+Enter meanwhile would post it twice.
        let posting = |p: &Pending| matches!(p, Pending::Comment(k, b) if *k == key && *b == body);
        if self.pending.values().any(posting) {
            return;
        }
        let Some(eye) = self.scene.eye(&key) else {
            return;
        };
        let cluster = self.scene.eyes[eye].cluster;
        if let Some(why) = self.refusal(cluster, Permission::Comment) {
            self.note(why);
            return;
        }
        let Some(place) = Place::of_cluster(&self.scene, cluster) else {
            return;
        };
        let task = self.scene.eyes[eye].task.id;
        self.send_write(
            place.source,
            place.workspace,
            Write::Comment {
                task,
                body: body.clone(),
            },
            Pending::Comment(key, body),
        );
    }

    pub(super) fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(eye) = self.selected_eye() else {
            self.note("Select a task first, then C comments on it");
            return;
        };
        if let Some(why) = self.refusal(self.scene.eyes[eye].cluster, Permission::Comment) {
            self.note(why);
            return;
        }
        if !self.inspecting {
            self.inspecting = true;
            self.load_detail();
        }
        if let Some(input) = self.composer_for(eye, cx) {
            window.focus(input.read(cx).focus_handle());
            // The box sits under every comment, usually below the fold.
            self.details_scroll.scroll_to_bottom();
        }
    }

    /// The answer to a write. Saved tasks go on screen straight away; the
    /// change event that follows brings the rest.
    pub(super) fn wrote(
        &mut self,
        source: SourceKey,
        request: u64,
        result: Result<Wrote, WriteError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.pending.remove(&request) else {
            return;
        };
        let mine = |saving: Option<u64>| saving == Some(request);
        // The form waiting on this request: does it still hold just what was
        // sent (it can close), or was it typed into while saving (it stays)?
        let still_sent = |this: &Self, sent: &TaskFields| {
            this.editor
                .as_ref()
                .filter(|e| mine(e.saving))
                .map(|e| matches!(e.fields(cx), Ok(f) if f == *sent))
        };
        match (pending, result) {
            (Pending::Save { key, draft, .. }, Ok(Wrote::Task(task))) => {
                self.patch_task(&source, &task);
                match still_sent(self, &draft) {
                    Some(true) => {
                        self.editor = None;
                        window.focus(&self.focus_handle);
                    }
                    Some(false) => {
                        self.keep_typing(&task, None);
                        self.note(format!(
                            "Saved {}. What you typed since is still a draft",
                            task.reference
                        ));
                    }
                    None => {}
                }
                if let Some(key) = key.filter(|k| self.selected.as_ref() == Some(k)) {
                    self.request_detail(&key);
                }
            }
            (Pending::Create(sent), Ok(Wrote::Task(task))) => {
                self.patch_task(&source, &task);
                let key = NodeKey {
                    source,
                    id: task.id.0,
                };
                let kept = still_sent(self, &sent) == Some(false);
                match kept {
                    // Now an edit of the task just made, which the panel opens on.
                    true => self.keep_typing(&task, Some(key.clone())),
                    false if self.editor.as_ref().is_some_and(|e| mine(e.saving)) => {
                        self.editor = None;
                        window.focus(&self.focus_handle);
                    }
                    false => {}
                }
                if let Some(eye) = self.scene.eye(&key) {
                    self.go_to(eye, true);
                }
                self.note(match kept {
                    true => format!(
                        "Created {}. What you typed since is still a draft",
                        task.reference
                    ),
                    false => format!("Created {}", task.reference),
                });
            }
            // One review at a time, and it's never replaced: this draft waits
            // in its form, or a note says it didn't save once the review closes.
            (Pending::Save { label, .. }, Err(WriteError::Conflict(latest)))
                if self.conflict.is_some() =>
            {
                self.patch_task(&source, &latest);
                match &mut self.editor {
                    Some(editor) if mine(editor.saving) => {
                        editor.saving = None;
                        editor.problem = Some(format!(
                            "{label} changed meanwhile. Save again to review it"
                        ));
                    }
                    _ => self.unsaved.push(label),
                }
            }
            (
                Pending::Save {
                    place,
                    key,
                    label,
                    draft,
                },
                Err(WriteError::Conflict(latest)),
            ) => {
                // The form itself goes to the review, typing after Save and all.
                let editor = self.editor.take_if(|e| mine(e.saving));
                let draft = editor
                    .as_ref()
                    .and_then(|e| e.fields(cx).ok())
                    .unwrap_or(*draft);
                self.patch_task(&source, &latest);
                let focus = cx.focus_handle();
                window.focus(&focus);
                self.conflict = Some(Conflict {
                    place,
                    key,
                    label,
                    draft,
                    latest: *latest,
                    focus,
                    editor,
                });
            }
            (Pending::Comment(key, body), Ok(_)) => {
                // Text typed while the post was on its way is a new draft.
                if let Some((k, input)) = &self.composer
                    && *k == key
                    && input.read(cx).text().trim() == body
                {
                    input.update(cx, |input, cx| input.set_text("", cx));
                }
                // Another task opened meanwhile has its own request waiting.
                if self.selected.as_ref() == Some(&key) {
                    self.request_detail(&key);
                }
            }
            // The dialog open now may be a newer one, opened after this
            // request's own was cancelled; only its own answer touches it.
            (Pending::Dialog(made), Ok(_)) => {
                if self.dialog.as_ref().is_some_and(|d| mine(d.waiting_on())) {
                    self.dialog = None;
                    window.focus(&self.focus_handle);
                }
                self.note(format!("Created the {made}"));
            }
            (Pending::Delete(key, reference), Ok(_)) => {
                if self.dialog.as_ref().is_some_and(|d| mine(d.waiting_on())) {
                    self.dialog = None;
                    window.focus(&self.focus_handle);
                }
                self.forget_task(&key);
                self.note(format!("Deleted {reference}"));
            }
            (Pending::Delete(_, reference), Err(WriteError::Failed(why))) => {
                match &mut self.dialog {
                    Some(dialog) if mine(dialog.waiting_on()) => {
                        *dialog.saving() = None;
                        *dialog.problem() = Some(why);
                    }
                    _ => self.note(format!("{reference} wasn't deleted: {why}")),
                }
            }
            (Pending::Dialog(made), Err(WriteError::Failed(why))) => {
                let why = why_text(&why).unwrap_or(why);
                match &mut self.dialog {
                    Some(dialog) if mine(dialog.waiting_on()) => {
                        *dialog.saving() = None;
                        *dialog.problem() = Some(why);
                    }
                    _ => self.note(format!("The {made} wasn't created: {why}")),
                }
            }
            (Pending::Done(done), Ok(_)) => self.note(done),
            (_, Err(WriteError::Failed(why))) => {
                // A form waiting on this shows why and keeps its draft.
                match &mut self.editor {
                    Some(editor) if mine(editor.saving) => {
                        editor.saving = None;
                        editor.problem = Some(why);
                    }
                    _ => self.note(format!("That didn't save: {why}")),
                }
            }
            (_, _) => {}
        }
    }

    /// Takes a deleted task off the screen now rather than after the
    /// refetch, along with its links and anything open on it.
    fn forget_task(&mut self, key: &NodeKey) {
        if self.selected.as_ref() == Some(key) {
            self.selected = None;
            self.inspecting = false;
        }
        self.editor.take_if(|e| e.key.as_ref() == Some(key));
        self.composer.take_if(|(k, _)| k == key);
        let Some(state) = self.sources.iter_mut().find(|s| s.key == key.source) else {
            return;
        };
        let id = hyakume_core::ids::TaskId(key.id);
        for snapshot in state.workspaces.iter_mut().flat_map(|w| &mut w.snapshots) {
            snapshot.tasks.retain(|t| t.id != id);
            snapshot.relations.retain(|r| r.from != id && r.to != id);
            snapshot.linked_tasks.retain(|t| t.id != id);
        }
        self.rebuild();
    }

    /// Puts `task` into the loaded data as saved, so the canvas shows it
    /// now instead of after the change event's refetch.
    fn patch_task(&mut self, source: &SourceKey, task: &Task) {
        let Some(state) = self.sources.iter_mut().find(|s| &s.key == source) else {
            return;
        };
        for snapshot in state.workspaces.iter_mut().flat_map(|w| &mut w.snapshots) {
            if snapshot.project.id != task.project_id {
                continue;
            }
            match snapshot.tasks.iter_mut().find(|t| t.id == task.id) {
                // An older answer must not undo a newer one.
                Some(old) if old.version >= task.version => {}
                Some(old) => *old = task.clone(),
                None => snapshot.tasks.push(task.clone()),
            }
        }
        self.rebuild();
    }

    pub(super) fn conflict_event(
        &mut self,
        event: FormEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(conflict) = self.conflict.take() else {
            return;
        };
        if !self.unsaved.is_empty() {
            let unsaved = std::mem::take(&mut self.unsaved).join(", ");
            self.note_for(
                format!("{unsaved} also changed meanwhile, so your change there wasn't saved"),
                Duration::from_secs(10),
            );
        }
        match event {
            FormEvent::Discard => {
                window.focus(&self.focus_handle);
                if let Some(key) = &conflict.key {
                    self.request_detail(key);
                }
            }
            _ => {
                // The draft goes back in the form, now based on the latest.
                let target = Target::Edit {
                    task: conflict.latest.id,
                    base: conflict.latest.version,
                };
                let editor = match conflict.editor {
                    Some(mut editor) => {
                        editor.target = target;
                        editor.saving = None;
                        editor.problem = None;
                        editor
                    }
                    None => Editor::new(
                        conflict.place,
                        target,
                        conflict.key.clone(),
                        conflict.label,
                        &conflict.draft,
                        cx,
                    ),
                };
                self.editor = Some(editor);
                if let Some(eye) = conflict.key.as_ref().and_then(|k| self.scene.eye(k)) {
                    self.select(eye, true);
                }
                self.focus_title(window, cx);
            }
        }
    }

    /// A dialog for workspace `column`, or for the one in view.
    pub(super) fn open_dialog(
        &mut self,
        role: bool,
        column: Option<NodeKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let column = column
            .and_then(|k| self.scene.columns.iter().position(|c| c.key == k))
            .or(self
                .context_cluster()
                .map(|c| self.scene.clusters[c].column));
        let Some(i) = column else {
            self.note("Open a project first, or pick the workspace in the palette");
            return;
        };
        let col = &self.scene.columns[i];
        let permission = match role {
            true => Permission::ManageRoles,
            false => Permission::ConfigureProjects,
        };
        let Some(workspace) = col.workspace else {
            return;
        };
        if role && col.source == SourceKey::Local {
            self.note("Local work is yours alone, so it has no roles to set up");
            return;
        }
        if !col.allows(permission, None) {
            let why = match matches!(col.status, Status::Live) {
                false => format!("{} isn't connected, so it's read-only for now", col.chip),
                true if role => format!("Your role can't manage roles in {}", col.title),
                true => format!("Your role can't add relationship types in {}", col.title),
            };
            self.note(why);
            return;
        }
        let place = DialogPlace {
            source: col.source.clone(),
            workspace,
            name: format!("{} / {}", col.chip, col.title),
        };
        let dialog = match role {
            true => Dialog::role(place, cx),
            false => Dialog::relation_type(place, cx),
        };
        window.focus(&dialog.first_focus(cx));
        self.dialog = Some(dialog);
    }

    /// Delete asks first, in a dialog where Escape keeps the task.
    pub(super) fn ask_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(eye) = self.selected_eye() else {
            self.note("Select a task first, then Delete removes it");
            return;
        };
        let cluster = self.scene.eyes[eye].cluster;
        if let Some(why) = self.refusal(cluster, Permission::DeleteTasks) {
            self.note(why);
            return;
        }
        let Some(place) = Place::of_cluster(&self.scene, cluster) else {
            return;
        };
        let column = &self.scene.columns[self.scene.clusters[cluster].column];
        let place = DialogPlace {
            source: place.source,
            workspace: place.workspace,
            name: format!("{} / {}", column.chip, column.title),
        };
        let key = self.scene.eyes[eye].key.clone();
        let dialog = Dialog::delete_task(place, key, &self.scene.eyes[eye].task, cx);
        window.focus(&dialog.first_focus(cx));
        self.dialog = Some(dialog);
    }

    /// The palette's way to change someone's roles: the workspace in
    /// `column` (or the one in view), then the person, then the role.
    pub(super) fn open_roles(
        &mut self,
        column: Option<NodeKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let column = column
            .and_then(|k| self.scene.columns.iter().position(|c| c.key == k))
            .or(self
                .context_cluster()
                .map(|c| self.scene.clusters[c].column));
        let Some(col) = column.map(|i| &self.scene.columns[i]) else {
            self.note("Open a project first, or pick the workspace in the palette");
            return;
        };
        let why = if col.source == SourceKey::Local {
            Some("Local work is yours alone, so it has no roles to give".to_owned())
        } else if !matches!(col.status, Status::Live) {
            Some(format!(
                "{} isn't connected, so it's read-only for now",
                col.chip
            ))
        } else if !col.allows(Permission::ManageRoles, None) {
            Some(format!("Your role can't manage roles in {}", col.title))
        } else {
            None
        };
        if let Some(why) = why {
            self.note(why);
            return;
        }
        self.roles_for = Some((col.key.clone(), None));
        let restore = window.focused(cx);
        self.open_picker(Mode::Members, restore, window, cx);
    }

    /// Rows for the two role palettes, from what was loaded last.
    pub(super) fn role_items(&self, mode: Mode) -> Vec<crate::picker::Item> {
        let Some((key, member)) = &self.roles_for else {
            return Vec::new();
        };
        let Some(col) = self.scene.columns.iter().find(|c| c.key == *key) else {
            return Vec::new();
        };
        match (mode, member) {
            (Mode::Members, _) => crate::picker::member_items(&col.members, &col.roles),
            (Mode::MemberRoles, Some(id)) => col
                .members
                .iter()
                .find(|m| m.identity.id == *id)
                .map(|m| crate::picker::member_role_items(m, &col.roles))
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// Gives `role` to the person picked, or takes it away. The server
    /// takes the whole set, built from the roles they had when loaded.
    fn toggle_role(&mut self, role: hyakume_core::ids::RoleId) {
        let Some((key, Some(id))) = self.roles_for.take() else {
            return;
        };
        let Some(col) = self.scene.columns.iter().find(|c| c.key == key) else {
            return;
        };
        let (Some(member), Some(found)) = (
            col.members.iter().find(|m| m.identity.id == id),
            col.roles.iter().find(|r| r.id == role),
        ) else {
            return;
        };
        let Some(workspace) = col.workspace else {
            return;
        };
        let name = &member.identity.display_name;
        let mut roles = member.roles.clone();
        let done = match roles.contains(&role) {
            true => {
                roles.retain(|r| *r != role);
                format!("Took the {} role from {name}", found.name)
            }
            false => {
                roles.push(role);
                format!("Gave {name} the {} role", found.name)
            }
        };
        self.send_write(
            col.source.clone(),
            workspace,
            Write::SetRoles {
                identity: id,
                roles,
            },
            Pending::Done(done),
        );
    }

    pub(super) fn dialog_event(
        &mut self,
        event: FormEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        match event {
            FormEvent::Cancel => {
                self.dialog = None;
                window.focus(&self.focus_handle);
            }
            FormEvent::ToggleDirected => {
                if let Dialog::RelationType { directed, .. } = dialog {
                    *directed = !*directed;
                }
            }
            FormEvent::TogglePermission(p) => {
                if let Dialog::Role { permissions, .. } = dialog
                    && !permissions.remove(&p)
                {
                    permissions.insert(p);
                }
            }
            FormEvent::Save => {
                if dialog.saving().is_some() {
                    return;
                }
                let body = match dialog.body(cx) {
                    Ok(body) => body,
                    Err(problem) => {
                        *dialog.problem() = Some(problem);
                        return;
                    }
                };
                *dialog.problem() = None;
                let place = dialog.place().clone();
                let (write, pending) = match body {
                    DialogBody::RelationType(body) => (
                        Write::CreateRelationType(body),
                        Pending::Dialog("relationship type"),
                    ),
                    DialogBody::Role(body) => (Write::CreateRole(body), Pending::Dialog("role")),
                    DialogBody::DeleteTask { key, task } => {
                        let reference = self
                            .scene
                            .eye(&key)
                            .map(|e| self.scene.eyes[e].task.reference.clone())
                            .unwrap_or_else(|| "the task".into());
                        (Write::DeleteTask(task), Pending::Delete(key, reference))
                    }
                };
                let request = self.send_write(place.source, place.workspace, write, pending);
                if let Some(dialog) = &mut self.dialog {
                    *dialog.saving() = Some(request);
                }
            }
            _ => {}
        }
    }

    /// Enter in a form: a new line in a multiline field, else submit when
    /// a field has focus. On a button it does nothing here, and the
    /// button's own key-up click takes it.
    pub(super) fn form_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focused = |input: &Entity<TextInput>| input.read(cx).focus_handle().is_focused(window);
        if let Some(dialog) = &self.dialog {
            if dialog.inputs().into_iter().any(&focused) {
                self.dialog_event(FormEvent::Save, window, cx);
            }
            return;
        }
        if self.conflict.is_some() {
            return;
        }
        if let Some((_, input)) = self.composer.clone()
            && focused(&input)
        {
            input.update(cx, |input, cx| input.newline(window, cx));
            return;
        }
        if let Some(editor) = &self.editor {
            // Labels go one per line, so Enter adds one there too.
            if let Some(field) = [&editor.description, &editor.labels]
                .into_iter()
                .find(|i| focused(i))
            {
                let field = field.clone();
                field.update(cx, |input, cx| input.newline(window, cx));
            } else if editor.inputs().into_iter().any(&focused) {
                self.save_editor(cx);
            }
        }
    }

    /// Up and Down move between rows of whichever multiline field has focus.
    pub(super) fn form_line(&mut self, down: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mut multiline: Vec<Entity<TextInput>> = Vec::new();
        multiline.extend(self.composer.as_ref().map(|(_, i)| i.clone()));
        if let Some(editor) = &self.editor {
            multiline.extend([editor.description.clone(), editor.labels.clone()]);
        }
        for input in multiline {
            if input.read(cx).focus_handle().is_focused(window) {
                input.update(cx, |input, cx| match down {
                    true => input.line_down(cx),
                    false => input.line_up(cx),
                });
            }
        }
    }

    /// Moves the selected eye, or the web the arrows are on, a step. A
    /// nudge pins it, like a drag.
    pub(super) fn nudge(&mut self, dx: f32, dy: f32) {
        let by = Vec2::new(dx * NUDGE, dy * NUDGE);
        if let Some(eye) = self.selected_eye() {
            let at = self.placement.eyes[eye] + by;
            let node = self.layout.move_eye(&self.scene, &self.placement, eye, at);
            self.after_move(self.scene.eyes[eye].cluster, node);
        } else if let Some(c) = self.web_cursor.as_ref().and_then(|k| self.scene.cluster(k))
            && self.focus.is_none()
        {
            let at = self.placement.clusters[c].0 + by;
            let node = self
                .layout
                .move_cluster(&self.scene, &self.placement, c, at);
            self.after_move(c, node);
        } else {
            self.note("Select a task (or a web on the overview) to move it");
        }
    }

    /// Re-places everything around a moved node and saves its new spot.
    pub(super) fn after_move(&mut self, cluster: usize, node: LayoutNode) {
        self.replace();
        self.save_node(cluster, node);
    }

    pub(super) fn replace(&mut self) {
        self.placement = std::rc::Rc::new(self.layout.place(&self.scene));
        if let Some(c) = self.focus.as_ref().and_then(|k| self.scene.cluster(k)) {
            self.stubs = std::rc::Rc::new(crate::canvas::stub_positions(
                &self.scene,
                &self.placement,
                c,
            ));
        }
    }

    pub(super) fn save_node(&mut self, cluster: usize, node: LayoutNode) {
        let column = &self.scene.columns[self.scene.clusters[cluster].column];
        let Some(workspace) = column.workspace else {
            return;
        };
        self.runtime.send(Command::SaveLayout {
            source: column.source.clone(),
            workspace,
            nodes: vec![node],
        });
    }

    pub(super) fn toggle_pin(&mut self) {
        let Some(eye) = self.selected_eye() else {
            self.note("Select a task first, then P pins it where it is");
            return;
        };
        let Some(node) = self.layout.toggle_pin(&self.scene, eye) else {
            return;
        };
        let reference = self.scene.eyes[eye].task.reference.clone();
        self.note(match node.pinned {
            true => format!("Pinned {reference}: re-layout leaves it where it is"),
            false => format!("Unpinned {reference}"),
        });
        self.after_move(self.scene.eyes[eye].cluster, node);
    }

    /// The buttons under a task's title, for the mouse; each has a key.
    pub(super) fn task_actions(&self, ring: Option<Hsla>, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let act = |id: &'static str, label: &'static str, action: &dyn gpui::Action| {
            let hint = keys::hint(action, cx);
            button(&t, id, ring)
                .h(px(28.0))
                .px(px(10.0))
                .gap(px(6.0))
                .border_1()
                .border_color(t.line)
                .child(label)
                .children(hint.map(|h| div().text_color(t.muted).child(h)))
        };
        div()
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .child(act("task-edit", "Edit", &EditTask).on_click(cx.listener(
                |this, _: &ClickEvent, window, cx| {
                    this.start_edit(window, cx);
                    cx.notify();
                },
            )))
            .child(
                act("task-stage", "Stage", &MoveToStage).on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| {
                        this.open_change(Mode::Stage, window, cx);
                        cx.notify();
                    },
                )),
            )
            .child(
                act("task-assign", "Assign", &AssignTask).on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| {
                        this.open_change(Mode::Assignee, window, cx);
                        cx.notify();
                    },
                )),
            )
            .child(act("task-link", "Link", &LinkTask).on_click(cx.listener(
                |this, _: &ClickEvent, window, cx| {
                    this.open_change(Mode::LinkType, window, cx);
                    cx.notify();
                },
            )))
            .child(
                act("task-delete", "Delete", &DeleteTask).on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| {
                        this.ask_delete(window, cx);
                        cx.notify();
                    },
                )),
            )
            .into_any_element()
    }

    /// The task form, wrapped in the `Form` key context with its keys.
    pub(super) fn editor_panel(
        &self,
        ring: Option<Hsla>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let editor = self.editor.as_ref()?;
        let listen = edit::listener(cx, |this: &mut Self, event, _, window, cx| {
            match event {
                FormEvent::Save => this.save_editor(cx),
                FormEvent::Cancel => this.cancel_editor(window),
                FormEvent::PickStage => this.open_change(Mode::Stage, window, cx),
                FormEvent::PickAssignee => this.open_change(Mode::Assignee, window, cx),
                FormEvent::PickPriority => this.open_change(Mode::Priority, window, cx),
                _ => {}
            }
            cx.notify();
        });
        let hints = Hints {
            save: keys::hint(&SaveForm, cx),
            cancel: keys::hint(&CancelForm, cx),
            stage: keys::hint(&MoveToStage, cx),
            assign: keys::hint(&AssignTask, cx),
        };
        let ui = edit::Ui {
            t: &self.theme,
            ring,
            listen: &listen,
            window,
            cx,
        };
        let form = edit::editor_form(editor, &self.scene, hints, ui);
        Some(
            div()
                .key_context("Form")
                .on_action(cx.listener(|this, _: &SaveForm, _, cx| {
                    this.save_editor(cx);
                    cx.notify();
                }))
                .on_action(cx.listener(|this, _: &CancelForm, window, cx| {
                    this.cancel_editor(window);
                    cx.notify();
                }))
                .on_action(cx.listener(|this, _: &FormEnter, window, cx| {
                    this.form_enter(window, cx);
                    cx.notify();
                }))
                .on_action(cx.listener(|this, _: &FormUp, window, cx| {
                    this.form_line(false, window, cx);
                }))
                .on_action(cx.listener(|this, _: &FormDown, window, cx| {
                    this.form_line(true, window, cx);
                }))
                .child(form)
                .into_any_element(),
        )
    }

    /// A new task's form, in the side panel's place.
    pub(super) fn create_panel(
        &self,
        width: f32,
        ring: Option<Hsla>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let editor = self
            .editor
            .as_ref()
            .filter(|e| e.target == Target::Create)?;
        let t = self.theme;
        let form = self.editor_panel(ring, window, cx)?;
        Some(
            div()
                .id("new-task")
                .w(px(width))
                .h_full()
                .flex_shrink_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(16.0))
                .px(px(24.0))
                .py(px(20.0))
                .bg(t.surface)
                .border_l_1()
                .border_color(t.line)
                .child(
                    div()
                        .text_size(px(18.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(crate::inspector::clean(
                            &format!("New task in {}", editor.label),
                            false,
                        )),
                )
                .child(form)
                .into_any_element(),
        )
    }

    /// The comment box, with Ctrl+Enter to post.
    pub(super) fn composer_view(
        &self,
        input: Entity<TextInput>,
        ring: Option<Hsla>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        let focused = input.read(cx).focus_handle().is_focused(window);
        let hint = keys::hint(&SaveForm, cx);
        div()
            .key_context("Form")
            .on_action(cx.listener(|this, _: &SaveForm, _, cx| {
                this.post_comment(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CancelForm, window, cx| {
                window.focus(&this.focus_handle);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &FormEnter, window, cx| {
                this.form_enter(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FormUp, window, cx| {
                this.form_line(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FormDown, window, cx| {
                this.form_line(true, window, cx);
            }))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .px(px(10.0))
                    .py(px(6.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(if focused {
                        ring.unwrap_or(t.accent)
                    } else {
                        t.line
                    })
                    .bg(t.bg)
                    .text_size(px(13.0))
                    .line_height(px(19.0))
                    .child(input),
            )
            .child(
                div().flex().child(
                    button(&t, "post-comment", ring)
                        .gap(px(8.0))
                        .border_1()
                        .border_color(t.line)
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.post_comment(cx);
                            cx.notify();
                        }))
                        .child("Post comment")
                        .children(hint.map(|h| div().text_color(t.muted).child(h))),
                ),
            )
            .into_any_element()
    }

    /// The conflict review or a dialog, over everything, like the palette.
    pub(super) fn modal(
        &self,
        window_w: f32,
        ring: Option<Hsla>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let t = self.theme;
        let hints = (keys::hint(&SaveForm, cx), keys::hint(&CancelForm, cx));
        let (content, focus): (AnyElement, FocusHandle) = if let Some(conflict) = &self.conflict {
            let listen = edit::listener(cx, |this: &mut Self, event, _, window, cx| {
                this.conflict_event(event, window, cx);
                cx.notify();
            });
            let width = (window_w - 32.0).min(860.0);
            // Escape keeps the draft here, so Discard gets no key at all:
            // throwing work away takes a deliberate click or Enter on it.
            let hints = (hints.0, None);
            (
                edit::conflict_view(conflict, &self.scene, width, hints, &t, ring, &listen),
                conflict.focus.clone(),
            )
        } else if let Some(dialog) = &self.dialog {
            let listen = edit::listener(cx, |this: &mut Self, event, _, window, cx| {
                this.dialog_event(event, window, cx);
                cx.notify();
            });
            let width = (window_w - 32.0).min(480.0);
            (
                edit::dialog_view(
                    dialog,
                    width,
                    hints,
                    edit::Ui {
                        t: &t,
                        ring,
                        listen: &listen,
                        window,
                        cx,
                    },
                ),
                dialog.focus().clone(),
            )
        } else {
            return None;
        };
        let conflict = self.conflict.is_some();
        Some(
            div()
                .id("modal-backdrop")
                .absolute()
                .size_full()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(HEADER_H + 12.0))
                .bg(gpui::black().opacity(0.35))
                .occlude()
                // Unlike the palette, a stray click outside doesn't throw the
                // draft or a half-filled dialog away.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .id("modal")
                        .key_context("Form")
                        .track_focus(&focus)
                        .on_action(cx.listener(move |this, _: &SaveForm, window, cx| {
                            match conflict {
                                true => this.conflict_event(FormEvent::Revise, window, cx),
                                false => this.dialog_event(FormEvent::Save, window, cx),
                            }
                            cx.notify();
                        }))
                        .on_action(cx.listener(move |this, _: &CancelForm, window, cx| {
                            // Escape on the review keeps the draft: back to editing it.
                            match conflict {
                                true => this.conflict_event(FormEvent::Revise, window, cx),
                                false => this.dialog_event(FormEvent::Cancel, window, cx),
                            }
                            cx.notify();
                        }))
                        .on_action(cx.listener(|this, _: &FormEnter, window, cx| {
                            this.form_enter(window, cx);
                            cx.notify();
                        }))
                        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                            cx.stop_propagation()
                        })
                        .child(content),
                )
                .into_any_element(),
        )
    }
}

/// Server refusals read as sentences already; this only shortens the one
/// people hit most in dialogs.
fn why_text(why: &str) -> Option<String> {
    why.contains("already exists")
        .then(|| format!("{why}. Pick another name."))
}
