//! The semantic colour palette: [`Role`] names a *meaning*, [`Palette`]
//! answers with a [`Color`].
//!
//! Every colour on a nitro desktop — the toolkit's buttons, the server's
//! window decorations, the terminal's ANSI table, the wallpaper's
//! gradient — comes from one of the roles below. No app picks a colour:
//! it asks for `Role::Accent` and gets whatever the current scheme says.
//! That is the whole point. A hard-coded `Color::rgb(0x33, 0x88, 0xff)`
//! in an app is a colour the user's light/dark switch cannot reach, and
//! `deploy/lint-colors.sh` fails the build for one.
//!
//! # Why an enum and an array, not a struct of fields
//!
//! A struct of named fields (which is what `nitro_ui::Theme` still is, as
//! a *view* on this) cannot be indexed, iterated, addressed by a
//! configuration key, or sent as a fixed-size wire payload. All four are
//! needed here: `server.conf` names roles by key, the wire sends the
//! whole table as `Role::COUNT` colours, the control socket prints every
//! role, and the tests iterate. The enum is dense and `repr(u8)`, so the
//! array index *is* the wire index.
//!
//! # Adding a role
//!
//! Append it to the `roles!` table below — never insert in the middle,
//! because the position is the wire index. Give it a value in both
//! [`Palette::light`] and [`Palette::dark`] (the table is exhaustive by
//! construction: both constructors list every variant, so a missing one
//! is a compile error). Appending bumps the `Theme` message's `N`; see
//! `docs/wire.md` §Theme and `docs/theme.md`.

use crate::Color;

/// Declare the role table once: the enum, its count, the wire index, and
/// the `snake_case` configuration key each role answers to.
macro_rules! roles {
    ($( $(#[$m:meta])* $variant:ident = $key:literal ),* $(,)?) => {
        /// What a colour is *for*.
        ///
        /// The discriminant is the wire index and the array index; it is
        /// stable, so roles are only ever appended.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(u8)]
        #[allow(missing_docs)] // Each variant carries its own doc below.
        pub enum Role {
            $( $(#[$m])* $variant, )*
        }

        impl Role {
            /// Every role, in wire order.
            pub const ALL: &'static [Role] = &[ $( Role::$variant, )* ];

            /// How many roles there are — the length of a [`Palette`] and
            /// the `N` of the wire's `Theme` message.
            pub const COUNT: usize = Role::ALL.len();

            /// The `server.conf` key this role answers to, without the
            /// `theme.` prefix (`window_background`, `ansi5`).
            #[must_use]
            pub const fn key(self) -> &'static str {
                match self {
                    $( Role::$variant => $key, )*
                }
            }

            /// The role a configuration key names, if any.
            #[must_use]
            pub fn from_key(key: &str) -> Option<Self> {
                match key {
                    $( $key => Some(Role::$variant), )*
                    _ => None,
                }
            }
        }
    };
}

roles! {
    /// The background of an ordinary window or panel.
    WindowBackground = "window_background",
    /// A raised surface sitting on the window background: a card, a
    /// popup, the row area of a list.
    Surface = "surface",
    /// The scrim painted over everything behind a modal dialog.
    /// Translucent by definition — the only role whose alpha matters.
    ModalBackground = "modal_background",
    /// Default text.
    Text = "text",
    /// Secondary text: a hint, a units suffix, a disabled label.
    TextDim = "text_dim",
    /// Text drawn on top of [`Role::Accent`].
    TextOnAccent = "text_on_accent",
    /// Text on a button face.
    ButtonText = "button_text",
    /// The greyed prompt in an empty text field.
    Placeholder = "placeholder",
    /// The one colour that says "this is the interesting thing": a
    /// checked checkbox, a filled slider track, a selected row.
    Accent = "accent",
    /// Accent under the pointer.
    AccentHover = "accent_hover",
    /// Accent while pressed.
    AccentActive = "accent_active",
    /// A button's face.
    Button = "button",
    /// A button's face under the pointer.
    ButtonHover = "button_hover",
    /// A button's face while pressed.
    ButtonActive = "button_active",
    /// A disabled button's face.
    ButtonDisabled = "button_disabled",
    /// The outline of a panel, a button or a field.
    Border = "border",
    /// The focus ring.
    Focus = "focus",
    /// Selected text's highlight.
    Selection = "selection",
    /// The background of an editable field.
    Field = "field",
    /// The caret in a text field.
    Caret = "caret",
    /// The unfilled part of a slider track, and a separator line.
    Track = "track",
    /// Destructive: a delete button, an error message.
    Danger = "danger",
    /// Something is off but nothing is lost.
    Warning = "warning",
    /// It worked.
    Success = "success",
    /// The title bar of the focused window (server-drawn).
    TitleBarActive = "title_bar_active",
    /// The title bar of an unfocused window (server-drawn).
    TitleBarInactive = "title_bar_inactive",
    /// Title text of the focused window.
    TitleTextActive = "title_text_active",
    /// Title text of an unfocused window.
    TitleTextInactive = "title_text_inactive",
    /// The frame border of the focused window: **a shade of that
    /// window's title bar**, not an unrelated accent.
    ///
    /// The border and the title bar are one continuous shape since #3724
    /// (`docs/wm.md`), and the eye reads a frame as one thing or as two.
    /// Before #3724 this was a mid blue against a pale-blue bar, which is
    /// two: the box reported "a frame around the bottom left and right
    /// window sides… a bit wider than the title bar, and a different
    /// color". It is now the bar's own colour **darkened** — which is what
    /// a 1-px outline around a filled shape is for — and the *focused*
    /// signal is carried by the title bar, where it covers 28 px of window
    /// rather than one.
    WindowBorderActive = "window_border_active",
    /// The frame border of an unfocused window: `title_bar_inactive`'s
    /// edge, by the same rule.
    WindowBorderInactive = "window_border_inactive",
    /// The close button on a title bar: since #3715 its **hover**
    /// background, the disc that appears under the `x` glyph when the
    /// pointer is on it.
    ///
    /// It used to be the button's resting face — a red circle, painted
    /// whether or not anyone was pointing at it. The buttons are symbolic
    /// icons now (`docs/wm.md`), so the red moved from "what a close
    /// button looks like" to "what it looks like when a click would close
    /// the window", which is where every other desktop puts it.
    TitleClose = "title_close",
    /// The maximize button on a title bar. **No longer painted**: the
    /// maximize button is a `square` glyph on the same hover background as
    /// minimize since #3715, and there is no green circle left to colour.
    ///
    /// Kept because a role index *is* a wire index (see the module docs):
    /// removing one would renumber every role after it and silently
    /// re-colour a client one release behind. An unused role costs four
    /// bytes in the `Theme` message and nothing else.
    TitleMaximize = "title_maximize",
    /// Top of the desktop's wallpaper gradient.
    DesktopTop = "desktop_top",
    /// Bottom of the desktop's wallpaper gradient.
    DesktopBottom = "desktop_bottom",
    /// A terminal's default background (`SGR 49`).
    TerminalBackground = "terminal_background",
    /// A terminal's default foreground (`SGR 39`).
    TerminalText = "terminal_text",
    /// A terminal's cursor block.
    TerminalCursor = "terminal_cursor",
    /// ANSI 0: black.
    Ansi0 = "ansi0",
    /// ANSI 1: red.
    Ansi1 = "ansi1",
    /// ANSI 2: green.
    Ansi2 = "ansi2",
    /// ANSI 3: yellow.
    Ansi3 = "ansi3",
    /// ANSI 4: blue.
    Ansi4 = "ansi4",
    /// ANSI 5: magenta.
    Ansi5 = "ansi5",
    /// ANSI 6: cyan.
    Ansi6 = "ansi6",
    /// ANSI 7: white.
    Ansi7 = "ansi7",
    /// ANSI 8: bright black.
    Ansi8 = "ansi8",
    /// ANSI 9: bright red.
    Ansi9 = "ansi9",
    /// ANSI 10: bright green.
    Ansi10 = "ansi10",
    /// ANSI 11: bright yellow.
    Ansi11 = "ansi11",
    /// ANSI 12: bright blue.
    Ansi12 = "ansi12",
    /// ANSI 13: bright magenta.
    Ansi13 = "ansi13",
    /// ANSI 14: bright cyan.
    Ansi14 = "ansi14",
    /// ANSI 15: bright white.
    Ansi15 = "ansi15",
    /// The frame edge a window can be resized by, while the pointer is in
    /// its grab band.
    ///
    /// The resize band is six logical pixels wide and the border it
    /// straddles is one, so without something to see there is nothing to
    /// tell a user where to press — they grab the visible border, miss by
    /// three pixels, and conclude that resizing does not work (#3713).
    /// The cursor shape says *that* the pointer is over a resize band;
    /// only the edge lighting up says *which* edge, so the highlight
    /// stays even now that shapes exist (`docs/wm.md`). It is a role and
    /// not a tint of `window_border_active` because it has to read
    /// clearly against both border colours, both title bars, the desktop
    /// and the window background — everything a 1-px stroke can sit
    /// beside — at 3:1 or better. Both schemes test that.
    ResizeHint = "resize_hint",
    /// The disc under a title-bar button while the pointer is on it.
    ///
    /// The frame's buttons are symbolic glyphs on a background that is
    /// **transparent until hovered** (`docs/wm.md`), so this role is the
    /// whole of the "you are on the button" affordance for minimize and
    /// maximize — close has [`Role::TitleClose`], because a red close is
    /// the one convention every desktop shares.
    ///
    /// It is a role of its own rather than [`Role::ButtonHover`] because
    /// the two sit on different backgrounds: a toolkit button hovers on a
    /// window background, and in the light scheme `button_hover`
    /// (`#d6d9e4`) against `title_bar_active` (`#d6dde8`) is a difference
    /// of two units in one channel — an affordance nobody can see.
    TitleButtonHover = "title_button_hover",
    /// The sidebar pane of a split view (`docs/ui.md`, "Split view
    /// blueprint"): a shade **darker** than [`Role::WindowBackground`]
    /// in the light scheme and a shade **lighter** in the dark one, which
    /// is how both GNOME and macOS make the two panes read as two
    /// without drawing a border between them.
    SidebarBackground = "sidebar_background",
    /// The selected sidebar row's pill. Neutral, not the accent: the
    /// accent is reserved for the control that wants attention, and a
    /// permanently-selected category is not it.
    SidebarSelected = "sidebar_selected",
    /// A sidebar row under the pointer. Its own role for the reason
    /// [`Role::TitleButtonHover`] is: [`Role::ButtonHover`] is chosen
    /// against the window background and is invisible on the sidebar's.
    SidebarHover = "sidebar_hover",
    /// A 1-px separation that is a *shade*, not a border: the line
    /// between the two panes of a split view, the ring around a card and
    /// the lines between a card's rows. Distinct from [`Role::Border`],
    /// which outlines a control and has to clear 3:1.
    Hairline = "hairline",
}

impl Role {
    /// This role's index into a [`Palette`] — and its position on the
    /// wire.
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The role at wire index `index`, or `None` past the end.
    #[must_use]
    pub fn from_index(index: usize) -> Option<Self> {
        Self::ALL.get(index).copied()
    }

    /// The ANSI role for palette index 0–15.
    ///
    /// # Panics
    /// Never: the sixteen ANSI roles are contiguous and the argument is
    /// masked to four bits.
    #[must_use]
    pub fn ansi(index: u8) -> Self {
        let i = Self::Ansi0.index() + (index & 0x0f) as usize;
        Self::from_index(i).expect("the sixteen ANSI roles are contiguous")
    }
}

/// Which of the two built-in schemes a palette started from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Scheme {
    /// Dark text on light surfaces. The default: it is what every
    /// screenshot in the docs shows.
    #[default]
    Light,
    /// Light text on dark surfaces.
    Dark,
}

impl Scheme {
    /// The name in `server.conf` (`light`, `dark`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// Parse a scheme name, case-insensitively.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    /// The palette this scheme names.
    #[must_use]
    pub fn palette(self) -> Palette {
        match self {
            Self::Light => Palette::light(),
            Self::Dark => Palette::dark(),
        }
    }
}

/// One colour per [`Role`].
///
/// Cheap to clone (four bytes a role, so ~200), compared by value, and
/// sent whole: a palette change is one wire message and one repaint,
/// never a negotiation.
///
/// The size is written as the arithmetic rather than as a number because
/// the number moves: appending a role grows it by four bytes, and a stale
/// literal here is the sort of comment that quietly stops being true.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Palette([Color; Role::COUNT]);

impl Default for Palette {
    /// [`Palette::light`] — see [`Scheme::Light`].
    fn default() -> Self {
        Self::light()
    }
}

impl Palette {
    /// A palette from a full table in wire order.
    #[must_use]
    pub const fn from_colors(colors: [Color; Role::COUNT]) -> Self {
        Self(colors)
    }

    /// The colour of `role`.
    #[must_use]
    pub const fn get(&self, role: Role) -> Color {
        self.0[role as usize]
    }

    /// Override one role.
    pub const fn set(&mut self, role: Role, color: Color) {
        self.0[role as usize] = color;
    }

    /// The whole table in wire order.
    #[must_use]
    pub const fn colors(&self) -> &[Color; Role::COUNT] {
        &self.0
    }

    /// Every `(role, colour)` pair, in wire order.
    pub fn iter(&self) -> impl Iterator<Item = (Role, Color)> + '_ {
        Role::ALL.iter().map(|r| (*r, self.get(*r)))
    }

    /// The colour of ANSI palette index 0–255.
    ///
    /// 0–15 are the [`Role::ansi`] table; 16–231 are xterm's 6×6×6 cube
    /// and 232–255 its greyscale ramp, both computed rather than stored
    /// because they are defined by arithmetic that every program
    /// emitting `38;5;n` assumes.
    #[must_use]
    pub fn ansi_indexed(&self, index: u8) -> Color {
        match index {
            0..=15 => self.get(Role::ansi(index)),
            16..=231 => {
                let n = index - 16;
                let (red, green, blue) = (n / 36, (n % 36) / 6, n % 6);
                Color::rgb(cube(red), cube(green), cube(blue))
            }
            232..=255 => {
                // The ramp is 24 steps from near-black to near-white,
                // deliberately excluding both ends: 0 and 255 are
                // already in the sixteen.
                let v = 8 + (index - 232) * 10;
                Color::rgb(v, v, v)
            }
        }
    }

    /// The light scheme: dark ink on paper.
    ///
    /// The default, and deliberately: every screenshot in `docs/` was
    /// taken on it, and a desktop that changes its whole look because a
    /// configuration file is missing is a desktop that cannot be
    /// supported over the phone.
    #[must_use]
    pub fn light() -> Self {
        use Role::{
            Accent, AccentActive, AccentHover, Ansi0, Ansi1, Ansi2, Ansi3, Ansi4, Ansi5, Ansi6,
            Ansi7, Ansi8, Ansi9, Ansi10, Ansi11, Ansi12, Ansi13, Ansi14, Ansi15, Border, Button,
            ButtonActive, ButtonDisabled, ButtonHover, ButtonText, Caret, Danger, DesktopBottom,
            DesktopTop, Field, Focus, Hairline, ModalBackground, Placeholder, ResizeHint,
            Selection, SidebarBackground, SidebarHover, SidebarSelected, Success,
            TerminalBackground, TerminalCursor, TerminalText, Text, TextDim, TextOnAccent,
            TitleBarActive, TitleBarInactive, TitleButtonHover, TitleClose, TitleMaximize,
            TitleTextActive, TitleTextInactive, Track, Warning, WindowBackground,
            WindowBorderActive, WindowBorderInactive,
        };
        let mut p = Self([Color::BLACK; Role::COUNT]);
        // Chrome. These are `nitro_ui::Theme::default()`'s values, which
        // is why converting the toolkit to roles changed no pixel.
        p.set(WindowBackground, Color::rgb(0xf2, 0xf2, 0xf2));
        p.set(Role::Surface, Color::rgb(0xff, 0xff, 0xff));
        p.set(ModalBackground, Color::rgba(0x20, 0x20, 0x24, 0x99));
        p.set(Text, Color::rgb(0x1a, 0x1a, 0x1a));
        p.set(TextDim, Color::rgb(0x5e, 0x5e, 0x5e));
        p.set(TextOnAccent, Color::rgb(0xff, 0xff, 0xff));
        p.set(ButtonText, Color::rgb(0x12, 0x12, 0x16));
        p.set(Placeholder, Color::rgb(0x6b, 0x6b, 0x6b));
        p.set(Accent, Color::rgb(0x0f, 0x5f, 0xbe));
        p.set(AccentHover, Color::rgb(0x12, 0x6e, 0xd8));
        p.set(AccentActive, Color::rgb(0x0b, 0x4c, 0x99));
        p.set(Button, Color::rgb(0xe4, 0xe4, 0xe8));
        p.set(ButtonHover, Color::rgb(0xd6, 0xd9, 0xe4));
        p.set(ButtonActive, Color::rgb(0xc0, 0xc6, 0xd8));
        p.set(ButtonDisabled, Color::rgb(0xec, 0xec, 0xee));
        p.set(Border, Color::rgb(0xc2, 0xc2, 0xc8));
        p.set(Focus, Color::rgb(0x0f, 0x5f, 0xbe));
        p.set(Selection, Color::rgb(0xb3, 0xd4, 0xff));
        p.set(Field, Color::rgb(0xff, 0xff, 0xff));
        p.set(Caret, Color::rgb(0x1a, 0x1a, 0x1a));
        p.set(Track, Color::rgb(0xd4, 0xd4, 0xda));
        p.set(Danger, Color::rgb(0xb3, 0x26, 0x1a));
        p.set(Warning, Color::rgb(0x8a, 0x55, 0x00));
        p.set(Success, Color::rgb(0x1e, 0x6b, 0x2c));
        // Decorations. Light title bars, dark title text.
        p.set(TitleBarActive, Color::rgb(0xd6, 0xdd, 0xe8));
        p.set(TitleBarInactive, Color::rgb(0xea, 0xec, 0xf0));
        p.set(TitleTextActive, Color::rgb(0x17, 0x1c, 0x24));
        p.set(TitleTextInactive, Color::rgb(0x55, 0x5c, 0x66));
        // The border is the *bar's* edge, not a second colour: each is its
        // own title bar darkened enough to read as an outline against the
        // desktop behind it. See `Role::WindowBorderActive`. `#d6dde8` and
        // `#eaecf0` darken to these.
        p.set(WindowBorderActive, Color::rgb(0x8c, 0x9a, 0xae));
        p.set(WindowBorderInactive, Color::rgb(0xb6, 0xbb, 0xc4));
        p.set(TitleClose, Color::rgb(0xd9, 0x5b, 0x4e));
        p.set(TitleMaximize, Color::rgb(0x62, 0xa8, 0x5c));
        // A hover disc on a *light* title bar: darker than both bars, and
        // distinctly bluer than the neutral `button_hover` so the two
        // cannot be confused when they sit side by side on a dialog.
        p.set(TitleButtonHover, Color::rgb(0xb3, 0xc0, 0xd4));
        // The resize affordance. Deliberately *not* the accent: the
        // focused border is a blue shade of a blue bar, and an
        // accent-blue hint over it (`#0f5fbe`, 2.2:1) was a shade shift
        // the box could not see at arm's length (#565). Navy clears 3:1
        // against every colour that can be adjacent to the stroke —
        // both borders, both bars, the desktop, the window background.
        p.set(ResizeHint, Color::rgb(0x00, 0x3a, 0x80));
        // Split view. See docs/ui.md §Split view blueprint. The sidebar
        // is a shade *below* the window background, the pill and hover
        // a shade below that, and the hairline sits between the sidebar
        // and the white surface so it reads on both.
        p.set(SidebarBackground, Color::rgb(0xeb, 0xeb, 0xed));
        p.set(SidebarSelected, Color::rgb(0xd8, 0xd8, 0xdc));
        p.set(SidebarHover, Color::rgb(0xdf, 0xdf, 0xe3));
        p.set(Hairline, Color::rgb(0xdc, 0xdc, 0xe0));
        // Desktop.
        p.set(DesktopTop, Color::rgb(0xdc, 0xe3, 0xed));
        p.set(DesktopBottom, Color::rgb(0xbe, 0xc7, 0xd4));
        // Terminal. A light terminal needs *darkened* ANSI colours: the
        // familiar saturated ones are chosen for a dark background and
        // `ls --color`'s blue directory is unreadable on paper.
        p.set(TerminalBackground, Color::rgb(0xfb, 0xfb, 0xf8));
        p.set(TerminalText, Color::rgb(0x1c, 0x1c, 0x1c));
        p.set(TerminalCursor, Color::rgb(0x1c, 0x1c, 0x1c));
        p.set(Ansi0, Color::rgb(0x2b, 0x2b, 0x2b));
        p.set(Ansi1, Color::rgb(0xa3, 0x1d, 0x1d));
        p.set(Ansi2, Color::rgb(0x2b, 0x66, 0x1f));
        p.set(Ansi3, Color::rgb(0x7a, 0x54, 0x00));
        p.set(Ansi4, Color::rgb(0x1b, 0x4f, 0xa8));
        p.set(Ansi5, Color::rgb(0x82, 0x28, 0x9c));
        p.set(Ansi6, Color::rgb(0x0f, 0x63, 0x6b));
        p.set(Ansi7, Color::rgb(0x55, 0x55, 0x55));
        p.set(Ansi8, Color::rgb(0x6e, 0x6e, 0x6e));
        p.set(Ansi9, Color::rgb(0xc4, 0x2b, 0x1c));
        p.set(Ansi10, Color::rgb(0x35, 0x7d, 0x24));
        p.set(Ansi11, Color::rgb(0x8f, 0x66, 0x00));
        p.set(Ansi12, Color::rgb(0x1f, 0x61, 0xc4));
        p.set(Ansi13, Color::rgb(0x9b, 0x31, 0xba));
        p.set(Ansi14, Color::rgb(0x11, 0x75, 0x7f));
        p.set(Ansi15, Color::rgb(0x1c, 0x1c, 0x1c));
        p
    }

    /// The dark scheme: light ink on slate.
    ///
    /// The ANSI half is the table `nitro-term` shipped before roles
    /// existed, so a terminal on the dark scheme looks exactly as it did.
    #[must_use]
    pub fn dark() -> Self {
        use Role::{
            Accent, AccentActive, AccentHover, Ansi0, Ansi1, Ansi2, Ansi3, Ansi4, Ansi5, Ansi6,
            Ansi7, Ansi8, Ansi9, Ansi10, Ansi11, Ansi12, Ansi13, Ansi14, Ansi15, Border, Button,
            ButtonActive, ButtonDisabled, ButtonHover, ButtonText, Caret, Danger, DesktopBottom,
            DesktopTop, Field, Focus, Hairline, ModalBackground, Placeholder, ResizeHint,
            Selection, SidebarBackground, SidebarHover, SidebarSelected, Success,
            TerminalBackground, TerminalCursor, TerminalText, Text, TextDim, TextOnAccent,
            TitleBarActive, TitleBarInactive, TitleButtonHover, TitleClose, TitleMaximize,
            TitleTextActive, TitleTextInactive, Track, Warning, WindowBackground,
            WindowBorderActive, WindowBorderInactive,
        };
        let mut p = Self([Color::BLACK; Role::COUNT]);
        p.set(WindowBackground, Color::rgb(0x1e, 0x20, 0x26));
        p.set(Role::Surface, Color::rgb(0x27, 0x2a, 0x32));
        p.set(ModalBackground, Color::rgba(0x08, 0x09, 0x0c, 0xb0));
        p.set(Text, Color::rgb(0xe6, 0xe8, 0xec));
        p.set(TextDim, Color::rgb(0xa2, 0xa8, 0xb2));
        // The accent is light, so what sits on it is *dark*. The two
        // schemes disagree about this, which is exactly why it is a role
        // and not a constant.
        p.set(TextOnAccent, Color::rgb(0x0c, 0x10, 0x16));
        p.set(ButtonText, Color::rgb(0xe6, 0xe8, 0xec));
        p.set(Placeholder, Color::rgb(0x90, 0x96, 0xa0));
        p.set(Accent, Color::rgb(0x6c, 0xa8, 0xf0));
        p.set(AccentHover, Color::rgb(0x8a, 0xbc, 0xf6));
        p.set(AccentActive, Color::rgb(0x52, 0x8e, 0xd6));
        p.set(Button, Color::rgb(0x33, 0x38, 0x42));
        p.set(ButtonHover, Color::rgb(0x3e, 0x44, 0x50));
        p.set(ButtonActive, Color::rgb(0x4b, 0x53, 0x61));
        p.set(ButtonDisabled, Color::rgb(0x26, 0x29, 0x30));
        p.set(Border, Color::rgb(0x44, 0x4a, 0x56));
        p.set(Focus, Color::rgb(0x6c, 0xa8, 0xf0));
        p.set(Selection, Color::rgb(0x2d, 0x4c, 0x74));
        p.set(Field, Color::rgb(0x16, 0x18, 0x1d));
        p.set(Caret, Color::rgb(0xe6, 0xe8, 0xec));
        p.set(Track, Color::rgb(0x3a, 0x40, 0x4a));
        p.set(Danger, Color::rgb(0xf2, 0x6d, 0x63));
        p.set(Warning, Color::rgb(0xe3, 0xb3, 0x41));
        p.set(Success, Color::rgb(0x6f, 0xcf, 0x6a));
        p.set(TitleBarActive, Color::rgb(0x2c, 0x3e, 0x55));
        p.set(TitleBarInactive, Color::rgb(0x23, 0x2a, 0x33));
        p.set(TitleTextActive, Color::rgb(0xf0, 0xf4, 0xf8));
        p.set(TitleTextInactive, Color::rgb(0x9a, 0xa4, 0xb0));
        // Each border is its own title bar, lightened rather than darkened
        // — a dark bar's edge has to come *up* to be visible against a dark
        // desktop. `#2c3e55` and `#232a33` give these.
        p.set(WindowBorderActive, Color::rgb(0x4d, 0x67, 0x88));
        p.set(WindowBorderInactive, Color::rgb(0x39, 0x42, 0x4e));
        p.set(TitleClose, Color::rgb(0xd9, 0x5b, 0x4e));
        p.set(TitleMaximize, Color::rgb(0x62, 0xa8, 0x5c));
        p.set(TitleButtonHover, Color::rgb(0x46, 0x5c, 0x78));
        // Not the accent, for the light scheme's reason: `#6ca8f0` on the
        // active border was 2.3:1, a shade of the same blue (#565). Pale
        // sky clears 3:1 against everything beside the stroke.
        p.set(ResizeHint, Color::rgb(0xb8, 0xdc, 0xff));
        // Split view: the sidebar is a shade *above* the window
        // background here — the light scheme's rule, mirrored.
        p.set(SidebarBackground, Color::rgb(0x2a, 0x2d, 0x34));
        p.set(SidebarSelected, Color::rgb(0x3f, 0x44, 0x50));
        p.set(SidebarHover, Color::rgb(0x35, 0x39, 0x42));
        p.set(Hairline, Color::rgb(0x3a, 0x3f, 0x48));
        p.set(DesktopTop, Color::rgb(0x2a, 0x30, 0x3c));
        p.set(DesktopBottom, Color::rgb(0x15, 0x18, 0x20));
        p.set(TerminalBackground, Color::rgb(0x14, 0x14, 0x18));
        p.set(TerminalText, Color::rgb(0xdc, 0xdc, 0xdc));
        p.set(TerminalCursor, Color::rgb(0xdc, 0xdc, 0xdc));
        p.set(Ansi0, Color::rgb(0x1c, 0x1c, 0x1c));
        p.set(Ansi1, Color::rgb(0xcc, 0x33, 0x3c));
        p.set(Ansi2, Color::rgb(0x5a, 0xb0, 0x38));
        p.set(Ansi3, Color::rgb(0xc8, 0x9b, 0x27));
        p.set(Ansi4, Color::rgb(0x3d, 0x82, 0xd6));
        p.set(Ansi5, Color::rgb(0xa6, 0x53, 0xc4));
        p.set(Ansi6, Color::rgb(0x2f, 0xa8, 0xa8));
        p.set(Ansi7, Color::rgb(0xc8, 0xc8, 0xc8));
        p.set(Ansi8, Color::rgb(0x5c, 0x5c, 0x5c));
        p.set(Ansi9, Color::rgb(0xf2, 0x5b, 0x63));
        p.set(Ansi10, Color::rgb(0x84, 0xd6, 0x5c));
        p.set(Ansi11, Color::rgb(0xf0, 0xc4, 0x4c));
        p.set(Ansi12, Color::rgb(0x67, 0xa8, 0xf0));
        p.set(Ansi13, Color::rgb(0xc9, 0x82, 0xe8));
        p.set(Ansi14, Color::rgb(0x55, 0xd0, 0xd0));
        p.set(Ansi15, Color::rgb(0xf2, 0xf2, 0xf2));
        p
    }
}

/// One axis of the 6×6×6 colour cube. The first step is 0 and the rest
/// are 55 plus multiples of 40 — xterm's table, not a linear ramp, and
/// programs do depend on the exact values.
const fn cube(n: u8) -> u8 {
    if n == 0 { 0 } else { 55 + n * 40 }
}

/// Parse `#rrggbb`, `#rrggbbaa`, `rrggbb` or `rrggbbaa`.
///
/// The `#` is optional because `server.conf` is edited by `sed` and by
/// people, and half of them will leave it out. Nothing else is accepted:
/// no colour names, no `rgb()`, no three-digit shorthand — a
/// configuration file that needs a colour parser with a grammar is one
/// nobody can debug from a text console.
#[must_use]
pub fn parse_color(text: &str) -> Option<Color> {
    let s = text.trim().strip_prefix('#').unwrap_or(text.trim());
    if !s.is_ascii() || (s.len() != 6 && s.len() != 8) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(s.get(i..i + 2)?, 16).ok();
    Some(Color::rgba(
        byte(0)?,
        byte(2)?,
        byte(4)?,
        if s.len() == 8 { byte(6)? } else { 255 },
    ))
}

/// Format a colour as `#rrggbb`, or `#rrggbbaa` when it is translucent.
///
/// The inverse of [`parse_color`] for every colour: opaque colours round
/// trip through the short form, translucent ones through the long one.
#[must_use]
pub fn format_color(c: Color) -> String {
    if c.is_opaque() {
        format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a)
    }
}

/// WCAG relative luminance of an sRGB colour, 0.0 (black) to 1.0 (white).
///
/// Alpha is ignored: a contrast ratio is only defined between opaque
/// colours, and every text/background pair the tests check is opaque.
#[must_use]
pub fn luminance(c: Color) -> f32 {
    fn channel(v: u8) -> f32 {
        let v = f32::from(v) / 255.0;
        if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
}

/// WCAG contrast ratio between two colours, 1.0 (identical) to 21.0
/// (black on white).
///
/// AA wants 4.5 for body text and 3.0 for large text and UI outlines;
/// the palette tests assert both, which is what stops a "nicer" grey
/// from quietly making a label unreadable.
#[must_use]
pub fn contrast(a: Color, b: Color) -> f32 {
    let (x, y) = (luminance(a), luminance(b));
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG AA for body text.
    const AA: f32 = 4.5;
    /// WCAG AA for large text, icons and UI outlines.
    const AA_LARGE: f32 = 3.0;

    fn both() -> [(&'static str, Palette); 2] {
        [("light", Palette::light()), ("dark", Palette::dark())]
    }

    #[test]
    fn every_role_has_a_value_in_both_schemes() {
        // "Has a value" is not "is not black": the constructors start
        // from an all-black array, so an unset role *is* black. Both
        // schemes are checked against the other one instead — a role
        // nobody set would be black in both, and black-on-black is the
        // one pair that cannot be a deliberate choice.
        for role in Role::ALL {
            let (l, d) = (Palette::light().get(*role), Palette::dark().get(*role));
            assert!(
                l != Color::BLACK || d != Color::BLACK,
                "{} is unset in both schemes",
                role.key()
            );
        }
        assert_eq!(Role::COUNT, Role::ALL.len());
        // The *last* role, pinned so that an insertion in the middle — which
        // would renumber every wire index after it — fails here rather than
        // on somebody's screen. Appending updates this line, and that edit
        // is the point: it is where you notice you are changing the wire.
        assert_eq!(Role::ALL.last(), Some(&Role::Hairline));
    }

    #[test]
    fn keys_and_indices_round_trip() {
        for (i, role) in Role::ALL.iter().enumerate() {
            assert_eq!(role.index(), i, "{}", role.key());
            assert_eq!(Role::from_index(i), Some(*role));
            assert_eq!(Role::from_key(role.key()), Some(*role), "{}", role.key());
        }
        assert_eq!(Role::from_index(Role::COUNT), None);
        assert_eq!(Role::from_key("nonsense"), None);
        assert_eq!(Role::from_key(""), None);
        // Every key is distinct, which `from_key` would silently hide by
        // matching the first arm.
        let mut keys: Vec<&str> = Role::ALL.iter().map(|r| r.key()).collect();
        keys.sort_unstable();
        let count = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), count, "duplicate role key");
    }

    #[test]
    fn ansi_roles_are_contiguous() {
        for i in 0..16u8 {
            assert_eq!(Role::ansi(i).index(), Role::Ansi0.index() + i as usize);
            assert_eq!(
                Palette::dark().ansi_indexed(i),
                Palette::dark().get(Role::ansi(i))
            );
        }
        // The high nibble is masked, so a caller cannot index past the
        // table with a stray `SGR 38;5;n` value.
        assert_eq!(Role::ansi(0x1f), Role::Ansi15);
    }

    #[test]
    fn the_cube_and_ramp_match_xterms_arithmetic() {
        let p = Palette::dark();
        assert_eq!(p.ansi_indexed(16), Color::rgb(0, 0, 0));
        assert_eq!(p.ansi_indexed(231), Color::rgb(255, 255, 255));
        assert_eq!(p.ansi_indexed(196), Color::rgb(255, 0, 0));
        // The first step is 0, the second 95 — not 51, which a linear
        // ramp would give and which programs would render visibly wrong.
        assert_eq!(p.ansi_indexed(16 + 36), Color::rgb(95, 0, 0));
        assert_eq!(p.ansi_indexed(232), Color::rgb(8, 8, 8));
        assert_eq!(p.ansi_indexed(255), Color::rgb(238, 238, 238));
    }

    #[test]
    fn text_is_readable_on_what_it_sits_on() {
        use Role::{
            Accent, Button, ButtonText, Field, Placeholder, SidebarBackground, SidebarHover,
            SidebarSelected, Surface, Text, TextDim, TextOnAccent, TitleBarActive,
            TitleBarInactive, TitleTextActive, TitleTextInactive, WindowBackground,
        };
        for (name, p) in both() {
            for (fg, bg, want) in [
                (Text, WindowBackground, AA),
                (Text, Surface, AA),
                (Text, Field, AA),
                (ButtonText, Button, AA),
                (TextOnAccent, Accent, AA),
                (TitleTextActive, TitleBarActive, AA),
                (TitleTextInactive, TitleBarInactive, AA),
                (Role::TerminalText, Role::TerminalBackground, AA),
                // Secondary text is allowed the large-text ratio: it is
                // a hint, not the content, and holding it to 4.5 makes
                // it indistinguishable from `Text`, which defeats it.
                (TextDim, WindowBackground, AA_LARGE),
                (Placeholder, Field, AA_LARGE),
                // The split view's sidebar: a row's label on the pane,
                // on the selected pill and under the pointer.
                (Text, SidebarBackground, AA),
                (Text, SidebarSelected, AA),
                (Text, SidebarHover, AA),
                (TextDim, SidebarBackground, AA_LARGE),
                (TextDim, SidebarSelected, AA_LARGE),
                (TextDim, Surface, AA_LARGE),
            ] {
                let got = contrast(p.get(fg), p.get(bg));
                assert!(
                    got >= want,
                    "{name}: {} on {} is {got:.2}:1, want {want}:1",
                    fg.key(),
                    bg.key()
                );
            }
        }
    }

    /// A frame's border is the edge of **its own title bar**, not a
    /// second colour beside it.
    ///
    /// #3724, from the box: "there seems to be a frame around the bottom
    /// left and right window sides, but that is a bit wider than the
    /// title bar, and a different color". Half of that was geometry (the
    /// border did not follow the bar's rounded corners — `docs/wm.md`);
    /// this is the other half. `window_border_active` was `#6d8eb8`, a
    /// mid blue against a `#d6dde8` pale-blue bar: a *different colour*,
    /// which the eye reads as a second frame around the first.
    ///
    /// "Its own bar's shade" is checkable without naming a value, in two
    /// parts. Each border must be **distinct enough from its bar to read
    /// as an outline, and no more** — the ceiling is what the old blue
    /// failed, at 2.9:1. And the two borders must be **sorted the way
    /// their two bars are**: whichever bar is lighter has the lighter
    /// border, which is what "each is its own bar's edge" means, and is
    /// something a single accent could never satisfy. The obvious third
    /// claim — each border is *nearer* its own bar than the other — is
    /// not checkable here: the light scheme's two bars are 1.16:1 apart,
    /// closer to each other than either is to its border, so the
    /// comparison would measure rounding rather than design.

    #[test]
    fn a_frame_border_is_its_own_title_bars_shade() {
        use Role::{TitleBarActive, TitleBarInactive, WindowBorderActive, WindowBorderInactive};
        for (name, p) in both() {
            for (border, own) in [
                (WindowBorderActive, TitleBarActive),
                (WindowBorderInactive, TitleBarInactive),
            ] {
                let (b, o) = (p.get(border), p.get(own));
                let mine = contrast(b, o);
                // Visible as an outline, but a *shade*: a 1-px line needs
                // some separation to exist at all, and more than this
                // stops being the bar's edge and starts being a frame of
                // its own. The old `#6d8eb8` on `#d6dde8` was 2.9.
                assert!(
                    (1.25..=2.6).contains(&mine),
                    "{name}: {} against {} is {mine:.2}:1 — outside the \
                     ‘a shade of its own bar’ band",
                    border.key(),
                    own.key()
                );
            }
            let brighter_bar =
                luminance(p.get(TitleBarActive)) > luminance(p.get(TitleBarInactive));
            let brighter_border =
                luminance(p.get(WindowBorderActive)) > luminance(p.get(WindowBorderInactive));
            assert_eq!(
                brighter_bar, brighter_border,
                "{name}: the borders are not sorted the way their bars are, \
                 so at least one is not its own bar's shade"
            );
        }
    }

    /// The split view's sidebar is a *shade* off the window background —
    /// darker on paper, lighter on slate — and its pill and hairline are
    /// shades too, not borders. See `docs/ui.md`, "Split view blueprint".
    #[test]
    fn the_sidebar_sits_a_shade_off_the_window() {
        use Role::{Hairline, SidebarBackground, SidebarHover, SidebarSelected, Surface};
        let light = Palette::light();
        assert!(
            luminance(light.get(SidebarBackground)) < luminance(light.get(Role::WindowBackground))
        );
        let dark = Palette::dark();
        assert!(
            luminance(dark.get(SidebarBackground)) > luminance(dark.get(Role::WindowBackground))
        );
        for (name, p) in both() {
            // The pill and the hover face read against the pane, but as
            // a pill, not a slab; the hover is fainter than the pill.
            let pill = contrast(p.get(SidebarSelected), p.get(SidebarBackground));
            let hover = contrast(p.get(SidebarHover), p.get(SidebarBackground));
            assert!((1.1..=2.0).contains(&pill), "{name}: pill is {pill:.2}:1");
            assert!(
                (1.05..=2.0).contains(&hover),
                "{name}: hover is {hover:.2}:1"
            );
            assert!(hover < pill, "{name}: hover is stronger than the pill");
            // The hairline is visible beside everything it separates,
            // and a shade beside all of them.
            for beside in [Surface, Role::WindowBackground, SidebarBackground] {
                let got = contrast(p.get(Hairline), p.get(beside));
                assert!(
                    (1.05..=2.0).contains(&got),
                    "{name}: hairline beside {} is {got:.2}:1",
                    beside.key()
                );
            }
        }
    }

    #[test]
    fn the_resize_hint_reads_against_everything_beside_it() {
        use Role::{
            DesktopBottom, DesktopTop, ResizeHint, TitleBarActive, TitleBarInactive,
            WindowBackground, WindowBorderActive, WindowBorderInactive,
        };
        // A 1-px stroke that replaces the border for as long as the
        // pointer is in the band: it has to be a *different thing* from
        // the border, not a shade of it. The old accent-coloured hint was
        // 2.35:1 (dark) and 2.16:1 (light) against the active border,
        // which is what #565 could not see on hardware.
        for (name, p) in both() {
            let hint = p.get(ResizeHint);
            for beside in [
                WindowBorderActive,
                WindowBorderInactive,
                TitleBarActive,
                TitleBarInactive,
                DesktopTop,
                DesktopBottom,
                WindowBackground,
            ] {
                let got = contrast(hint, p.get(beside));
                assert!(
                    got >= AA_LARGE,
                    "{name}: resize_hint against {} is {got:.2}:1",
                    beside.key()
                );
            }
        }
    }

    #[test]
    fn the_ansi_colours_are_legible_on_their_terminal_background() {
        for (name, p) in both() {
            let bg = p.get(Role::TerminalBackground);
            for i in 0..16u8 {
                let c = p.get(Role::ansi(i));
                assert_ne!(c, bg, "{name}: ansi {i} is invisible");
                // 0, 7, 8 and 15 are the palette's own greyscale ends:
                // by ANSI convention one of them *is* the background
                // colour of the scheme (black on dark, white on light),
                // so holding them to a contrast ratio would mean
                // choosing colours no terminal uses. The twelve
                // chromatic ones carry the meaning and are checked.
                if matches!(i, 0 | 7 | 8 | 15) {
                    continue;
                }
                let got = contrast(c, bg);
                assert!(
                    got >= AA_LARGE,
                    "{name}: ansi {i} on the terminal background is {got:.2}:1"
                );
            }
        }
    }

    #[test]
    fn the_two_schemes_really_are_different() {
        // A copy-paste that left one scheme identical to the other would
        // pass every contrast test above.
        assert_ne!(Palette::light(), Palette::dark());
        assert!(
            luminance(Palette::light().get(Role::WindowBackground))
                > luminance(Palette::dark().get(Role::WindowBackground))
        );
        assert_eq!(Palette::default(), Palette::light());
        assert_eq!(Scheme::default(), Scheme::Light);
        assert_eq!(Scheme::Light.palette(), Palette::light());
        assert_eq!(Scheme::Dark.palette(), Palette::dark());
    }

    #[test]
    fn scheme_names_round_trip() {
        for s in [Scheme::Light, Scheme::Dark] {
            assert_eq!(Scheme::from_name(s.name()), Some(s));
        }
        assert_eq!(Scheme::from_name(" DARK "), Some(Scheme::Dark));
        assert_eq!(Scheme::from_name("Light"), Some(Scheme::Light));
        assert_eq!(Scheme::from_name("solarized"), None);
        assert_eq!(Scheme::from_name(""), None);
    }

    #[test]
    fn colours_round_trip_through_hex() {
        for (_, p) in both() {
            for (role, c) in p.iter() {
                let text = format_color(c);
                assert_eq!(parse_color(&text), Some(c), "{}", role.key());
                // The `#` is optional on the way in.
                assert_eq!(parse_color(text.trim_start_matches('#')), Some(c));
            }
        }
        assert_eq!(parse_color("#ff8000"), Some(Color::rgb(255, 128, 0)));
        assert_eq!(parse_color("FF8000"), Some(Color::rgb(255, 128, 0)));
        assert_eq!(
            parse_color("  #ff800080  "),
            Some(Color::rgba(255, 128, 0, 128))
        );
        assert_eq!(format_color(Color::rgba(1, 2, 3, 4)), "#01020304");
        assert_eq!(format_color(Color::rgb(1, 2, 3)), "#010203");
    }

    #[test]
    fn a_malformed_colour_is_none_rather_than_a_guess() {
        for bad in [
            "",
            "#",
            "#fff",
            "fff",
            "#ff88",
            "#gg8800",
            "#ff88000",
            "#ff8800000",
            "blue",
            "rgb(1,2,3)",
            "#ff88 00",
            "−#ff8800",
            "#ff88\u{00a0}0",
        ] {
            assert_eq!(parse_color(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn luminance_and_contrast_agree_with_the_standard() {
        assert!((luminance(Color::BLACK) - 0.0).abs() < 1e-6);
        assert!((luminance(Color::WHITE) - 1.0).abs() < 1e-6);
        // The two anchors every WCAG implementation is checked against.
        assert!((contrast(Color::BLACK, Color::WHITE) - 21.0).abs() < 0.01);
        assert!((contrast(Color::WHITE, Color::WHITE) - 1.0).abs() < 1e-6);
        // Symmetric, and alpha is ignored. Bit equality rather than an
        // epsilon: both sides are the *same* arithmetic on the same two
        // luminances, so anything but an exact match would mean the
        // function is not the pure computation it looks like.
        assert_eq!(
            contrast(Color::BLACK, Color::WHITE).to_bits(),
            contrast(Color::WHITE, Color::BLACK).to_bits()
        );
        assert_eq!(
            contrast(Color::WHITE.with_alpha(3), Color::BLACK).to_bits(),
            contrast(Color::WHITE, Color::BLACK).to_bits()
        );
    }

    #[test]
    fn set_and_get_address_the_same_slot() {
        let mut p = Palette::light();
        p.set(Role::Accent, Color::rgb(1, 2, 3));
        assert_eq!(p.get(Role::Accent), Color::rgb(1, 2, 3));
        assert_eq!(p.colors()[Role::Accent.index()], Color::rgb(1, 2, 3));
        assert_eq!(p.iter().count(), Role::COUNT);
        assert_eq!(
            Palette::from_colors(*Palette::dark().colors()),
            Palette::dark()
        );
    }
}
