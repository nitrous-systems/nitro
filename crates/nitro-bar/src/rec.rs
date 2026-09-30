//! Screen recording (#676 C): the red **REC** indicator on every panel,
//! and the prompt that asks the user whether a program may record.
//!
//! The server owns both decisions. It sends
//! [`ShellEvent::CaptureState`] whenever what is recorded changes (the
//! indicator follows it and nothing else), and
//! [`ShellEvent::CapturePrompt`] when a program on `capture.allow` asks
//! to record. The bar answers the prompt with one of three buttons —
//! Deny, Allow once, Allow for this session — via
//! [`Ui::capture_answer`]. Subscribing is one `CaptureAnswer { Watch }`
//! at start-up; after that everything arrives unasked, so an idle bar
//! still schedules nothing.
//!
//! The prompt is a popup **without** the pointer grab: it stays until it
//! is answered, and a press elsewhere does not silently decide. Closing
//! it any other way (the server dismissing it) is a Deny. Prompts that
//! arrive while one is showing queue behind it.

use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::quick::{QS_GAP, QS_PAD, QS_RADIUS, QS_WIDTH};
use nitro_ui::shell::CaptureAnswerKind;
use nitro_ui::widgets::{button, label, panel, row};
use nitro_ui::{ColorRole, CrossAlign, PopupPlacement, Ui, WidgetId, WindowId};

use crate::Bar;

/// The `hey`-addressable names.
pub mod names {
    /// The REC indicator on every panel.
    pub const INDICATOR: &str = "rec";
    /// The prompt popup's root.
    pub const PROMPT: &str = "capture_prompt";
    /// The prompt's question.
    pub const QUESTION: &str = "capture_question";
    /// Deny.
    pub const DENY: &str = "capture_deny";
    /// Allow this one recording.
    pub const ALLOW_ONCE: &str = "capture_allow_once";
    /// Allow this program until logout.
    pub const ALLOW_SESSION: &str = "capture_allow_session";
}

/// One question from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    /// The server's id for it.
    pub request: u32,
    /// The output to be recorded.
    pub output: u32,
    /// The program's executable name.
    pub client_name: String,
}

/// The recording state, inside [`Bar`].
#[derive(Debug, Default)]
pub struct Rec {
    /// Whether the server says something is being recorded.
    active: bool,
    /// The captured outputs' mask, as the server sent it.
    outputs_mask: u32,
    /// The question on screen, and its popup.
    showing: Option<(Ask, WindowId)>,
    /// Questions waiting behind it.
    queue: Vec<Ask>,
    /// Answers sent, for the tests.
    answers: Vec<(u32, CaptureAnswerKind)>,
}

impl Rec {
    /// Whether the indicator is on.
    #[must_use]
    pub fn active(&self) -> bool {
        self.active
    }

    /// The captured outputs' mask.
    #[must_use]
    pub fn outputs_mask(&self) -> u32 {
        self.outputs_mask
    }

    /// The question on screen.
    #[must_use]
    pub fn showing(&self) -> Option<&Ask> {
        self.showing.as_ref().map(|(a, _)| a)
    }

    /// The prompt popup, if one is open.
    #[must_use]
    pub fn popup(&self) -> Option<WindowId> {
        self.showing.as_ref().map(|(_, w)| *w)
    }

    /// Every answer sent, in order.
    #[must_use]
    pub fn answers(&self) -> &[(u32, CaptureAnswerKind)] {
        &self.answers
    }
}

/// Build one panel's indicator: a red "● REC", collapsed (no space
/// taken) until something is recorded.
pub(crate) fn indicator(ui: &mut Ui<Bar>, active: bool) -> WidgetId {
    ui.build(
        label("● REC")
            .name(names::INDICATOR)
            .size(13.0)
            .color_role(ColorRole::Danger)
            .collapsed(!active),
    )
}

/// Subscribe to prompts and the recording state.
pub(crate) fn subscribe(ui: &mut Ui<Bar>) {
    if let Err(e) = ui.capture_answer(0, CaptureAnswerKind::Watch) {
        eprintln!("nitro-bar: capture watch: {e}");
    }
}

/// `CaptureState`: show or hide the indicator on every panel.
pub(crate) fn state(s: &mut Bar, ui: &mut Ui<Bar>, active: bool, outputs_mask: u32) {
    s.rec.outputs_mask = outputs_mask;
    if s.rec.active == active {
        return;
    }
    s.rec.active = active;
    for p in &s.panels {
        ui.set_collapsed(p.ids.rec, !active);
    }
}

/// `CapturePrompt`: ask now, or after the question on screen.
pub(crate) fn prompt(s: &mut Bar, ui: &mut Ui<Bar>, ask: Ask) {
    if s.rec.showing.is_some() {
        s.rec.queue.push(ask);
        return;
    }
    show(s, ui, ask);
}

fn show(s: &mut Bar, ui: &mut Ui<Bar>, ask: Ask) {
    let Some(main) = s.panels.first() else {
        // No panel yet to hang it on: the answer the server would give
        // with no shell at all.
        send(s, ui, ask.request, CaptureAnswerKind::Deny);
        return;
    };
    let parent = main.win;
    let anchor = ui.window_bounds(main.ids.clock);
    let root = build(ui, &ask);
    let placement = PopupPlacement::below(anchor).grab(false);
    match ui.add_popup(parent, placement, None, root) {
        Ok(win) => {
            let request = ask.request;
            s.rec.showing = Some((ask, win));
            ui.on_window_closed(win, move |s: &mut Bar, ui: &mut Ui<Bar>| {
                // Closed without an answer (dismissed by the server): no.
                if s.rec
                    .showing
                    .as_ref()
                    .is_some_and(|(a, _)| a.request == request)
                {
                    s.rec.showing = None;
                    send(s, ui, request, CaptureAnswerKind::Deny);
                    next(s, ui);
                }
            });
            if ui.is_shell() {
                let _ = ui.grab_keyboard_of(win, true);
            }
        }
        Err(e) => {
            eprintln!("nitro-bar: capture prompt: {e}");
            let _ = ui.remove(root);
            send(s, ui, ask.request, CaptureAnswerKind::Deny);
        }
    }
}

fn build(ui: &mut Ui<Bar>, ask: &Ask) -> WidgetId {
    let root = ui.build(
        panel()
            .name(names::PROMPT)
            .background_role(ColorRole::WindowBackground)
            .border_role(1.0, ColorRole::Hairline)
            .radius(QS_RADIUS)
            .padding(QS_PAD)
            .gap(QS_GAP)
            .width(QS_WIDTH)
            .cross_align(CrossAlign::Stretch),
    );
    let question = ui.build(
        label(format!("{} wants to record the screen", ask.client_name))
            .name(names::QUESTION)
            .size(13.0)
            .color_role(ColorRole::Text)
            .elide(true),
    );
    let buttons = ui.build(row().gap(8.0).cross_align(CrossAlign::Center));
    for (text, name, answer) in [
        ("Deny", names::DENY, CaptureAnswerKind::Deny),
        (
            "Allow once",
            names::ALLOW_ONCE,
            CaptureAnswerKind::AllowOnce,
        ),
        (
            "Allow this session",
            names::ALLOW_SESSION,
            CaptureAnswerKind::AllowSession,
        ),
    ] {
        let b = ui.build(
            button(text)
                .name(name)
                .size(13.0)
                .on_click(move |s: &mut Bar, ui: &mut Ui<Bar>| answer_shown(s, ui, answer)),
        );
        let _ = ui.attach(buttons, b);
    }
    let _ = ui.attach(root, question);
    let _ = ui.attach(root, buttons);
    root
}

/// A button: answer the question on screen, close it, show the next.
fn answer_shown(s: &mut Bar, ui: &mut Ui<Bar>, answer: CaptureAnswerKind) {
    let Some((ask, win)) = s.rec.showing.take() else {
        return;
    };
    send(s, ui, ask.request, answer);
    // From inside the popup's own callback: removed at the end of the
    // turn, as the quick menu does.
    ui.defer(move |s: &mut Bar, ui: &mut Ui<Bar>| {
        let _ = ui.remove_window(s, win);
        next(s, ui);
    });
}

fn next(s: &mut Bar, ui: &mut Ui<Bar>) {
    if s.rec.showing.is_none() && !s.rec.queue.is_empty() {
        let ask = s.rec.queue.remove(0);
        show(s, ui, ask);
    }
}

fn send(s: &mut Bar, ui: &mut Ui<Bar>, request: u32, answer: CaptureAnswerKind) {
    s.rec.answers.push((request, answer));
    if ui.is_shell()
        && let Err(e) = ui.capture_answer(request, answer)
    {
        eprintln!("nitro-bar: capture answer: {e}");
    }
}
