//! Every shortcut: the built-in table, then `keys.toml` on top, with hints
//! read from GPUI's live keymap. Contexts: `Hyakume` is the window, `Canvas`
//! the focused canvas, `Picker` the palette, `Form` a form or dialog, and
//! `TextInput` any text field.

use std::path::Path;
use std::rc::Rc;

use gpui::{
    Action, App, DummyKeyboardMapper, KeyBinding, KeyBindingContextPredicate, KeyContext,
    KeybindingKeystroke, Keymap, NoAction, actions,
};

/// The person's own shortcuts, next to `client.toml` in the profile.
pub const FILE: &str = "keys.toml";

actions!(
    hyakume,
    [
        OpenPalette,
        Find,
        ZoomIn,
        ZoomOut,
        Fit,
        Relayout,
        ShowAllStages,
        ShowOverview,
        ReloadKeys,
        Back,
        FocusNext,
        FocusPrev,
        ScrollDetailsUp,
        ScrollDetailsDown,
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        PanLeft,
        PanRight,
        PanUp,
        PanDown,
        Open,
        FollowOut,
        FollowIn,
        EditTask,
        NewTask,
        MoveToStage,
        AssignTask,
        SetPriority,
        LinkTask,
        UnlinkTask,
        CommentOnTask,
        TogglePin,
        DeleteTask,
        SetRoles,
        NudgeLeft,
        NudgeRight,
        NudgeUp,
        NudgeDown,
        NewRelationType,
        NewRole,
        SaveForm,
        CancelForm,
        FormEnter,
        FormUp,
        FormDown,
        PickerUp,
        PickerDown,
        PickerConfirm,
        PickerDismiss,
    ]
);

actions!(
    text_input,
    [
        Backspace,
        Delete,
        DeleteWordLeft,
        Left,
        Right,
        WordLeft,
        WordRight,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Copy,
        Cut,
        Paste,
    ]
);

/// A table in `keys.toml`, and the key context its bindings get.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Section {
    Window,
    Canvas,
    Palette,
    Form,
    Text,
}

impl Section {
    const ALL: [Self; 5] = [
        Self::Window,
        Self::Canvas,
        Self::Palette,
        Self::Form,
        Self::Text,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Canvas => "canvas",
            Self::Palette => "palette",
            Self::Form => "form",
            Self::Text => "text",
        }
    }

    fn context(self) -> &'static str {
        match self {
            Self::Window => "Hyakume",
            Self::Canvas => "Canvas",
            Self::Palette => "Picker",
            Self::Form => "Form",
            Self::Text => "TextInput",
        }
    }

    /// Where focus sits when someone reaches for this section's keys. Window
    /// keys get read off the header while the canvas has focus, and the only
    /// text field is the palette's.
    fn focus_path(self) -> Vec<KeyContext> {
        let path: &[&str] = match self {
            Self::Window | Self::Canvas => &["Hyakume", "Canvas"],
            Self::Palette | Self::Text => &["Hyakume", "Picker", "TextInput"],
            Self::Form => &["Hyakume", "Form", "TextInput"],
        };
        path.iter()
            .map(|c| KeyContext::parse(c).expect("contexts are plain identifiers"))
            .collect()
    }

    /// Window actions are handled on the root, which every focused element
    /// sits under, so they work from any section. The rest only from home.
    fn reaches(self, home: Self) -> bool {
        home == self || home == Self::Window
    }
}

/// Every action a shortcut can run, with the section whose element handles
/// it. A binding anywhere else would never reach the handler.
fn actions() -> Vec<(Section, Box<dyn Action>)> {
    use Section::*;
    vec![
        (Window, Box::new(OpenPalette)),
        (Window, Box::new(Find)),
        (Window, Box::new(ZoomIn)),
        (Window, Box::new(ZoomOut)),
        (Window, Box::new(Fit)),
        (Window, Box::new(Relayout)),
        (Window, Box::new(ShowAllStages)),
        (Window, Box::new(ShowOverview)),
        (Window, Box::new(ReloadKeys)),
        (Window, Box::new(Back)),
        (Window, Box::new(FocusNext)),
        (Window, Box::new(FocusPrev)),
        (Window, Box::new(ScrollDetailsUp)),
        (Window, Box::new(ScrollDetailsDown)),
        (Window, Box::new(Open)),
        (Window, Box::new(FollowOut)),
        (Window, Box::new(FollowIn)),
        (Window, Box::new(EditTask)),
        (Window, Box::new(NewTask)),
        (Window, Box::new(MoveToStage)),
        (Window, Box::new(AssignTask)),
        (Window, Box::new(SetPriority)),
        (Window, Box::new(LinkTask)),
        (Window, Box::new(UnlinkTask)),
        (Window, Box::new(CommentOnTask)),
        (Window, Box::new(TogglePin)),
        (Window, Box::new(DeleteTask)),
        (Window, Box::new(SetRoles)),
        (Window, Box::new(NewRelationType)),
        (Window, Box::new(NewRole)),
        (Canvas, Box::new(NudgeLeft)),
        (Canvas, Box::new(NudgeRight)),
        (Canvas, Box::new(NudgeUp)),
        (Canvas, Box::new(NudgeDown)),
        (Form, Box::new(SaveForm)),
        (Form, Box::new(CancelForm)),
        (Form, Box::new(FormEnter)),
        (Form, Box::new(FormUp)),
        (Form, Box::new(FormDown)),
        (Canvas, Box::new(MoveLeft)),
        (Canvas, Box::new(MoveRight)),
        (Canvas, Box::new(MoveUp)),
        (Canvas, Box::new(MoveDown)),
        (Canvas, Box::new(PanLeft)),
        (Canvas, Box::new(PanRight)),
        (Canvas, Box::new(PanUp)),
        (Canvas, Box::new(PanDown)),
        (Palette, Box::new(PickerUp)),
        (Palette, Box::new(PickerDown)),
        (Palette, Box::new(PickerConfirm)),
        (Palette, Box::new(PickerDismiss)),
        (Text, Box::new(Backspace)),
        (Text, Box::new(Delete)),
        (Text, Box::new(DeleteWordLeft)),
        (Text, Box::new(Left)),
        (Text, Box::new(Right)),
        (Text, Box::new(WordLeft)),
        (Text, Box::new(WordRight)),
        (Text, Box::new(SelectLeft)),
        (Text, Box::new(SelectRight)),
        (Text, Box::new(SelectAll)),
        (Text, Box::new(Home)),
        (Text, Box::new(End)),
        (Text, Box::new(Copy)),
        (Text, Box::new(Cut)),
        (Text, Box::new(Paste)),
    ]
}

/// "MoveLeft" for `hyakume::MoveLeft`: what people write in `keys.toml`.
fn short_name(action: &dyn Action) -> &'static str {
    let name = action.name();
    name.rsplit_once("::").map_or(name, |(_, short)| short)
}

/// A binding without a context outranks every contextual one in GPUI, so
/// window-wide shortcuts sit in the root `Hyakume` context instead. When an
/// action has two keys, hints show the one listed last.
pub fn bindings() -> Vec<KeyBinding> {
    let window = Some("Hyakume");
    let canvas = Some("Canvas");
    let picker = Some("Picker");
    let form = Some("Form");
    let input = Some("TextInput");
    vec![
        KeyBinding::new("ctrl-shift-p", OpenPalette, window),
        KeyBinding::new("ctrl-f", Find, window),
        KeyBinding::new("ctrl-+", ZoomIn, window),
        KeyBinding::new("ctrl-=", ZoomIn, window),
        KeyBinding::new("ctrl--", ZoomOut, window),
        KeyBinding::new("ctrl-0", Fit, window),
        KeyBinding::new("escape", Back, window),
        KeyBinding::new("tab", FocusNext, window),
        KeyBinding::new("shift-tab", FocusPrev, window),
        KeyBinding::new("pageup", ScrollDetailsUp, window),
        KeyBinding::new("pagedown", ScrollDetailsDown, window),
        KeyBinding::new("left", MoveLeft, canvas),
        KeyBinding::new("right", MoveRight, canvas),
        KeyBinding::new("up", MoveUp, canvas),
        KeyBinding::new("down", MoveDown, canvas),
        KeyBinding::new("shift-left", PanLeft, canvas),
        KeyBinding::new("shift-right", PanRight, canvas),
        KeyBinding::new("shift-up", PanUp, canvas),
        KeyBinding::new("shift-down", PanDown, canvas),
        KeyBinding::new("enter", Open, canvas),
        KeyBinding::new("o", FollowOut, canvas),
        KeyBinding::new("i", FollowIn, canvas),
        KeyBinding::new("e", EditTask, canvas),
        KeyBinding::new("n", NewTask, canvas),
        KeyBinding::new("m", MoveToStage, canvas),
        KeyBinding::new("a", AssignTask, canvas),
        KeyBinding::new("l", LinkTask, canvas),
        KeyBinding::new("c", CommentOnTask, canvas),
        KeyBinding::new("p", TogglePin, canvas),
        KeyBinding::new("delete", DeleteTask, canvas),
        KeyBinding::new("alt-left", NudgeLeft, canvas),
        KeyBinding::new("alt-right", NudgeRight, canvas),
        KeyBinding::new("alt-up", NudgeUp, canvas),
        KeyBinding::new("alt-down", NudgeDown, canvas),
        KeyBinding::new("ctrl-enter", SaveForm, form),
        KeyBinding::new("escape", CancelForm, form),
        KeyBinding::new("enter", FormEnter, form),
        KeyBinding::new("up", FormUp, form),
        KeyBinding::new("down", FormDown, form),
        KeyBinding::new("shift-tab", PickerUp, picker),
        KeyBinding::new("tab", PickerDown, picker),
        KeyBinding::new("up", PickerUp, picker),
        KeyBinding::new("down", PickerDown, picker),
        KeyBinding::new("enter", PickerConfirm, picker),
        KeyBinding::new("escape", PickerDismiss, picker),
        KeyBinding::new("backspace", Backspace, input),
        KeyBinding::new("delete", Delete, input),
        KeyBinding::new("ctrl-backspace", DeleteWordLeft, input),
        KeyBinding::new("left", Left, input),
        KeyBinding::new("right", Right, input),
        KeyBinding::new("ctrl-left", WordLeft, input),
        KeyBinding::new("ctrl-right", WordRight, input),
        KeyBinding::new("shift-left", SelectLeft, input),
        KeyBinding::new("shift-right", SelectRight, input),
        KeyBinding::new("ctrl-a", SelectAll, input),
        KeyBinding::new("home", Home, input),
        KeyBinding::new("end", End, input),
        KeyBinding::new("ctrl-c", Copy, input),
        KeyBinding::new("ctrl-x", Cut, input),
        KeyBinding::new("ctrl-v", Paste, input),
    ]
}

/// Binds the defaults, then the person's file on top, and returns what was
/// wrong with the file. GPUI prefers later bindings, so the file wins ties.
pub fn install(cx: &mut App, file: Option<&Path>) -> Vec<String> {
    cx.clear_key_bindings();
    cx.bind_keys(bindings());
    let Some(path) = file else {
        return Vec::new();
    };
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let custom = parse(&text);
            cx.bind_keys(custom.bindings);
            custom.problems
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => vec![format!("can't read it: {e}")],
    }
}

/// What a `keys.toml` adds, and why any of its entries were skipped.
pub struct Custom {
    pub bindings: Vec<KeyBinding>,
    pub problems: Vec<String>,
}

/// A bad entry is skipped on its own, so one typo doesn't cost someone the
/// rest of their shortcuts.
pub fn parse(text: &str) -> Custom {
    let mut custom = Custom {
        bindings: Vec::new(),
        problems: Vec::new(),
    };
    let table: toml::Table = match toml::from_str(text) {
        Ok(table) => table,
        Err(e) => {
            let line = e.span().map(|s| text[..s.start].matches('\n').count() + 1);
            let at = line.map(|l| format!("line {l}: ")).unwrap_or_default();
            custom.problems.push(format!("{at}{}", e.message().trim()));
            return custom;
        }
    };
    let defaults = bindings();
    for (name, entries) in &table {
        let Some(entries) = entries.as_table() else {
            custom.problems.push(format!(
                "{name:?} needs to go under a section such as [canvas]"
            ));
            continue;
        };
        let Some(section) = Section::ALL.into_iter().find(|s| s.name() == name) else {
            custom.problems.push(format!(
                "[{name}] isn't a section; use [window], [canvas], [palette], [form], or [text]"
            ));
            continue;
        };
        for (keys, value) in entries {
            match entry(section, keys, value, &defaults) {
                Ok(binding) => custom.bindings.push(binding),
                Err(why) => custom
                    .problems
                    .push(format!("[{}] {keys:?}: {why}", section.name())),
            }
        }
    }
    custom
}

fn entry(
    section: Section,
    keys: &str,
    value: &toml::Value,
    defaults: &[KeyBinding],
) -> Result<KeyBinding, String> {
    let action: Box<dyn Action> = match value {
        toml::Value::String(name) => actions()
            .into_iter()
            .find(|(_, a)| short_name(a.as_ref()) == name)
            .map(|(_, a)| a)
            .ok_or_else(|| format!("there's no action called {name:?}"))?,
        toml::Value::Boolean(false) => Box::new(NoAction),
        _ => return Err("expected an action name, or false to unbind the key".into()),
    };
    let predicate = KeyBindingContextPredicate::parse(section.context())
        .expect("section contexts are plain identifiers");
    let binding = KeyBinding::load(
        keys,
        action,
        Some(Rc::new(predicate)),
        false,
        None,
        &DummyKeyboardMapper,
    )
    .map_err(|_| "that isn't a key; write it like ctrl-shift-p".to_string())?;
    for k in binding.keystrokes() {
        if let Some((_, real)) = ALIASES.iter().find(|(alias, _)| *alias == k.key()) {
            return Err(format!("{:?} isn't a key name; try {real:?}", k.key()));
        }
    }
    if gpui::is_no_action(binding.action()) {
        let bound = defaults
            .iter()
            .any(|d| section_of(d) == Some(section) && d.keystrokes() == binding.keystrokes());
        if !bound {
            return Err(format!("nothing is bound to it in [{}]", section.name()));
        }
    } else {
        check(section, binding.action(), binding.keystrokes())?;
    }
    Ok(binding)
}

/// Spellings people reach for that GPUI names differently. Without this they
/// load fine and then never fire.
const ALIASES: &[(&str, &str)] = &[
    ("esc", "escape"),
    ("return", "enter"),
    ("del", "delete"),
    ("ins", "insert"),
    ("pgup", "pageup"),
    ("pgdn", "pagedown"),
    ("pgdown", "pagedown"),
    ("arrowleft", "left"),
    ("arrowright", "right"),
    ("arrowup", "up"),
    ("arrowdown", "down"),
];

fn section_of(binding: &KeyBinding) -> Option<Section> {
    let context = binding.predicate()?.to_string();
    Section::ALL.into_iter().find(|s| s.context() == context)
}

/// The rules every binding follows, built-in or from `keys.toml`.
fn check(
    section: Section,
    action: &dyn Action,
    keys: &[KeybindingKeystroke],
) -> Result<(), String> {
    let home = actions()
        .into_iter()
        .find(|(_, a)| a.partial_eq(action))
        .map(|(home, _)| home)
        .ok_or("that action can't have a shortcut")?;
    if !section.reaches(home) {
        return Err(format!(
            "{} only works in [{}]",
            short_name(action),
            home.name()
        ));
    }
    // Anywhere but the canvas a text field might have focus, and the key
    // would never reach it.
    if section != Section::Canvas && keys.iter().any(types_a_character) {
        return Err("keys that type a character only work in [canvas]".into());
    }
    Ok(())
}

fn types_a_character(k: &KeybindingKeystroke) -> bool {
    let m = k.modifiers();
    !m.control && !m.alt && !m.platform && (k.key().chars().count() == 1 || k.key() == "space")
}

/// The shortcut for `action` as people read it, such as "Ctrl+Shift+P".
pub fn hint(action: &dyn Action, cx: &App) -> Option<String> {
    hint_in(&cx.key_bindings().borrow(), action)
}

/// Only a key that really runs `action` from where focus usually is counts:
/// one taken by another action, or bound only in [text], would mislead. Of
/// those, the newest wins, so the person's own key beats the default.
fn hint_in(keymap: &Keymap, action: &dyn Action) -> Option<String> {
    let (home, _) = actions().into_iter().find(|(_, a)| a.partial_eq(action))?;
    let path = home.focus_path();
    let binding = keymap.bindings_for_action(action).rfind(|b| {
        let (winners, _) = keymap.bindings_for_input(b.keystrokes(), &path);
        winners
            .first()
            .is_some_and(|w| w.action().partial_eq(action))
    })?;
    let parts: Vec<String> = binding
        .keystrokes()
        .iter()
        .map(|k| {
            let m = k.modifiers();
            let mut s = String::new();
            for (on, name) in [
                (m.control, "Ctrl+"),
                (m.alt, "Alt+"),
                (m.platform, "Super+"),
                (m.shift, "Shift+"),
            ] {
                if on {
                    s.push_str(name);
                }
            }
            s.push_str(&key_name(k.key()));
            s
        })
        .collect();
    Some(parts.join(" "))
}

fn key_name(key: &str) -> String {
    match key {
        "escape" => "Esc".into(),
        "delete" => "Del".into(),
        "enter" => "Enter".into(),
        "tab" => "Tab".into(),
        "space" => "Space".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "pageup" => "Page Up".into(),
        "pagedown" => "Page Down".into(),
        key if key.chars().count() == 1 => key.to_uppercase(),
        key => key.into(),
    }
}

/// The reminder shown while a project is focused, built from whatever is
/// bound right now. Anything left without a key drops out of the line.
pub fn canvas_hint(cx: &App) -> String {
    let h = |a: &dyn Action| hint(a, cx);
    let moves: Vec<String> = [&MoveLeft as &dyn Action, &MoveUp, &MoveDown, &MoveRight]
        .into_iter()
        .filter_map(h)
        .collect();
    let follow: Vec<String> = [&FollowOut as &dyn Action, &FollowIn]
        .into_iter()
        .filter_map(h)
        .collect();
    let mut parts = Vec::new();
    if moves == ["←", "↑", "↓", "→"] {
        parts.push("Arrows move".to_string());
    } else if !moves.is_empty() {
        parts.push(format!("{} move", moves.join(" ")));
    }
    parts.extend(h(&Open).map(|k| format!("{k} opens")));
    parts.extend(h(&EditTask).map(|k| format!("{k} edits")));
    parts.extend(h(&NewTask).map(|k| format!("{k} adds a task")));
    if !follow.is_empty() {
        parts.push(format!("{} follow links", follow.join(" / ")));
    }
    parts.extend(h(&Back).map(|k| format!("{k} goes back")));
    parts.join(" · ")
}

/// The palette's footer, the same way.
pub fn picker_hint(cx: &App) -> String {
    let h = |a: &dyn Action| hint(a, cx);
    let steps: Vec<String> = [&PickerUp as &dyn Action, &PickerDown]
        .into_iter()
        .filter_map(h)
        .collect();
    let mut parts = Vec::new();
    if !steps.is_empty() {
        parts.push(format!("{} move", steps.join(" ")));
    }
    parts.extend(h(&PickerConfirm).map(|k| format!("{k} chooses")));
    parts.extend(h(&PickerDismiss).map(|k| format!("{k} closes")));
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyContext, Keystroke};

    fn context(b: &KeyBinding) -> String {
        b.predicate().map(|p| p.to_string()).unwrap_or_default()
    }

    fn keys(b: &KeyBinding) -> String {
        b.keystrokes()
            .iter()
            .map(|k| k.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// What GPUI would run for `key` with focus inside `stack`.
    fn runs(keymap: &Keymap, key: &str, stack: &[&str]) -> Option<&'static str> {
        let stack: Vec<KeyContext> = stack
            .iter()
            .map(|c| KeyContext::parse(c).unwrap())
            .collect();
        let (found, _) = keymap.bindings_for_input(&[Keystroke::parse(key).unwrap()], &stack);
        found.first().map(|b| short_name(b.action()))
    }

    #[test]
    fn no_two_bindings_in_one_context_share_keys() {
        let all = bindings();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert!(
                    context(a) != context(b) || keys(a) != keys(b),
                    "{} in {:?} is bound twice",
                    keys(a),
                    context(a)
                );
            }
        }
    }

    #[test]
    fn the_defaults_follow_the_rules_a_keys_file_is_held_to() {
        // So typing into Find never trips a shortcut, and no binding sits
        // where its handler can't hear it.
        for b in bindings() {
            let section = section_of(&b).expect("every default has a section");
            let checked = check(section, b.action(), b.keystrokes());
            assert!(
                checked.is_ok(),
                "{} in {:?}: {checked:?}",
                keys(&b),
                context(&b)
            );
        }
    }

    #[test]
    fn a_keys_file_adds_moves_and_removes_shortcuts() {
        let custom = parse(
            r#"
            [window]
            ctrl-k = "OpenPalette"
            ctrl-shift-p = false

            [canvas]
            h = "MoveLeft"
            o = false
            "shift-o" = "FollowOut"

            [text]
            ctrl-h = "Backspace"
            ctrl-p = "Find"
            "#,
        );
        assert_eq!(custom.problems, Vec::<String>::new());
        let mut all = bindings();
        all.extend(custom.bindings);
        let keymap = Keymap::new(all);
        let canvas = ["Hyakume", "Canvas"];
        let field = ["Hyakume", "Picker", "TextInput"];

        assert_eq!(runs(&keymap, "h", &canvas), Some("MoveLeft"));
        assert_eq!(runs(&keymap, "left", &canvas), Some("MoveLeft"));
        assert_eq!(runs(&keymap, "o", &canvas), None);
        assert_eq!(runs(&keymap, "shift-o", &canvas), Some("FollowOut"));
        assert_eq!(runs(&keymap, "ctrl-k", &field), Some("OpenPalette"));
        assert_eq!(runs(&keymap, "ctrl-shift-p", &canvas), None);
        assert_eq!(runs(&keymap, "ctrl-h", &field), Some("Backspace"));
        // Canvas keys stay off while someone types.
        assert_eq!(runs(&keymap, "h", &field), None);

        // Hints follow the file, so the palette never shows a dead key.
        assert_eq!(hint_in(&keymap, &OpenPalette).as_deref(), Some("Ctrl+K"));
        assert_eq!(hint_in(&keymap, &MoveLeft).as_deref(), Some("H"));
        assert_eq!(hint_in(&keymap, &FollowOut).as_deref(), Some("Shift+O"));
        assert_eq!(hint_in(&keymap, &FollowIn).as_deref(), Some("I"));
        // Ctrl+P only works in a text field, so the header keeps Ctrl+F.
        assert_eq!(hint_in(&keymap, &Find).as_deref(), Some("Ctrl+F"));
    }

    #[test]
    fn a_hint_never_names_a_key_another_action_took() {
        let custom = parse(
            r#"
            [window]
            ctrl-f = "OpenPalette"

            [canvas]
            ctrl-0 = "Relayout"
            "#,
        );
        assert_eq!(custom.problems, Vec::<String>::new());
        let mut all = bindings();
        all.extend(custom.bindings);
        let keymap = Keymap::new(all);

        assert_eq!(hint_in(&keymap, &OpenPalette).as_deref(), Some("Ctrl+F"));
        assert_eq!(hint_in(&keymap, &Find), None);
        // Over the canvas Ctrl+0 now re-lays out, so Fit has no key to show.
        assert_eq!(hint_in(&keymap, &Relayout).as_deref(), Some("Ctrl+0"));
        assert_eq!(hint_in(&keymap, &Fit), None);
        // Untouched actions keep their defaults.
        assert_eq!(hint_in(&keymap, &ZoomIn).as_deref(), Some("Ctrl+="));
        assert_eq!(hint_in(&keymap, &PickerDown).as_deref(), Some("↓"));
    }

    #[test]
    fn a_bad_entry_is_skipped_with_a_reason_and_the_rest_still_load() {
        let custom = parse(
            r#"
            stray-key = "Find"

            [window]
            ctrl-k = "OpenPalete"
            g = "Find"
            ctrl-m = "MoveLeft"
            ctrl-j = 3
            ctrl-y = false
            esc = "Back"

            [canvas]
            "ctrl-shft-p" = "Find"
            j = "MoveDown"
            esc = false

            [sidebar]
            ctrl-b = "Find"
            "#,
        );
        let bad = [
            "ctrl-k",
            "g",
            "ctrl-m",
            "ctrl-j",
            "ctrl-y",
            "esc",
            "ctrl-shft-p",
            "[canvas] \"esc\"",
            "[sidebar]",
            "stray-key",
        ];
        assert_eq!(custom.problems.len(), bad.len(), "{:#?}", custom.problems);
        for key in bad {
            assert!(
                custom.problems.iter().any(|p| p.contains(key)),
                "nothing says what's wrong with {key}: {:#?}",
                custom.problems
            );
        }
        // Unbinding a misspelt key gets the spelling fix, not "nothing is bound".
        let esc = custom
            .problems
            .iter()
            .find(|p| p.starts_with("[canvas] \"esc\""));
        assert!(esc.is_some_and(|p| p.contains("\"escape\"")), "{esc:?}");
        let kept: Vec<_> = custom
            .bindings
            .iter()
            .map(|b| short_name(b.action()))
            .collect();
        assert_eq!(kept, ["MoveDown"]);

        // The mistake starts its line, which is where off-by-ones hide.
        let broken = parse("[canvas]\nh = \"MoveLeft\"\n= \"MoveUp\"\n");
        assert!(broken.bindings.is_empty());
        assert!(
            broken.problems[0].starts_with("line 3: "),
            "{:?}",
            broken.problems
        );
    }
}
