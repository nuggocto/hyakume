//! A text field, one line or several. GPUI 0.2.2 ships no widgets, so this
//! follows its `input` example: the platform input handler gives us typing,
//! IME composition, and dead keys; we handle the caret and the clipboard.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, GlobalElementId,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window,
    WrappedLine, div, fill, point, prelude::*, px, relative, size,
};
use hyakume_core::model::is_unsafe_char;
use unicode_segmentation::UnicodeSegmentation;

use crate::keys::{
    Backspace, Copy, Cut, Delete, DeleteWordLeft, End, Home, Left, Paste, Right, SelectAll,
    SelectLeft, SelectRight, WordLeft, WordRight,
};

/// Sent after every edit, so the owner can react to the new text.
pub struct Changed;

pub struct TextInput {
    focus: FocusHandle,
    content: String,
    placeholder: SharedString,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    last_line: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    /// How far the line is shifted left so a long query keeps its caret in
    /// view. Clicks and the input method's popup are offset by it too.
    scroll_x: Pixels,
    /// Where the input method was last told the caret is.
    ime_caret: Option<Bounds<Pixels>>,
    selecting: bool,
    /// Several lines: Enter adds a line, and the text wraps instead of
    /// scrolling sideways.
    multiline: bool,
    /// The wrapped paragraphs as last painted, when multiline.
    last_block: Option<Block>,
    /// How far a multiline field is scrolled down to keep its caret in view.
    scroll_y: Pixels,
    /// Rows a multiline field shows before it scrolls.
    rows: f32,
}

/// One painted multiline field. Paragraphs are split at newlines, and each
/// wraps on its own; `starts` and `tops` place them in the text and on screen.
#[derive(Clone)]
struct Block {
    lines: Vec<WrappedLine>,
    starts: Vec<usize>,
    tops: Vec<Pixels>,
    line_height: Pixels,
    /// Top left of the text, scroll included.
    origin: Point<Pixels>,
}

impl Block {
    fn paragraph(&self, offset: usize) -> usize {
        self.starts.iter().rposition(|&s| s <= offset).unwrap_or(0)
    }

    /// Where `offset` sits, relative to the top left of the text.
    fn position(&self, offset: usize) -> Point<Pixels> {
        let i = self.paragraph(offset);
        let local = offset
            .saturating_sub(self.starts[i])
            .min(self.lines[i].len());
        let at = self.lines[i]
            .position_for_index(local, self.line_height)
            .unwrap_or_default();
        point(at.x, self.tops[i] + at.y)
    }

    /// The offset nearest `at`, given relative to the top left of the text.
    fn index_at(&self, at: Point<Pixels>, len: usize) -> usize {
        if at.y < px(0.0) {
            return 0;
        }
        let Some(i) = self.tops.iter().rposition(|&top| top <= at.y) else {
            return 0;
        };
        let line = &self.lines[i];
        let height = line.size(self.line_height).height;
        if at.y >= self.tops[i] + height && i + 1 == self.lines.len() {
            return len;
        }
        let inside = point(
            at.x.max(px(0.0)),
            (at.y - self.tops[i]).min(height - px(1.0)),
        );
        let local = match line.closest_index_for_position(inside, self.line_height) {
            Ok(i) | Err(i) => i,
        };
        self.starts[i] + local
    }
}

impl EventEmitter<Changed> for TextInput {}

impl TextInput {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle().tab_stop(true),
            content: String::new(),
            placeholder: placeholder.into(),
            selected: 0..0,
            reversed: false,
            marked: None,
            last_line: None,
            last_bounds: None,
            scroll_x: px(0.0),
            ime_caret: None,
            selecting: false,
            multiline: false,
            last_block: None,
            scroll_y: px(0.0),
            rows: 6.0,
        }
    }

    pub fn multiline(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            multiline: true,
            ..Self::new(placeholder, cx)
        }
    }

    /// How many rows a multiline field shows before it scrolls.
    pub fn with_rows(mut self, rows: f32) -> Self {
        self.rows = rows;
        self
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    /// Replaces the whole text and puts the caret at its end.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.content.clear();
        self.marked = None;
        let inserted = self.splice(Some(0..0), text);
        self.selected = inserted.end..inserted.end;
        self.reversed = false;
        cx.notify();
    }

    /// Enter in a multiline field: a new line where the caret is.
    pub fn newline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, "\n", window, cx);
    }

    /// Up and Down in a multiline field move the caret a row; true if it moved.
    pub fn line_up(&mut self, cx: &mut Context<Self>) -> bool {
        self.vertical(-1.0, cx)
    }

    pub fn line_down(&mut self, cx: &mut Context<Self>) -> bool {
        self.vertical(1.0, cx)
    }

    fn vertical(&mut self, rows: f32, cx: &mut Context<Self>) -> bool {
        let Some(block) = self.last_block.as_ref().filter(|_| self.multiline) else {
            return false;
        };
        let at = block.position(self.cursor());
        let target = point(at.x, at.y + block.line_height * (rows + 0.5));
        let offset = block.index_at(target, self.content.len());
        self.move_to(offset, cx);
        true
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected = offset..offset;
        self.reversed = false;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.reversed {
            self.selected.start = offset;
        } else {
            self.selected.end = offset;
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(i, _)| (i < offset).then_some(i))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(i, _)| (i > offset).then_some(i))
            .unwrap_or(self.content.len())
    }

    /// Start of the word before `offset`, skipping the spaces in between.
    fn previous_word(&self, offset: usize) -> usize {
        self.content
            .split_word_bound_indices()
            .rev()
            .find(|(i, w)| *i < offset && !w.trim().is_empty())
            .map_or(0, |(i, _)| i)
    }

    fn next_word(&self, offset: usize) -> usize {
        self.content
            .split_word_bound_indices()
            .find(|(i, w)| *i + w.len() > offset && !w.trim().is_empty())
            .map_or(self.content.len(), |(i, w)| i + w.len())
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            self.move_to(self.previous_boundary(self.cursor()), cx);
        } else {
            self.move_to(self.selected.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            self.move_to(self.next_boundary(self.selected.end), cx);
        } else {
            self.move_to(self.selected.end, cx);
        }
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.previous_word(self.cursor()), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.next_word(self.cursor()), cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            self.select_to(self.previous_boundary(self.cursor()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            self.select_to(self.next_boundary(self.cursor()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_word_left(
        &mut self,
        _: &DeleteWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected.is_empty() {
            self.select_to(self.previous_word(self.cursor()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected.is_empty() {
            let text = self.content[self.selected.clone()].to_owned();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected.is_empty() {
            let text = self.content[self.selected.clone()].to_owned();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn index_at(&self, position: Point<Pixels>) -> usize {
        if self.multiline {
            return match &self.last_block {
                Some(block) => block.index_at(position - block.origin, self.content.len()),
                None => 0,
            };
        }
        let (Some(bounds), Some(line)) = (self.last_bounds, self.last_line.as_ref()) else {
            return 0;
        };
        if self.content.is_empty() || position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left() + self.scroll_x)
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.selecting = true;
        if event.modifiers.shift {
            self.select_to(self.index_at(event.position), cx);
        } else {
            self.move_to(self.index_at(event.position), cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.select_to(self.index_at(event.position), cx);
        }
    }

    // The platform speaks UTF-16 offsets; the content is UTF-8.
    fn utf8_offset(&self, offset: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for c in self.content.chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += c.len_utf16();
            utf8 += c.len_utf8();
        }
        utf8
    }

    fn utf16_offset(&self, offset: usize) -> usize {
        self.content[..offset].chars().map(char::len_utf16).sum()
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.utf8_offset(range.start)..self.utf8_offset(range.end)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.utf16_offset(range.start)..self.utf16_offset(range.end)
    }

    /// Replaces `range` (or the composition, or the selection) with `text`.
    fn splice(&mut self, range: Option<Range<usize>>, text: &str) -> Range<usize> {
        let range = range
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        let text = clean(text, self.multiline);
        self.content.replace_range(range.clone(), &text);
        range.start..range.start + text.len()
    }
}

/// Control characters never get in; newlines and tabs only into a multiline
/// field, the same rule the server holds descriptions to. Text pasted from
/// Windows ends lines with \r\n, which is one line break, not two.
fn clean(text: &str, multiline: bool) -> String {
    text.replace("\r\n", "\n")
        .chars()
        .map(|c| match c {
            '\r' if multiline => '\n',
            '\n' | '\t' | '\r' if !multiline => ' ',
            c => c,
        })
        .filter(|&c| matches!(c, '\n' | '\t') || !is_unsafe_char(c))
        .collect()
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range);
        actual.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected),
            reversed: self.reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range.map(|r| self.range_from_utf16(&r));
        let inserted = self.splice(range, text);
        self.selected = inserted.end..inserted.end;
        self.reversed = false;
        self.marked = None;
        cx.emit(Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        new_selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range.map(|r| self.range_from_utf16(&r));
        let inserted = self.splice(range, text);
        self.marked = (!inserted.is_empty()).then_some(inserted.clone());
        // The new selection is given relative to the inserted text.
        self.selected = match new_selected {
            Some(r) => {
                let start = inserted.start;
                let within = &self.content[start..inserted.end];
                let at = |u: usize| {
                    start
                        + within
                            .char_indices()
                            .scan(0, |n, (i, c)| {
                                let here = (*n, i);
                                *n += c.len_utf16();
                                Some(here)
                            })
                            .find(|(n, _)| *n >= u)
                            .map_or(within.len(), |(_, i)| i)
                };
                at(r.start)..at(r.end)
            }
            None => inserted.end..inserted.end,
        };
        cx.emit(Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // An empty field keeps no line (the placeholder's would give wrong
        // offsets), but the input method still needs the caret there on its
        // first use, or it puts its candidates in a corner of the screen.
        let range = self.range_from_utf16(&range);
        if let Some(block) = self.last_block.as_ref().filter(|_| self.multiline) {
            let (start, end) = (block.position(range.start), block.position(range.end));
            return Some(Bounds::from_corners(
                block.origin + start,
                block.origin + point(end.x.max(start.x), end.y + block.line_height),
            ));
        }
        let x = |i: usize| {
            self.last_line
                .as_ref()
                .map_or(px(0.0), |l| l.x_for_index(i))
        };
        let left = bounds.left() - self.scroll_x;
        Some(Bounds::from_corners(
            point(left + x(range.start), bounds.top()),
            point(left + x(range.end), bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        at: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        if let Some(block) = self.last_block.as_ref().filter(|_| self.multiline) {
            let index = block.index_at(at - block.origin, self.content.len());
            return Some(self.utf16_offset(index));
        }
        let local = self.last_bounds?.localize(&at)?;
        let line = self.last_line.as_ref()?;
        let index = line.index_for_x(local.x + self.scroll_x)?;
        Some(self.utf16_offset(index))
    }
}

impl Render for TextInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_1()
            .min_w_0()
            .key_context("TextInput")
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .map(|field| match self.multiline {
                true => field.child(Lines { input: cx.entity() }).into_any_element(),
                false => field.child(Line { input: cx.entity() }).into_any_element(),
            })
    }
}

/// The painted line itself. It registers the input handler while painting,
/// which is how GPUI routes typed text to whoever has focus.
struct Line {
    input: Entity<TextInput>,
}

/// The new scroll for a caret at `caret`: moved just far enough to reach
/// it, and never past the end of the text, so deleting pulls text back in.
fn follow_caret(scroll: f32, caret: f32, text_w: f32, view_w: f32) -> f32 {
    let reach = scroll.min(caret).max(caret - view_w);
    reach.min(text_w - view_w).max(0.0)
}

struct Prepainted {
    origin: Point<Pixels>,
    line: ShapedLine,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for Line {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Line {
    type RequestLayoutState = ();
    type PrepaintState = Prepainted;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepainted {
        let input = self.input.read(cx);
        let style = window.text_style();
        let (shown, color) = if input.content.is_empty() {
            (input.placeholder.clone(), style.color.opacity(0.5))
        } else {
            (SharedString::from(input.content.clone()), style.color)
        };
        let run = TextRun {
            len: shown.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        // Text being composed with an input method is underlined.
        let runs = match &input.marked {
            Some(marked) if !input.content.is_empty() => [
                (0..marked.start, None),
                (
                    marked.clone(),
                    Some(UnderlineStyle {
                        color: Some(color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                ),
                (marked.end..shown.len(), None),
            ]
            .into_iter()
            .filter(|(r, _)| !r.is_empty())
            .map(|(r, underline)| TextRun {
                len: r.len(),
                underline,
                ..run.clone()
            })
            .collect(),
            _ => vec![run],
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(shown, font_size, &runs, None);

        let x = |i: usize| {
            if input.content.is_empty() {
                px(0.0)
            } else {
                line.x_for_index(i)
            }
        };
        let caret_w = px(1.5);
        let scroll = px(follow_caret(
            f32::from(input.scroll_x),
            f32::from(x(input.cursor())),
            f32::from(line.width),
            f32::from(bounds.size.width - caret_w),
        ));
        let left = bounds.left() - scroll;
        let (selection, cursor) = if input.selected.is_empty() {
            let at = point(left + x(input.cursor()), bounds.top());
            let caret = fill(
                Bounds::new(at, size(caret_w, bounds.size.height)),
                style.color,
            );
            (None, Some(caret))
        } else {
            let area = Bounds::from_corners(
                point(left + x(input.selected.start), bounds.top()),
                point(left + x(input.selected.end), bounds.bottom()),
            );
            (Some(fill(area, style.color.opacity(0.2))), None)
        };
        self.input.update(cx, |input, _| input.scroll_x = scroll);
        Prepainted {
            origin: point(left, bounds.top()),
            line,
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepainted: &mut Prepainted,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let caret = prepainted
            .cursor
            .as_ref()
            .or(prepainted.selection.as_ref())
            .map(|quad| quad.bounds);
        // Text scrolled out of the field must not paint over its neighbours.
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if let Some(selection) = prepainted.selection.take() {
                window.paint_quad(selection);
            }
            let _ = prepainted
                .line
                .paint(prepainted.origin, window.line_height(), window, cx);
            if focus.is_focused(window)
                && let Some(cursor) = prepainted.cursor.take()
            {
                window.paint_quad(cursor);
            }
        });
        let line = prepainted.line.clone();
        let focused = focus.is_focused(window);
        let caret = focused.then_some(caret).flatten();
        let moved = self.input.update(cx, |input, _| {
            // The placeholder's layout would give wrong offsets for clicks.
            input.last_line = (!input.content.is_empty()).then_some(line);
            input.last_bounds = Some(bounds);
            std::mem::replace(&mut input.ime_caret, caret) != caret
        });
        // GPUI tells the input method where the caret is only when asked.
        // Without this, its candidate window opens in a corner of the screen.
        if moved && caret.is_some() {
            window.invalidate_character_coordinates();
        }
    }
}

/// The painted text of a multiline field: wrapped paragraphs, scrolled
/// down as far as the caret needs, under a fixed height.
struct Lines {
    input: Entity<TextInput>,
}

struct LinesPrepainted {
    block: Block,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
    /// The placeholder, when the field is empty.
    placeholder: Option<ShapedLine>,
}

impl IntoElement for Lines {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Lines {
    type RequestLayoutState = ();
    type PrepaintState = LinesPrepainted;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (window.line_height() * self.input.read(cx).rows).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> LinesPrepainted {
        let input = self.input.read(cx);
        let style = window.text_style();
        let line_height = window.line_height();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let run = |len: usize, color| TextRun {
            len,
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let content = SharedString::from(input.content.clone());
        let lines: Vec<WrappedLine> = window
            .text_system()
            .shape_text(
                content.clone(),
                font_size,
                &[run(content.len(), style.color)],
                Some(bounds.size.width),
                None,
            )
            .map(|lines| lines.into_iter().collect())
            .unwrap_or_default();
        let mut starts = Vec::with_capacity(lines.len());
        let mut tops = Vec::with_capacity(lines.len());
        let (mut start, mut top) = (0, px(0.0));
        for line in &lines {
            starts.push(start);
            tops.push(top);
            start += line.len() + 1;
            top += line.size(line_height).height;
        }
        let placeholder = input.content.is_empty().then(|| {
            let text = input.placeholder.clone();
            let len = text.len();
            window.text_system().shape_line(
                text,
                font_size,
                &[run(len, style.color.opacity(0.5))],
                None,
            )
        });
        let mut block = Block {
            lines,
            starts,
            tops,
            line_height,
            origin: bounds.origin,
        };
        if block.lines.is_empty() {
            block.lines.push(WrappedLine::default());
            block.starts.push(0);
            block.tops.push(px(0.0));
        }

        // Scroll just enough to keep the caret's row in the field.
        let caret = block.position(input.cursor());
        let view = bounds.size.height;
        let mut scroll = input.scroll_y;
        if caret.y < scroll {
            scroll = caret.y;
        } else if caret.y + line_height > scroll + view {
            scroll = caret.y + line_height - view;
        }
        scroll = scroll.min((top - view).max(px(0.0))).max(px(0.0));
        block.origin = point(bounds.left(), bounds.top() - scroll);

        let at = |offset: usize| block.origin + block.position(offset);
        let (cursor, selection) = if input.selected.is_empty() {
            let caret = fill(
                Bounds::new(at(input.cursor()), size(px(1.5), line_height)),
                style.color,
            );
            (Some(caret), Vec::new())
        } else {
            // One band per row the selection touches.
            let (from, to) = (at(input.selected.start), at(input.selected.end));
            let mut bands = Vec::new();
            let mut y = from.y;
            while y <= to.y {
                let left = if y == from.y { from.x } else { bounds.left() };
                let right = if y == to.y { to.x } else { bounds.right() };
                if right > left {
                    bands.push(fill(
                        Bounds::from_corners(point(left, y), point(right, y + line_height)),
                        style.color.opacity(0.2),
                    ));
                }
                y += line_height;
            }
            (None, bands)
        };
        self.input.update(cx, |input, _| input.scroll_y = scroll);
        LinesPrepainted {
            block,
            cursor,
            selection,
            placeholder,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepainted: &mut LinesPrepainted,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let block = &prepainted.block;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for band in prepainted.selection.drain(..) {
                window.paint_quad(band);
            }
            if let Some(placeholder) = &prepainted.placeholder {
                let _ = placeholder.paint(bounds.origin, block.line_height, window, cx);
            }
            for (line, &top) in block.lines.iter().zip(&block.tops) {
                let origin = block.origin + point(px(0.0), top);
                let _ = line.paint(
                    origin,
                    block.line_height,
                    TextAlign::Left,
                    Some(bounds),
                    window,
                    cx,
                );
            }
            if focus.is_focused(window)
                && let Some(cursor) = prepainted.cursor.take()
            {
                window.paint_quad(cursor);
            }
        });
        let caret = block.origin + block.position(self.input.read(cx).cursor());
        let caret = focus
            .is_focused(window)
            .then(|| Bounds::new(caret, size(px(1.5), block.line_height)));
        let block = block.clone();
        let moved = self.input.update(cx, |input, _| {
            input.last_block = Some(block);
            input.last_bounds = Some(bounds);
            std::mem::replace(&mut input.ime_caret, caret) != caret
        });
        if moved && caret.is_some() {
            window.invalidate_character_coordinates();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{clean, follow_caret};

    #[test]
    fn pasted_line_breaks_stay_one_break_whatever_system_wrote_them() {
        for text in ["first\r\nsecond", "first\nsecond", "first\rsecond"] {
            assert_eq!(clean(text, true), "first\nsecond", "{text:?}");
            assert_eq!(clean(text, false), "first second", "{text:?}");
        }
        // A tab survives only where newlines do; other control characters never.
        assert_eq!(clean("a\tb\u{7}c", true), "a\tbc");
        assert_eq!(clean("a\tb\u{7}c", false), "a bc");
    }

    #[test]
    fn a_long_line_scrolls_to_keep_the_caret_in_the_field() {
        let view = 100.0;
        // Short text never scrolls.
        assert_eq!(follow_caret(0.0, 40.0, 40.0, view), 0.0);
        // Typing past the right edge pulls the line left just enough.
        assert_eq!(follow_caret(0.0, 130.0, 130.0, view), 30.0);
        // Moving back inside the view leaves the scroll alone.
        assert_eq!(follow_caret(30.0, 80.0, 130.0, view), 30.0);
        // Home goes back to the start.
        assert_eq!(follow_caret(30.0, 0.0, 130.0, view), 0.0);
        // Deleting the end of a long line pulls the rest back in.
        assert_eq!(follow_caret(30.0, 110.0, 110.0, view), 10.0);
        assert_eq!(follow_caret(30.0, 60.0, 60.0, view), 0.0);
    }
}
