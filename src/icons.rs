//! The button glyphs, in three renderings: plain Unicode (the default ---
//! readable on any terminal, no font requirement), Nerd Font ([`[ui]
//! icons`][crate::settings::icons] = `"nerd"` in the user config) and
//! *no glyphs at all* (`"none"`: the labels stand alone, and the
//! decorative emojis outside the buttons --- the file browser's kind
//! icons, the home screen's backend marks --- disappear too; see
//! [`IconSet::shows_decorations`]).
//!
//! This is the one place Private Use Area codepoints may exist at all ---
//! and even here only as `\u{…}` escapes, never literal characters, so
//! every source file stays scannable by `tests/no_private_use_glyphs.rs`
//! without an exception list. The rule the escape discipline enforces:
//! a terminal that did not opt in never meets a PUA glyph, because the
//! default set is Unicode and the Nerd set is unreachable without the
//! config key (`AGENTS.md`'s TUI Guidelines own the rule).
//!
//! The Nerd half draws from single-width BMP ranges present since the
//! first Nerd Fonts releases: Font Awesome (`nf-fa-*`, U+F000–U+F2FF)
//! and the custom/seti set (`nf-custom-*`, U+E5FA–U+E7C5) --- never the
//! FA5 brands (U+F3xx, missing from partially patched builds) nor the
//! Material Design icons Nerd Fonts v3 moved to the supplementary planes
//! (double-width besides). The one-column glyph budget
//! (`ui::button`'s width math counts `char`s) holds across all of them.
//!
//! Glyphs are named by the action they mark, not by the glyph they draw.
//! Each of today's Unicode glyphs keeps its own slot unchanged --- the
//! default rendering is exactly what the panes showed before this module
//! existed --- while the Nerd half may map two of them onto one glyph
//! when they mean the same action in different panes: `clean` (the build
//! pane's `×`) and `erase` (esptool's `⌫`) both become `nf-fa-eraser`,
//! `flash` (`⇧`) and `write` (`⇪`) both become `nf-fa-upload`, and the
//! two "again" arrows (`⟳`/`↻`) both become `nf-fa-refresh`. The same
//! vocabulary grammar `ui::overlay`'s Zephyr Actions menu follows.

/// Which rendering the button glyphs come from. Resolved once at startup
/// ([`App::resolve_icons`][crate::app::resolve_icons]) and read off `App`
/// by the draw calls that build a button stack --- unlike the theme it has
/// no per-frame derivation, so there is nothing to thread: the stored
/// value *is* the single source of truth. The one runtime switch is the
/// dashboard's `ctrl+i` cycle, which steps the three values in declaration
/// order and re-persists the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IconSet {
    /// Plain Unicode glyphs: the default, and the fallback for an absent
    /// or unrecognized `[ui] icons` value.
    #[default]
    Unicode,
    /// Nerd Font glyphs (`nf-fa-*`). Requires a terminal font with the
    /// Nerd Font patches; opting in without one renders tofu, which is
    /// why the default stays `Unicode`.
    Nerd,
    /// No glyphs at all: every glyph method answers `""`, which
    /// [`crate::ui::button::Button::icon`] reads as "draw the label
    /// alone", and the decorative emojis outside the buttons stop
    /// drawing ([`Self::shows_decorations`]). The information the
    /// glyphs preview (destructive vs read-only, by color and shape)
    /// is never *only* in the glyph: the confirm overlays and the
    /// labels carry it.
    None,
}

impl IconSet {
    /// Parses a stored `[ui] icons` value --- case-insensitively, the same
    /// tolerance [`ThemeChoice::from_slug`](crate::app::ThemeChoice::from_slug)
    /// gives `[ui] theme` (the config parser hands over the value already
    /// trimmed, so there is nothing to strip here either).
    pub fn from_slug(slug: &str) -> Option<Self> {
        if slug.eq_ignore_ascii_case("unicode") {
            Some(Self::Unicode)
        } else if slug.eq_ignore_ascii_case("nerd") {
            Some(Self::Nerd)
        } else if slug.eq_ignore_ascii_case("none") {
            Some(Self::None)
        } else {
            None
        }
    }

    /// Whether the *decorative* glyphs outside the button stacks --- the
    /// file browser's kind emojis (`📁`/`🐍`/…), the home screen's backend
    /// marks (`🐍`/`🔷`) --- draw at all. Every set but `None` keeps them:
    /// an icon *rendering* is a choice about how buttons look, while `none`
    /// is the operator saying the decoration itself is noise. The glyphs
    /// that carry state (the checklist's `✓ ⚠ ✗ □`, the sync markers, the
    /// spinner) are not decorations and never answer to this.
    pub const fn shows_decorations(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Start the action: `Build`, the installer's forward steps.
    /// `▶` / `nf-fa-play`.
    pub const fn play(self) -> &'static str {
        match self {
            Self::Unicode => "▶",
            Self::Nerd => "\u{F04B}",
            Self::None => "",
        }
    }

    /// Wipe the build dir: the build pane's `Clean`. `×` /
    /// `nf-fa-eraser` --- the same Nerd glyph [`Self::erase`] uses, since
    /// the two actions mean the same thing in different panes (the
    /// unification happens in the Nerd half only; the Unicode half keeps
    /// the distinct glyphs each pane already showed).
    pub const fn clean(self) -> &'static str {
        match self {
            Self::Unicode => "×",
            Self::Nerd => "\u{F12D}",
            Self::None => "",
        }
    }

    /// Wipe the board's flash: `Erase flash`. `⌫` / `nf-fa-eraser`.
    pub const fn erase(self) -> &'static str {
        match self {
            Self::Unicode => "⌫",
            Self::Nerd => "\u{F12D}",
            Self::None => "",
        }
    }

    /// Run the build again from a clean slate: the build pane's `Rebuild`.
    /// `⟳` / `nf-fa-refresh`.
    pub const fn rebuild(self) -> &'static str {
        match self {
            Self::Unicode => "⟳",
            Self::Nerd => "\u{F021}",
            Self::None => "",
        }
    }

    /// Re-fetch the workspace's checkouts: the Zephyr Actions menu's
    /// `Update Zephyr`. `↻` / `nf-fa-refresh` --- [`Self::rebuild`]'s Nerd
    /// glyph, for the same reason [`Self::erase`] shares [`Self::clean`]'s.
    pub const fn update(self) -> &'static str {
        match self {
            Self::Unicode => "↻",
            Self::Nerd => "\u{F021}",
            Self::None => "",
        }
    }

    /// Send firmware over the runner: the build pane's `Flash`. `⇧` /
    /// `nf-fa-upload`.
    pub const fn flash(self) -> &'static str {
        match self {
            Self::Unicode => "⇧",
            Self::Nerd => "\u{F093}",
            Self::None => "",
        }
    }

    /// Write an image at an address: `Write / flash firmware`. `⇪` /
    /// `nf-fa-upload` --- [`Self::flash`]'s Nerd glyph, the esptool side of
    /// the same action.
    pub const fn write(self) -> &'static str {
        match self {
            Self::Unicode => "⇪",
            Self::Nerd => "\u{F093}",
            Self::None => "",
        }
    }

    /// Edit a configuration: `Menuconfig`. `✎` / `nf-fa-pencil`.
    pub const fn pencil(self) -> &'static str {
        match self {
            Self::Unicode => "✎",
            Self::Nerd => "\u{F040}",
            Self::None => "",
        }
    }

    /// The workspace's settings: `Zephyr Actions`. `⚙` / `nf-fa-cogs`.
    pub const fn cogs(self) -> &'static str {
        match self {
            Self::Unicode => "⚙",
            Self::Nerd => "\u{F085}",
            Self::None => "",
        }
    }

    /// Fetch something new: `Install Zephyr`, `Add SDK toolchains`.
    /// `⇩` / `nf-fa-download`.
    pub const fn download(self) -> &'static str {
        match self {
            Self::Unicode => "⇩",
            Self::Nerd => "\u{F019}",
            Self::None => "",
        }
    }

    /// End a running command: `Stop`, every pane's footer box.
    /// `■` / `nf-fa-stop`.
    pub const fn stop(self) -> &'static str {
        match self {
            Self::Unicode => "■",
            Self::Nerd => "\u{F04D}",
            Self::None => "",
        }
    }

    /// Query the chip's identity: `Chip information`. `◆` /
    /// `nf-fa-microchip`.
    pub const fn microchip(self) -> &'static str {
        match self {
            Self::Unicode => "◆",
            Self::Nerd => "\u{F2DB}",
            Self::None => "",
        }
    }

    /// A read-only answer: `Flash information`. `ℹ` / `nf-fa-info-circle`.
    pub const fn info(self) -> &'static str {
        match self {
            Self::Unicode => "ℹ",
            Self::Nerd => "\u{F05A}",
            Self::None => "",
        }
    }

    /// Confirm against what is on the board: `Verify flash`, the
    /// installer's `Adopt`/`Done`. `✓` / `nf-fa-check`.
    pub const fn check(self) -> &'static str {
        match self {
            Self::Unicode => "✓",
            Self::Nerd => "\u{F00C}",
            Self::None => "",
        }
    }

    /// Power-cycle the board: `Reset`. `⏻` / `nf-fa-power-off`.
    pub const fn power(self) -> &'static str {
        match self {
            Self::Unicode => "⏻",
            Self::Nerd => "\u{F011}",
            Self::None => "",
        }
    }

    /// Look at what is there: `Identify firmware`. `◎` / `nf-fa-eye`.
    pub const fn eye(self) -> &'static str {
        match self {
            Self::Unicode => "◎",
            Self::Nerd => "\u{F06E}",
            Self::None => "",
        }
    }

    /// Look something up: `Search firmware online`. `⌕` / `nf-fa-search`.
    pub const fn search(self) -> &'static str {
        match self {
            Self::Unicode => "⌕",
            Self::Nerd => "\u{F002}",
            Self::None => "",
        }
    }

    /// Extend what is installed: the Zephyr Actions menu's
    /// `Add SDK toolchains`. `⊕` / `nf-fa-plus`.
    pub const fn plus(self) -> &'static str {
        match self {
            Self::Unicode => "⊕",
            Self::Nerd => "\u{F067}",
            Self::None => "",
        }
    }

    /// The HTML report: the Zephyr Actions menu's `Dashboard`.
    /// `▦` / `nf-fa-dashboard`.
    pub const fn dashboard(self) -> &'static str {
        match self {
            Self::Unicode => "▦",
            Self::Nerd => "\u{F0E4}",
            Self::None => "",
        }
    }

    /// The environment checklist pane's title. `☰` / `nf-fa-sliders`.
    /// Pane/tab glyphs are decoration (governed by
    /// [`Self::shows_decorations`], hidden whole by `none`) and stay
    /// width-1 in every set --- titles and strips have no per-char width
    /// budget the way `ui::button`'s math has, but the render tests'
    /// `TestBackend` annotates any line carrying a multi-width symbol,
    /// which would make the frames those titles appear in untestable.
    pub const fn environment(self) -> &'static str {
        match self {
            Self::Unicode => "☰",
            Self::Nerd => "\u{F1DE}",
            Self::None => "",
        }
    }

    /// A files pane's or tab's title. `▣` / `nf-fa-folder`. Deliberately a
    /// width-1 glyph even in the Unicode set (no emoji): the render tests'
    /// `TestBackend` annotates lines that carry multi-width symbols, and a
    /// pane title must not turn every frame it appears in untestable.
    pub const fn folder(self) -> &'static str {
        match self {
            Self::Unicode => "▣",
            Self::Nerd => "\u{F07B}",
            Self::None => "",
        }
    }

    /// An actions surface's title: the build pane and the device pane's
    /// actions tab. `↯` / `nf-fa-flash` (width-1 for the same reason
    /// [`Self::folder`] is).
    pub const fn bolt(self) -> &'static str {
        match self {
            Self::Unicode => "↯",
            Self::Nerd => "\u{F0E7}",
            Self::None => "",
        }
    }

    /// Row 3's Log tab. `▤` / `nf-fa-list`.
    pub const fn list(self) -> &'static str {
        match self {
            Self::Unicode => "▤",
            Self::Nerd => "\u{F03A}",
            Self::None => "",
        }
    }

    /// Row 3's Monitor tab. `◉` / `nf-fa-desktop`.
    pub const fn screen(self) -> &'static str {
        match self {
            Self::Unicode => "◉",
            Self::Nerd => "\u{F108}",
            Self::None => "",
        }
    }

    /// Row 3's Terminal tab: the shell's own prompt mark. `›` /
    /// `nf-fa-terminal`.
    pub const fn prompt(self) -> &'static str {
        match self {
            Self::Unicode => "›",
            Self::Nerd => "\u{F120}",
            Self::None => "",
        }
    }

    /// The MicroPython backend's mark: the Python logo under Nerd Font
    /// (the header's `▲` and the home row's `🐍` both become it), the
    /// header's own triangle in the Unicode set (the surface that owns the
    /// width-1 budget; the home keeps its emoji --- see
    /// [`crate::backend::BackendKind::icon`]). `\u{E73C}` is
    /// `nf-custom-python` (the seti set's logo): the FA5 brand at U+F3E2
    /// was tried and proved missing from partially patched Nerd Font
    /// builds, while the custom/seti range has shipped whole since the
    /// first release. The only backend with its own Nerd glyph --- Zephyr
    /// keeps its plain mark in every set.
    pub const fn python(self) -> &'static str {
        match self {
            Self::Unicode => "▲",
            Self::Nerd => "\u{E73C}",
            Self::None => "",
        }
    }

    /// Every glyph of this set, in vocabulary order --- so tests can walk
    /// the whole set without restating the method list by hand. Covers both
    /// vocabularies: the button glyphs and the pane/tab decorations.
    pub fn glyphs(self) -> [&'static str; 26] {
        [
            self.play(),
            self.clean(),
            self.erase(),
            self.rebuild(),
            self.update(),
            self.flash(),
            self.write(),
            self.pencil(),
            self.cogs(),
            self.download(),
            self.stop(),
            self.microchip(),
            self.info(),
            self.check(),
            self.power(),
            self.eye(),
            self.search(),
            self.plus(),
            self.dashboard(),
            self.environment(),
            self.folder(),
            self.bolt(),
            self.list(),
            self.screen(),
            self.prompt(),
            self.python(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::IconSet;

    /// The single-width, BMP-only contract behind the Nerd set: one
    /// `char`, inside the Basic Multilingual Plane's Private Use Area ---
    /// never Plane 15/16 (double-width territory in Nerd Fonts v3), never
    /// a multi-glyph string. `ui::button`'s width math counts `char`s, so
    /// a violation miscounts a row's centering rather than failing loudly.
    #[test]
    fn nerd_glyphs_are_single_chars_in_the_bmp_pua() {
        for glyph in IconSet::Nerd.glyphs() {
            let mut chars = glyph.chars();
            let Some(first) = chars.next() else {
                panic!("empty glyph");
            };
            assert!(
                chars.next().is_none(),
                "glyph must be a single char: {glyph:?}"
            );
            assert!(
                (0xE000..=0xF8FF).contains(&(first as u32)),
                "glyph left the BMP PUA: U+{:04X}",
                first as u32
            );
        }
    }

    /// The default set stays PUA-free --- that is the whole arrangement:
    /// opting in is what admits PUA, so nothing may leak into `Unicode`.
    #[test]
    fn unicode_glyphs_never_enter_the_pua() {
        for glyph in IconSet::Unicode.glyphs() {
            assert_eq!(
                glyph.chars().count(),
                1,
                "glyph must be a single char: {glyph:?}"
            );
            let code = glyph.chars().next().unwrap() as u32;
            assert!(
                !(0xE000..=0xF8FF).contains(&code) && !(0xF0000..=0x10FFFD).contains(&code),
                "Unicode set must stay standard: U+{code:04X}"
            );
        }
    }

    #[test]
    fn slugs_parse_case_insensitively_and_refuse_the_rest() {
        assert_eq!(IconSet::from_slug("unicode"), Some(IconSet::Unicode));
        assert_eq!(IconSet::from_slug("Unicode"), Some(IconSet::Unicode));
        assert_eq!(IconSet::from_slug("nerd"), Some(IconSet::Nerd));
        assert_eq!(IconSet::from_slug("NERD"), Some(IconSet::Nerd));
        assert_eq!(IconSet::from_slug("none"), Some(IconSet::None));
        assert_eq!(IconSet::from_slug("None"), Some(IconSet::None));
        assert_eq!(IconSet::from_slug("basic"), None);
        assert_eq!(IconSet::from_slug("nerd-font"), None);
        assert_eq!(IconSet::from_slug("not-a-font"), None);
    }

    /// The `none` set's whole contract: every glyph is the empty string ---
    /// which is what `Button::icon` reads as "label alone" and the
    /// decorative-emoji call sites read as "don't draw" --- and the
    /// decorations answer `false`.
    #[test]
    fn none_answers_empty_for_every_glyph_and_hides_decorations() {
        for glyph in IconSet::None.glyphs() {
            assert!(glyph.is_empty(), "the none set has no glyphs: {glyph:?}");
        }
        assert!(IconSet::Unicode.shows_decorations());
        assert!(IconSet::Nerd.shows_decorations());
        assert!(!IconSet::None.shows_decorations());
    }
}
