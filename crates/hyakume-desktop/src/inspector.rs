//! The side panel for one selected task. Read-only until editing arrives.

use gpui::{
    AnyElement, App, ClickEvent, FontWeight, Hsla, IntoElement, ParentElement, SharedString,
    Styled, Window, canvas, div, prelude::*, px,
};
use hyakume_core::api::TaskDetail;
use hyakume_core::model::is_unsafe_char;
use time::OffsetDateTime;
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;

use crate::clock::LocalTime;
use crate::glyphs;
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
    Loaded(Box<TaskDetail>),
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

pub fn panel(
    scene: &Scene,
    eye: usize,
    detail: Option<&Detail>,
    theme: &Theme,
    local: &LocalTime,
    width: f32,
    on_close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let t = *theme;
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
        .child(chip("Read-only".into(), &t))
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
                .on_click(on_close)
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
        .flex()
        .flex_col()
        .gap(px(18.0))
        .px(px(24.0))
        .py(px(20.0))
        .bg(t.surface)
        .border_l_1()
        .border_color(t.line)
        .text_color(t.ink)
        .child(header)
        .child(title)
        .child(grid);

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

    if !task.fields.description.trim().is_empty() {
        body = body.child(section(
            "Description",
            div()
                .text_size(px(13.0))
                .line_height(px(20.0))
                .child(clean(&task.fields.description, true)),
            &t,
        ));
    }

    let threads: Vec<_> = scene
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
        .collect();
    if !threads.is_empty() {
        let rows = threads.into_iter().map(|(name, other)| {
            let other = &scene.eyes[other].task;
            div()
                .flex()
                .gap(px(12.0))
                .text_size(px(13.0))
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
        body = body.child(section(
            "Threads",
            div().flex().flex_col().gap(px(8.0)).children(rows),
            &t,
        ));
    }

    let comments: AnyElement = match detail {
        None | Some(Detail::Loading) => muted("Loading comments…", &t),
        Some(Detail::Failed(reason)) => muted(&format!("Comments aren't available: {reason}"), &t),
        Some(Detail::Loaded(detail)) if detail.comments.is_empty() => muted("No comments yet.", &t),
        Some(Detail::Loaded(detail)) => div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .children(detail.comments.iter().map(|comment| {
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
    body.child(section("Comments", comments, &t))
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
