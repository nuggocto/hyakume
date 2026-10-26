//! The palette: one text field over a filtered list. It lists commands
//! (Ctrl+Shift+P), tasks to find (Ctrl+F), or links to follow, and the root
//! view decides what each pick does.

use std::rc::Rc;

use gpui::{
    Action, App, Entity, FontWeight, ScrollStrategy, SharedString, UniformListScrollHandle, Window,
    div, prelude::*, px, uniform_list,
};

use crate::inspector;
use crate::scene::{Dir, NodeKey, Scene};
use crate::sources::SourceKey;
use crate::text_input::TextInput;
use crate::theme::Theme;

/// Called with a row's position in the list when it's clicked.
pub type OnPick = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// Rows shown at once; the rest scroll, by wheel or by moving the highlight.
const ROWS: usize = 9;
/// Every row is this tall, which is what lets GPUI lay out only the visible ones.
const ROW_H: f32 = 34.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Commands,
    Find,
    Follow(Dir),
}

pub enum Pick {
    Action(Box<dyn Action>),
    Project(NodeKey),
    Workspace(NodeKey),
    Retry(SourceKey),
    Task(NodeKey),
    /// Hides or shows every task in the stage with this name key.
    Stage(String),
}

pub struct Item {
    pub label: SharedString,
    pub detail: SharedString,
    pub pick: Pick,
    /// Lowercased text the query is matched against.
    haystack: String,
}

impl Item {
    pub fn new(label: String, detail: String, extra: &str, pick: Pick) -> Self {
        let haystack = format!("{label} {detail} {extra}").to_lowercase();
        Self {
            label: label.into(),
            detail: detail.into(),
            pick,
            haystack,
        }
    }
}

pub struct Picker {
    pub mode: Mode,
    pub input: Entity<TextInput>,
    pub items: Vec<Item>,
    /// Indices into `items` that match the query, in order.
    pub shown: Vec<usize>,
    /// Position in `shown`.
    pub highlighted: usize,
    pub scroll: UniformListScrollHandle,
}

impl Picker {
    pub fn refilter(&mut self, cx: &App) {
        let query = self.input.read(cx).text().to_owned();
        self.shown = filter(&self.items, &query);
        self.highlighted = self.highlighted.min(self.shown.len().saturating_sub(1));
    }

    /// A new query starts over at the top. Live updates refilter too, but
    /// leave the scroll alone so someone browsing with the wheel stays put.
    pub fn restart(&mut self, cx: &App) {
        self.highlighted = 0;
        self.refilter(cx);
        self.reveal(ScrollStrategy::Top);
    }

    pub fn current(&self) -> Option<&Item> {
        self.shown.get(self.highlighted).map(|&i| &self.items[i])
    }

    pub fn step(&mut self, by: isize) {
        let n = self.shown.len();
        if n > 0 {
            self.highlighted = (self.highlighted as isize + by).rem_euclid(n as isize) as usize;
        }
        // Down pins the row to the bottom edge, so the list moves one row
        // at a time instead of a page.
        self.reveal(if by > 0 {
            ScrollStrategy::Bottom
        } else {
            ScrollStrategy::Top
        });
    }

    /// Scrolls only when the highlighted row is out of view, so a list the
    /// wheel moved stays put while the highlight is still on screen.
    fn reveal(&self, edge: ScrollStrategy) {
        self.scroll.scroll_to_item(self.highlighted, edge);
    }
}

/// Items holding every word of the query, in their original order.
pub fn filter(items: &[Item], query: &str) -> Vec<usize> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| words.iter().all(|w| item.haystack.contains(w.as_str())))
        .map(|(i, _)| i)
        .collect()
}

/// Every task in `scope` (one project, or all of them) that a stage filter
/// leaves `shown`, for Find.
pub fn task_items(scene: &Scene, scope: Option<usize>, shown: impl Fn(usize) -> bool) -> Vec<Item> {
    scene
        .eyes
        .iter()
        .enumerate()
        .filter(|&(i, eye)| scope.is_none_or(|c| eye.cluster == c) && shown(i))
        .map(|(_, eye)| eye)
        .map(|eye| {
            let cluster = &scene.clusters[eye.cluster];
            let column = &scene.columns[cluster.column];
            let task = &eye.task;
            let assignee = task
                .fields
                .assignee
                .and_then(|id| column.people.get(&id))
                .map_or("", String::as_str);
            let extra = format!(
                "{} {} {}",
                task.fields.labels.join(" "),
                assignee,
                cluster.name
            );
            Item::new(
                format!("{}  {}", task.reference, task.fields.title),
                format!("{} · {}", eye.stage, column.title),
                &extra,
                Pick::Task(eye.key.clone()),
            )
        })
        .collect()
}

/// The links leaving (or arriving at) one task, for following.
pub fn link_items(scene: &Scene, eye: usize, dir: Dir, shown: impl Fn(usize) -> bool) -> Vec<Item> {
    scene
        .links(eye, dir)
        .into_iter()
        .filter(|&(_, other)| shown(other))
        .map(|(name, other)| {
            let task = &scene.eyes[other].task;
            let cluster = &scene.clusters[scene.eyes[other].cluster];
            Item::new(
                format!("{name}  {}  {}", task.reference, task.fields.title),
                format!("{} · {}", scene.eyes[other].stage, cluster.project_key),
                "",
                Pick::Task(scene.eyes[other].key.clone()),
            )
        })
        .collect()
}

pub fn placeholder(mode: Mode) -> &'static str {
    match mode {
        Mode::Commands => "Type a command, project, or workspace",
        Mode::Find => "Find a task by title, reference, label, stage, or person",
        Mode::Follow(Dir::Out) => "Follow which link?",
        Mode::Follow(Dir::In) => "Follow which incoming link?",
    }
}

pub fn view(
    picker: &Picker,
    theme: &Theme,
    width: f32,
    footer: String,
    on_pick: OnPick,
) -> impl IntoElement {
    let t = *theme;
    let lit = picker.highlighted;
    // Shared labels, so the list can build whichever rows are on screen.
    let rows: Rc<Vec<(SharedString, SharedString)>> = Rc::new(
        picker
            .shown
            .iter()
            .map(|&i| {
                (
                    picker.items[i].label.clone(),
                    picker.items[i].detail.clone(),
                )
            })
            .collect(),
    );
    let visible = rows.len().min(ROWS);
    let list = uniform_list("pick-rows", rows.len(), move |range, _, _| {
        range
            .map(|at| {
                let (label, detail) = &rows[at];
                let on_pick = on_pick.clone();
                div()
                    .id(("pick", at))
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .px(px(14.0))
                    .h(px(ROW_H))
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .when(at == lit, |row| {
                        row.bg(t.raised).border_l_2().border_color(t.accent)
                    })
                    .hover(move |row| row.bg(t.raised))
                    .on_click(move |_, window, cx| on_pick(at, window, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(if at == lit {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .child(inspector::clean(label, false)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .max_w(px(width * 0.45))
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(t.muted)
                            .child(inspector::clean(detail, false)),
                    )
            })
            .collect()
    })
    .track_scroll(picker.scroll.clone())
    .h(px(visible as f32 * ROW_H));
    let count = if picker.shown.len() == picker.items.len() {
        format!("{}", picker.items.len())
    } else {
        format!("{} of {}", picker.shown.len(), picker.items.len())
    };
    let empty = picker.shown.is_empty().then(|| {
        div()
            .px(px(14.0))
            .py(px(10.0))
            .text_color(t.muted)
            .child(match picker.mode {
                Mode::Commands => "No command matches.",
                Mode::Find => "No task matches.",
                Mode::Follow(_) => "No link matches.",
            })
    });
    div()
        .key_context("Picker")
        .w(px(width))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .p(px(8.0))
        .rounded(px(14.0))
        .border_1()
        .border_color(t.line)
        .bg(t.surface)
        .shadow_lg()
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .px(px(10.0))
                .h(px(40.0))
                .text_size(px(15.0))
                .line_height(px(22.0))
                .border_b_1()
                .border_color(t.line)
                .child(picker.input.clone())
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(12.0))
                        .text_color(t.muted)
                        .child(SharedString::from(count)),
                ),
        )
        .when(visible > 0, |palette| palette.child(list))
        .children(empty)
        .child(
            div()
                .px(px(14.0))
                .pt(px(4.0))
                .text_size(px(11.0))
                .text_color(t.muted)
                .child(footer),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::tests::Fixture;

    #[test]
    fn find_matches_every_word_anywhere_in_a_task() {
        let mut ws = Fixture::new();
        ws.project("WEB", &[("Landing page", 2), ("Pricing teaser", 1)]);
        ws.project("API", &[("Signup endpoint", 3)]);
        ws.snapshots[0].tasks[1].fields.labels = vec!["launch".into()];
        let scene = Scene::build(&[ws.source()]);
        let items = task_items(&scene, None, |_| true);
        let found = |q: &str| -> Vec<String> {
            filter(&items, q)
                .into_iter()
                .map(|i| items[i].label.to_string())
                .collect()
        };

        assert_eq!(found("web-2"), ["WEB-2  Pricing teaser"]);
        assert_eq!(found("LAUNCH"), ["WEB-2  Pricing teaser"]);
        // Words can match different fields, in any order.
        assert_eq!(found("review signup"), ["API-1  Signup endpoint"]);
        assert_eq!(found("web page"), ["WEB-1  Landing page"]);
        assert!(found("web signup").is_empty());
        assert_eq!(found("  ").len(), 3);

        // Scoped to one project, the other project's tasks aren't offered.
        let api = scene.clusters.iter().position(|c| c.project_key == "API");
        let scoped = task_items(&scene, api, |_| true);
        assert_eq!(scoped.len(), 1);
        assert!(filter(&scoped, "web").is_empty());
    }
}
