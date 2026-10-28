//! The side panel for one selected task: its fields, or the form editing
//! them, then its blockers, links, and comments.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, ClickEvent, FocusHandle, FontWeight, Hsla, IntoElement, ParentElement,
    Pixels, ScrollHandle, SharedString, Styled, Window, canvas, div, point, prelude::*, px,
};
use hyakume_core::model::is_unsafe_char;
use time::OffsetDateTime;
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;

use crate::clock::LocalTime;
use crate::glyphs;
use crate::runtime::Fetched;
use crate::scene::Scene;
use crate::theme::Theme;

const POSTED: &[BorrowedFormatItem<'_>] = format_description!(
    "[month repr:short] [day padding:none], [hour repr:12 padding:none]:[minute] [period]"
);

/// When a comment was posted, in the viewer's time zone: "Oct 24, 2:34 PM".
pub fn posted_at(at: OffsetDateTime, local: &LocalTime) -> String {
    // The offset of that moment, not today's: summer comments stay summer time.
    at.to_offset(local.offset_at(at))
        .format(POSTED)
        .unwrap_or_default()
}

pub enum Detail {
    Loading,
    /// Fresh, or the copy kept from when it was last fetched.
    Loaded(Box<Fetched>),
    Failed(String),
}

/// Text from a server goes into the panel without control or bidi characters.
pub fn clean(text: &str, multiline: bool) -> SharedString {
    text.chars()
        .map(|c| {
            let kept = multiline && matches!(c, '\n' | '\t');
            if is_unsafe_char(c) && !kept {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect::<String>()
        .into()
}

type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type OnEye = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// The panel's keyboard stops: its close button, then one per thread row.
/// The window keeps them so Tab can scroll the one it lands on into view,
/// since GPUI moves focus without scrolling.
#[derive(Clone, Default)]
pub struct Stops {
    handles: Vec<FocusHandle>,
    /// Where each thread row was last painted.
    rows: Rc<RefCell<Vec<Bounds<Pixels>>>>,
}

impl Stops {
    /// Makes sure there are enough stops for the panel on `eye`.
    pub fn prepare(&mut self, scene: &Scene, eye: usize, cx: &mut App) {
        let needed = 1 + links(scene, eye).len();
        while self.handles.len() < needed {
            self.handles.push(cx.focus_handle().tab_stop(true));
        }
    }

    /// Scrolls the panel just enough to show the focused stop, if one is.
    pub fn reveal(&self, scroll: &ScrollHandle, window: &Window) {
        let Some(i) = self.handles.iter().position(|h| h.is_focused(window)) else {
            return;
        };
        let y = match i {
            // The close button sits at the very top.
            0 => 0.0,
            _ => {
                let Some(row) = self.rows.borrow().get(i - 1).copied() else {
                    return;
                };
                let view = scroll.bounds();
                reveal_offset(
                    f32::from(scroll.offset().y),
                    (f32::from(view.top()), f32::from(view.bottom())),
                    (f32::from(row.top()), f32::from(row.bottom())),
                    -f32::from(scroll.max_offset().height),
                )
            }
        };
        scroll.set_offset(point(scroll.offset().x, px(y)));
    }
}

/// The scroll offset that brings `item` (top and bottom on screen) inside
/// `view` with a little room, moving as little as possible. Offsets go
/// negative as the content scrolls down, as GPUI counts them.
fn reveal_offset(offset: f32, view: (f32, f32), item: (f32, f32), lowest: f32) -> f32 {
    const ROOM: f32 = 16.0;
    let shift = if item.0 < view.0 + ROOM {
        view.0 + ROOM - item.0
    } else if item.1 > view.1 - ROOM {
        view.1 - ROOM - item.1
    } else {
        0.0
    };
    (offset + shift).clamp(lowest.min(0.0), 0.0)
}

/// Every link touching `eye`, named from its side, with the eye at the other end.
fn links(scene: &Scene, eye: usize) -> Vec<(String, usize)> {
    scene
        .threads
        .iter()
        .filter_map(|th| {
            if th.from == eye {
                Some((th.name.clone(), th.to))
            } else if th.to == eye {
                Some((th.reverse.clone(), th.from))
            } else {
                None
            }
        })
        .collect()
}

/// What the panel needs from the window around it.
pub struct Hooks {
    pub scroll: ScrollHandle,
    pub stops: Stops,
    /// The focus ring color, while the keyboard is in use.
    pub ring: Option<Hsla>,
    pub on_close: OnClick,
    /// Go to the eye at the other end of a link.
    pub on_follow: OnEye,
    /// The edit form, shown instead of the fields while editing.
    pub form: Option<AnyElement>,
    /// Buttons for changing the task, under its title.
    pub actions: Option<AnyElement>,
    /// The box for a new comment, under the comments.
    pub composer: Option<AnyElement>,
    /// Why the task can't be changed from here, if it can't.
    pub read_only: Option<&'static str>,
}

pub fn panel(
    scene: &Scene,
    eye: usize,
    detail: Option<&Detail>,
    theme: &Theme,
    local: &LocalTime,
    width: f32,
    hooks: Hooks,
) -> impl IntoElement {
    let t = *theme;
    let ring = hooks.ring;
    let e = &scene.eyes[eye];
    let task = &e.task;
    let column = &scene.columns[scene.clusters[e.cluster].column];
    let shape = e.shape;

    let header = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(t.muted)
                .child(clean(&task.reference, false)),
        )
        .children(hooks.read_only.map(|why| chip(why.into(), &t)))
        .child(div().flex_1())
        .child(
            div()
                .id("close-inspector")
                .w(px(32.0))
                .h(px(32.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(16.0))
                .text_color(t.muted)
                .text_size(px(18.0))
                .cursor_pointer()
                .hover(move |s| s.bg(t.raised))
                .track_focus(&hooks.stops.handles[0])
                .border_2()
                .border_color(gpui::transparent_black())
                .when_some(ring, |b, ring| b.focus(move |s| s.border_color(ring)))
                .on_click(hooks.on_close)
                .child("×"),
        );

    let title = div()
        .flex()
        .items_center()
        .gap(px(14.0))
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    glyphs::eye(window, shape, bounds.center(), 56.0, &t);
                },
            )
            .flex_shrink_0()
            .w(px(64.0))
            .h(px(46.0)),
        )
        .child(
            // flex_1 + min_w_0 give the text a width, so long titles wrap.
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(22.0))
                .font_weight(FontWeight::SEMIBOLD)
                .line_height(px(27.0))
                .child(clean(&task.fields.title, false)),
        );

    let assignee = task.fields.assignee.map_or_else(
        || "Unassigned".to_owned(),
        |id| {
            column
                .people
                .get(&id)
                .cloned()
                .unwrap_or_else(|| "Someone".into())
        },
    );
    let mut fields = vec![
        ("Stage", text(&e.stage, &t)),
        ("Assignee", text(&assignee, &t)),
    ];
    if !task.fields.labels.is_empty() {
        let labels = div()
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .children(task.fields.labels.iter().map(|l| chip(clean(l, false), &t)));
        fields.push(("Labels", labels.into_any_element()));
    }
    if let Some(priority) = task.fields.priority {
        let name = priority.as_str();
        let mut chars = name.chars();
        let capitalized = chars
            .next()
            .map_or(String::new(), |c| c.to_uppercase().chain(chars).collect());
        fields.push(("Priority", text(&capitalized, &t)));
    }
    if let Some(due) = task.fields.due_date {
        fields.push(("Due", text(&due.to_string(), &t)));
    }
    fields.push(("Version", text(&task.version.to_string(), &t)));
    let grid = div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .text_size(px(13.0))
        .children(fields.into_iter().map(|(name, value)| {
            div()
                .flex()
                .items_center()
                .child(
                    div()
                        .w(px(96.0))
                        .flex_shrink_0()
                        .text_color(t.muted)
                        .child(name),
                )
                .child(div().flex_1().min_w_0().child(value))
        }));

    let mut body = div()
        .id("inspector")
        .w(px(width))
        .h_full()
        .flex_shrink_0()
        .overflow_y_scroll()
        .track_scroll(&hooks.scroll)
        .flex()
        .flex_col()
        .gap(px(18.0))
        .px(px(24.0))
        .py(px(20.0))
        .bg(t.surface)
        .border_l_1()
        .border_color(t.line)
        .text_color(t.ink)
        .child(header);
    let editing = hooks.form.is_some();
    body = match hooks.form {
        Some(form) => body.child(form),
        None => body.child(title).children(hooks.actions).child(grid),
    };

    if !e.blocked_by.is_empty() {
        let lines = e.blocked_by.iter().map(|b| {
            let state = match b.eye {
                Some(i) => format!("still in {}", scene.eyes[i].stage),
                None => "not finished".into(),
            };
            div().child(clean(
                &format!("Blocked by {} {}, {state}.", b.reference, b.title),
                false,
            ))
        });
        body = body.child(
            div()
                .flex()
                .items_start()
                .gap(px(10.0))
                .px(px(14.0))
                .py(px(12.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(t.blocked)
                .bg(t.blocked_bg)
                .text_size(px(13.0))
                .line_height(px(19.0))
                // Nudged down so the mark lines up with the first line of text.
                .child(div().mt(px(1.5)).child(badge(&t)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .children(lines),
                ),
        );
    }

    if !editing && !task.fields.description.trim().is_empty() {
        body = body.child(section(
            "Description",
            div()
                .text_size(px(13.0))
                .line_height(px(20.0))
                .child(clean(&task.fields.description, true)),
            &t,
        ));
    }

    let threads = links(scene, eye);
    if !threads.is_empty() {
        let rows = threads.into_iter().enumerate().map(|(i, (name, at))| {
            let other = &scene.eyes[at].task;
            let follow = hooks.on_follow.clone();
            div()
                .id(("thread", i))
                .flex()
                .gap(px(12.0))
                .mx(px(-8.0))
                .px(px(6.0))
                .py(px(4.0))
                .rounded(px(8.0))
                .border_2()
                .border_color(gpui::transparent_black())
                .text_size(px(13.0))
                .cursor_pointer()
                .hover(move |s| s.bg(t.raised))
                .track_focus(&hooks.stops.handles[i + 1])
                .when_some(ring, |row, ring| row.focus(move |s| s.border_color(ring)))
                .on_click(move |_, window, cx| follow(at, window, cx))
                .child(
                    div()
                        .w(px(84.0))
                        .flex_shrink_0()
                        .text_color(t.muted)
                        .child(clean(&name, false)),
                )
                .child(
                    div()
                        .w(px(64.0))
                        .flex_shrink_0()
                        .child(clean(&other.reference, false)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(clean(&other.fields.title, false)),
                )
        });
        let painted = hooks.stops.rows.clone();
        body = body.child(section(
            "Threads",
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .on_children_prepainted(move |bounds, _, _| *painted.borrow_mut() = bounds)
                .children(rows),
            &t,
        ));
    }

    let comments: AnyElement = match detail {
        None | Some(Detail::Loading) => muted("Loading comments…", &t),
        Some(Detail::Failed(reason)) => muted(&format!("Comments aren't available: {reason}"), &t),
        Some(Detail::Loaded(fetched)) if fetched.detail.comments.is_empty() => {
            muted(&saved_note(fetched, "No comments", local), &t)
        }
        Some(Detail::Loaded(fetched)) => div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .when(fetched.saved_at.is_some(), |d| {
                d.child(muted(&saved_note(fetched, "Comments", local), &t))
            })
            .children(fetched.detail.comments.iter().map(|comment| {
                let who = comment
                    .author
                    .and_then(|id| column.people.get(&id).cloned())
                    .unwrap_or_else(|| "Someone".into());
                let when = posted_at(comment.created_at, local);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .px(px(12.0))
                    .py(px(10.0))
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(t.line)
                    .bg(t.raised)
                    .text_size(px(13.0))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.0))
                            .text_size(px(12.0))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(clean(&who, false)),
                            )
                            .child(div().text_color(t.muted).child(format!("· {when}"))),
                    )
                    .child(
                        div()
                            .line_height(px(19.0))
                            .child(clean(&comment.body, true)),
                    )
            }))
            .into_any_element(),
    };
    body.child(section(
        "Comments",
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(comments)
            .children(hooks.composer),
        &t,
    ))
}

/// "No comments yet.", or when offline, the age of what's shown.
fn saved_note(fetched: &Fetched, what: &str, local: &LocalTime) -> String {
    match fetched.saved_at {
        None => format!("{what} yet."),
        Some(at) => format!(
            "{what} as of {}, the last time you opened this task. The server can't be reached.",
            posted_at(at, local)
        ),
    }
}

fn text(value: &str, t: &Theme) -> AnyElement {
    div()
        .text_color(t.ink)
        .child(clean(value, false))
        .into_any_element()
}

fn muted(value: &str, t: &Theme) -> AnyElement {
    div()
        .text_size(px(13.0))
        .text_color(t.muted)
        .child(clean(value, false))
        .into_any_element()
}

fn chip(label: SharedString, t: &Theme) -> impl IntoElement {
    div()
        .px(px(8.0))
        .py(px(1.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(t.line)
        .text_size(px(11.0))
        .text_color(t.muted)
        .child(label)
}

fn badge(t: &Theme) -> impl IntoElement {
    round_mark(t.blocked, t.on_blocked)
}

/// The red "!" used for blocked tasks on the canvas and in the panel.
pub fn round_mark(bg: Hsla, fg: Hsla) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .w(px(16.0))
        .h(px(16.0))
        .rounded(px(8.0))
        .bg(bg)
        .text_color(fg)
        .text_size(px(11.0))
        .font_weight(FontWeight::EXTRA_BOLD)
        .flex()
        .items_center()
        .justify_center()
        .child("!")
}

fn section(title: &str, content: impl IntoElement, t: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(
            div()
                .text_size(px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.muted)
                .child(SharedString::from(title.to_owned())),
        )
        .child(content)
}

#[cfg(test)]
mod tests {
    use time::UtcOffset;
    use time::macros::{datetime, offset};

    use super::*;

    #[test]
    fn tabbing_to_a_row_out_of_view_scrolls_just_enough_to_show_it() {
        // A 600 px panel from y 100, scrollable 800 px further down.
        let (view, lowest) = ((100.0, 700.0), -800.0);
        // Where a row lands on screen after scrolling from `offset`.
        let shown = |offset: f32, row: (f32, f32)| {
            let moved = reveal_offset(offset, view, row, lowest) - offset;
            (row.0 + moved, row.1 + moved)
        };
        let inside = |(top, bottom): (f32, f32)| top > view.0 && bottom < view.1;
        let middle = (view.0 + view.1) / 2.0;

        // Below the fold: it comes up into view but stays near the bottom.
        let below = shown(0.0, (900.0, 930.0));
        assert!(inside(below) && below.0 > middle, "{below:?}");
        // Above the top edge: back down into view, near the top.
        let above = shown(-400.0, (40.0, 70.0));
        assert!(inside(above) && above.1 < middle, "{above:?}");
        // Already in view: nothing moves.
        assert_eq!(reveal_offset(-120.0, view, (300.0, 330.0), lowest), -120.0);
        // Never past either end of the content.
        assert_eq!(reveal_offset(-790.0, view, (720.0, 750.0), lowest), -800.0);
        assert_eq!(reveal_offset(-10.0, view, (95.0, 125.0), lowest), 0.0);
    }

    #[test]
    fn comment_times_use_the_offset_of_their_own_date() {
        // Paris: +1 in winter, +2 from the last Sunday of March 2026, 01:00 UTC.
        let summer_starts = datetime!(2026-03-29 01:00 UTC).unix_timestamp();
        let paris =
            LocalTime::from_changes(vec![(i64::MIN, offset!(+1)), (summer_starts, offset!(+2))]);
        assert_eq!(
            posted_at(datetime!(2026-02-10 13:34 UTC), &paris),
            "Feb 10, 2:34 PM"
        );
        assert_eq!(
            posted_at(datetime!(2026-07-10 13:34 UTC), &paris),
            "Jul 10, 3:34 PM"
        );
        // The second before and the second of the change.
        assert_eq!(
            posted_at(datetime!(2026-03-29 00:59:59 UTC), &paris),
            "Mar 29, 1:59 AM"
        );
        assert_eq!(
            posted_at(datetime!(2026-03-29 01:00 UTC), &paris),
            "Mar 29, 3:00 AM"
        );

        let utc = LocalTime::from_changes(vec![(i64::MIN, UtcOffset::UTC)]);
        assert_eq!(
            posted_at(datetime!(2026-10-24 00:05 UTC), &utc),
            "Oct 24, 12:05 AM"
        );
    }
}
