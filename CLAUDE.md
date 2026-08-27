# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository state

A session now has **two screens** (`src/main.rs` alternates them, one `TerminalGuard` for both).
`startup::route` decides which, before the terminal is taken over: a directory whose backend is
known (registry → `chiptui.toml` → evidence, nearest ancestor first) or ambiguous opens the
dashboard; an *empty* one opens it too, so `Overlay::ProjectSetup` can scaffold it; anything else
(a directory with contents and no project — `$HOME`, `~/Downloads`) opens the **home screen**
(`src/home.rs`, `src/ui/home.rs`): create row, live search field, then the recorded projects,
each row tinted with `BackendKind::palette(theme)` — the backend's semantic color (`success`/
`info`) blended toward the theme's own background, so the tint follows the active theme —
deepened under the cursor, never reversed — a painted row cannot also be inverted). Creating a
project is folder picker (the workspace pane's
`dir_rows`, starting at `[projects] last_parent`) → name → an empty directory that routes straight
back into the backend prompt. `del` forgets a registry entry, never the directory. `shift+P` on
the dashboard returns to the list (`App::request_home_screen` → `Overlay::ConfirmSwitchProject`
when commands are running → `switch_requested`, read by `main.rs`, which drops the `App` and with
it every child process). Answering the backend prompt writes the backend's own starting layout
(`Backend::scaffold` → `project::scaffold::create`, never overwriting) and records the project;
it no longer creates a `chiptui.toml`.

Phase 1 of `SPEC.md` §17 is done (core, TUI, detection, backend registry, capabilities), plus the
process manager and the first real device operation: a dual-pane local/device **file browser** for
MicroPython, with list/compare, a per-entry action menu (send to device, download, view, edit,
diff, delete) and a read-only viewer with lightweight syntax highlighting (`src/highlight.rs`) and
`$EDITOR` handoff (`src/editor.rs`, `src/terminal.rs`'s `TerminalGuard::suspend`). `Enter` opens the
menu for *any* entry now, not just text files — a directory gets it too, defaulted to `Open`, plus a
recursive send/download/delete (`Browser::request_upload_dir` and friends, `mpremote fs --recursive
cp`); a binary file (e.g. `.mpy`) still offers send/download/delete, just not view/edit, which stay
gated on `files::is_text_like` (`FileAction::for_entry`). `Diff` (a unified
diff of local vs device, `src/diff.rs`, coloured in the viewer) appears only
when the comparison verdict marks the file as differing or same-size-unchecked.
`→` stays pure navigation, separate from the
menu: it descends into a directory directly (a no-op on a file), mirroring `←`/Backspace going back up
— only `Enter` opens the menu. `a` creates a new
entry in the focused pane inline — a trailing `/` on the typed name makes it a directory
(`Browser::request_mkdir`/`request_touch`). The local pane is titled `Files: name/path`
(`Browser::local_root`, re-rooted by `set_local_root`): MicroPython makes the project a
question too (`Capability::ProjectSelect`) --- `[micropython] projects` in the user config
(`settings::mpy_projects_raw`/`save_mpy_projects`) answers the Projects base row, a pick
from its subdirectories (`Overlay::ProjectPicker { mpy: true }`, any subdirectory qualifies)
re-roots the local pane session-only (`App::set_mpy_project`), and the pane's other two rows
report `Dependencies` (mip coverage: the specifications `requirements.txt` declares
against what the device's `/lib` already holds, `backend::micropython::deps`; the
file is parsed on the host --- mpremote 1.28 has no `-r` --- with `pkg==1.2.3` pins
rewritten to mip's `pkg@1.2.3` and ranges degraded to the bare name. A dotted name is
a *path*: `umqtt.simple` lands at `/lib/umqtt/simple.mpy`, so `deps::lib_target` maps
name to directory-plus-candidates and `deps::installed` answers `Yes/No/Unknown` ---
`Unknown` only while the directory it would live in exists but has not been listed,
which is what bounds the extra listings to one per namespace the board really has
(matching the dotted name flat against `/lib` reported the template's own example
missing forever, pinning the row at ⚠). The `/lib` listing rides the connection as a
background `Request::BackgroundList` queued by the first root listing, where a missing
`/lib` is cached as empty --- "0/N installed", not an error --- and the sub-listings
are queued off *its* completion, through `BrowserUpdate::listed`) and `Boot files`
(the device's root against the project's **sync root** --- `files::sync_root`, the
`src/`-when-it-exists rule `App::initial_local_dir` also delegates to, since the
scaffold writes `boot.py`/`main.py` into `src/` and reading the project root instead
left every scaffolded project reporting `boot.py ←` forever. `Identical` was equally
unreachable without pressing `c` by hand, so the same root listing arms a silent
`Request::Hash` for each file the two sides hold at the same size, and `SameSize`
reads as `□` unchecked rather than a warning; the row's mark is the *worst* of the two
files, since letting `main.py` override hid a differing `boot.py` behind a green
check. Verdicts are now keyed by `DevicePath` (`Browser::verdicts_for`) and
`Request::Hash` snapshots its directory at enqueue --- keyed by bare name they aliased
across directories, and navigation never cleared them. The script-running *belief*
lives on the device pane's tab strip and in the interrupt gates, not on a row).

`Enter` or `s` on the Dependencies row --- and `i` on the device pane, and the Actions
tab's `Manage packages` button --- open `Overlay::Packages` (`src/app/packages.rs`),
the package **manager**: one filterable list merging what `requirements.txt` declares,
what `/lib` holds, and the micropython-lib index, read from
`https://micropython.org/pi/v2/index.json` (the machine-generated listing of the same
index mip installs from --- `mip` has no search/list subcommand of its own) through
`curl fetch_page`, the exact delegation the firmware pages use, no bundled HTTP client,
fetched once per session into `App::package_index` (`Idle/Fetching/Ready/Failed`, events
consumed in `on_process` before any other subsystem), parsed by the hand-rolled tolerant
reader in `backend::micropython::packages` (no serde, same bias as the config parsers).
Each row carries its state (`✓` declared+installed, `□` declared+missing, `⚠` installed
but undeclared, blank for catalogue-only) and the details pane beside it names the
declared spec and the `/lib` paths the package occupies; `Tab` swaps the keyboard
between the two halves (`DocsFocus`, the board/shield pickers' grammar) and the geometry
is one definition in `ui::layout::packages`, shared with the hit-testing.

The variant is a **unit** one: every field lives on `App::packages` (`PackagesState`),
because the removal confirmation *replaces* the window --- the overlay slot is one deep
--- and has to hand it back unchanged. The filter line is free text, so no action can
live on a plain letter (the same rule that makes `?` filter instead of opening help):
`Enter` installs the row and declares it first when the file does not
(`deps::add_line`, which de-duplicates and *replaces* a differently-pinned line rather
than appending a second), `Del` removes both halves behind `Overlay::ConfirmRemovePackage`
in the §15 destructive grammar (`mip` has no `uninstall`, so the removal is
`Request::RemoveDevice`/`RemoveDeviceRecursive` over the paths `lib_target` resolves
against the listing --- a dotted package's *leaf*, never the directory it shares with
siblings, and never a namespace directory, which is why `package_rows` suppresses those
as rows at all), and "install everything" is a **row** rather than a key. Text that
looks like a spec (`:` or `/`) or matches nothing is offered verbatim as
`+ install "…"`, which is the dead end the search-only window had. The window stays
open after every action, and a click selects without activating (picker grammar). The
`j`/`k` cursor arms are gone with the search: bound before the printable arm, they made
`json` and `keyboard` untypeable. `requirements.txt` itself is read off the tick
(`App::requirements`, `RequirementsCache`) on the same 1 s cadence as the local
listings, not `read_to_string`d inside the draw path once per frame per consumer; the
app's own writes refresh it immediately. The board's
MicroPython version itself, parsed off the REPL
banner by `device::micropython_version` --- fed by the probe and the monitor, dropped with the
board on disconnect/switch --- rides the Device Info pane's `Firmware` row instead (see below),
appended to the `MicroPython` label. Editing a device file downloads it to a scratch temp file
(`browser::edit_download_path`), never the project tree — the point is to prove a change on the
device first; `Download` is the separate, explicit step for landing a confirmed-good result in the
project. On a clean `$EDITOR` exit it re-uploads to the same device path and then offers a
`soft-reset`, defaulting to *no* (`Overlay::ConfirmRestartDevice`). A board believed to be
running a script is never interrupted silently: a short `mpremote repl` probe runs before the
first listing (`src/app/probe.rs`), device operations are held behind a confirmation while a
script runs (`Overlay::ConfirmInterruptDevice`), and an accepted interruption ends with a
restore prompt (`Overlay::RestoreDeviceScript`: hard reset, `import main`, or leave stopped).

Row 2 is capability-driven now: a backend that can build without a device filesystem (Zephyr)
claims the *whole* row with a **Files | Actions** pair (`maybe_scan_devices`/
`ensure_browser_scanning` skip the browser entirely for such a backend — listing/editing the
project's own sources is the user's editor's job; MicroPython's dual-pane browser and its
capability-gated `FileAction::for_entry` menu are unchanged, except that its device pane is
a *tabbed* pane for a backend that can also flash: an `Actions • Device Files` strip
drawn on the pane's border the way row 3's Log/Monitor/Terminal strip is, the *active* tab's status
riding the strip's right edge: the walked device path plus the running-script flag for the
files tab, the flag alone for the actions tab (no listing to locate there, but a running
script gates every esptool action). `x`
(`App::open_flash`) creates the flash panel and switches to the actions tab instead of
opening a dialog; `ctrl+←/→` is a **dashboard-wide chord** (`App::switch_strip_tabs`,
handled in `on_dashboard_key` before every pane dispatch, like `m`) that walks the row's
stops *in the arrow's direction* (`App::step_focus_horizontal`): a row holds two panes
(Environment ↔ Device Info, workspace ↔ build), and a strip-owning device pane contributes
its tabs as stops of their own in strip order — MicroPython's working row walks pane 3 →
pane 4 Actions → pane 4 Device Files and back, one keypress per stop, clamped at the ends
(never a wrap; entering pane 4 from pane 3 lands on Actions, the strip's first tab, and
creates the flash panel the tab draws). **Tabs answer to the chord only, on every strip**:
the plain `←/→` no longer switch any tab — not the device pane's (its actions side takes
`↑/↓` alone for the buttons, the arrows answer to nothing there) and not row 3's either;
on the files side the plain arrows keep their directory meaning (descend/ascend, `Enter`'s
menu descends), the same grammar the local pane and an untabbed device pane (no flash
capability) always had — and because the chord is intercepted before the dispatch, it
never leaks into a pane's own arrows (on the local pane `ctrl+→` must not descend). Row 3
has no pane beside it, so its chord switches its own strip instead (`switch_log_tab`).
`ctrl+↑/↓` is the chord family's vertical half
(`App::step_focus_vertical`, intercepted beside the strip chord): it steps focus between
the dashboard's *rows* **staying in the same half of the screen** (`App::focus_column`/
`focusable_in_row` — `ctrl+↓` from Device Info lands on the device pane/build panel beneath
it, never the local column to its left), a half with nothing focusable falling back to the
other, a row with nothing focusable skipped, row 3 (full width) entered from either side
and left upward onto the left half; a no-op while row 3 is fullscreen, like `Tab`. Focus is also reachable without walking: **every pane's
title carries a fixed shortcut number** at the leading edge (`ui::numbered_title` over
`App::pane_number` — 1 Environment, 2 Device Info, 3 the working row's left pane, 4 its
right one, 5 row 3, fixed per position rather than per tour stop so the digits mean the same
thing in every backend; strips carry the number as a muted span before the first tab,
budgeted in `mouse::strip_tab`'s width walk too), and `1`..`5` jump straight to that pane
(`App::pane_for_number`; a digit with no focusable pane behind it falls through). Both row-1
panes stay off the `Tab` tour — the tour walks working panes only — but both are reachable
by digit and chord: Device Info (`Focus::DeviceInfo`, always drawn, never clamped) holds a
one-row selection grammar whose single actionable row is the MAC, selected the moment focus
arrives (`draw_detection` renders it through the shared `render_row`/`selection_style`), with
`Enter` copying it through the same `App::copy_to_clipboard` the row's click uses (`Enter`
with no MAC read is a quiet no-op). The tab renders the esptool menu as
the *same* stacked-button widget the Zephyr build pane uses (`ui::flash::draw_actions_pane`:
one button per `FlashPanel::pane_actions` row, in workflow order --- `⇩ Search firmware online`
(the menu's old `s` key as a button) leads, then `⚙ Manage packages` (the `Overlay::Packages`
door that does not go through the Project pane; `⚙` is `Zephyr Actions`' own glyph and the same
role, a button opening a menu of operations), then the read-only esptool actions, the destructive
erase/write pair last, capitalized like the build pane's; the chip
identity every device selection already queries in the background gets no button of its own
(it is not among the pane rows, though the dialog menu still lists it), and `Verify flash`
left the stack for a related reason --- a check rather than a workflow step, and gated on a
firmware file being chosen first --- answering to `v` on the tab instead while staying in
`FlashAction::ALL` for the dialog form of the menu. (`c` was deliberately not used: it already
means "compare by sha256" in the file panes.) The swap is **height-neutral by construction**:
`Manage packages` takes exactly the row `Verify flash` gave up, so the idle count stays six and
matches the `FlashAction::ALL.len()` fallback `row2_content_height` uses before a panel exists
--- which is what keeps row 2 from reflowing when the panel appears, and the declared 80x32
minimum where it is. A
direct download URL is pasted with `u` from the search windows --- over the
same reserved three-row footer, `■ Stop` as its own half-width box while a command runs,
the state line with a live counter/last report), row 2 sized to the stack whenever the strip
exists --- both tabs hold that height, so flipping Files/Actions (from anywhere, via the
chord) never reflows the rows below (`row2_content_height`, which applies the same stack
formula over `FlashAction::ALL`'s rows when no panel exists yet, so the height is right from
the first frame). A started command keeps focus on the pane
with the cursor
parked on `Stop` (the Zephyr rule; `show_flash_in_monitor` focuses the Monitor tab only when
the run started from a dialog), and a finished one lands back on its own row with a
`FlashReport` in the footer. The state line names whatever holds the panel
(`FlashPanel::activity`), because all four states dim every button: only `Activity::User` is
counted and reported as a result --- the background queries and the curl fetches must not
present themselves as the user's work --- but a query (`reading the board…`) or a fetch
(`searching online…`/`downloading…`, kept short: while anything runs the state line owns
only the footer's left half) still says so rather than leave a dimmed menu
unexplained, and `FlashPanel::stop` cancels a fetch as readily as a command, since `is_busy`
is what puts `Stop` on the pane in the first place. Either door onto the tab --- `x` or the
strip's arrows --- creates the panel it draws (`show_device_actions_tab`), since with no
board plugged in no background query ever will. The options/online/URL screens remain
dialogs (`View::Flash`, e.g. after erase's write offer or from the search button), but only
ever open onto a screen the panel actually reached (`show_flash_dialog`: a refused search
leaves it on `FlashScreen::Menu`, which is the pane itself, so nothing opens) and `esc`
out of one returns to the pane (`leave_flash_screen`) rather than to that now-hostless
menu --- the dialog form of the menu survives only for a backend with no pane to host it;
the Erase/Write confirmations are exactly as before. The environment's
prerequisites moved *up* into row 1's **Environment pane**, which is the checklist now:
`Zephyr path`, `Projects base`, `Project path`, `Board · Shield` (the last two answered by
the build panel but asked here; `←`/`→` on the merged target row switch which half `Enter`
acts on --- `App::board_segment`) for Zephyr, `Projects base`/`Project path`/`Dependencies`/
`Boot files` for MicroPython (`src/app/project_view.rs`, `App::project_rows`). The pane is
navigable but deliberately off the `Tab` tour: the shortcuts overlay's `e` letter (`ctrl+k`)
enters it (the cursor lands on the first question still open), `Tab`
re-enters the tour at its first stop. The operation buttons they gate live in the **Actions**
pane --- a small custom widget (`src/ui/button.rs`: one stacked
group sharing a rounded border, a centered icon label per row, a `├─┤` divider between each
pair --- N buttons cost 2N+1 lines, and `draw_dashboard`'s `row2_content_height` sizes row 2
to that content (the log pane, which scrolls, takes the remainder; a device pane with a
tab strip holds this height on *both* its tabs, so the browser row no longer uses the
historical 60/40 there --- only an untabbed device pane does); the group's frame is the theme's
muted color, labels bold `fg` (muted while disabled), and the selected row
is `palette.selection`/`palette.fg` (not `Modifier::REVERSED`, which read inconsistently
across terminals) applied as a `Buffer::set_style` patch over the row's inner cells *after*
the label is drawn, filling it edge to edge without ever painting over the side rules,
dividers or outer rules --- the same rule every list, picker and checklist row now follows
through the shared `ui::selection_style`/`ui::muted_style` helpers: selected rows draw on
`palette.selection`, secondary text (labels, legends, hints, timestamps) in `palette.muted`,
so the whole UI follows the active theme; the Monitor's fake terminal cursor keeps reverse
video on purpose, being the terminal's cursor rather than a selection), which stay
visible but dimmed until their answers exist (`WorkspacePanel::action_enabled`,
`App::build_action_enabled`) --- Enter on a dimmed row is a no-op; rows carry no trailing
text (the confirm overlays quote the literal commands, `SPEC.md` §15, not the rows).
Every button stack's leading glyph comes from `src/icons.rs` (`IconSet`, resolved once at
startup from `[ui] icons` in the user config, `App::icon_set`): plain Unicode by default ---
byte-identical to what the panes showed before the module existed --- Nerd Font
`nf-fa-*` codepoints when the operator opts in with `icons = "nerd"` (written as `\u{…}`
escapes so `tests/no_private_use_glyphs.rs` still scans every file, per the amended rule in
`AGENTS.md`), or no glyphs at all with `icons = "none"` --- labels stand alone (a pane's
geometry never changes: the glyph never had a row of its own) and the *decorative* emojis
outside the buttons (the file panes' kind icons, the home screen's backend marks,
`IconSet::shows_decorations`) disappear too; state-carrying glyphs (checklist `✓ ⚠ ✗ □`,
sync markers, the spinner) are never affected. Pane titles and tab strips carry the same set's
decoration glyphs (`☰ Environment`, `◆ Device Info`, `▣ Files:…`, `↯ Actions`, row 3's
`▤ Log • ◉ Monitor • › Terminal`, the device pane's `↯ Actions • ▣ Device Files`, and the
header's backend mark) via `ui::pane_title` --- width-1 glyphs in every set (the render tests'
`TestBackend` annotates lines carrying multi-width symbols, which would make those frames
untestable), hidden whole by `none` along with the other decorations. Under the Nerd set the
MicroPython mark becomes the Python logo (`IconSet::python` = `nf-custom-python` U+E73C, the
seti set's glyph --- the FA5 brand at U+F3E2 proved missing from partially patched fonts), in
both the header and the home rows; the header shows Zephyr's `◆` diamond in every set, and the
home row follows under Nerd too (`IconSet::zephyr`, not a dedicated Nerd Font glyph --- none
exists for Zephyr --- but the header's own mark reused so a Zephyr row costs the same
single cell a MicroPython row does), keeping the plain-Unicode/`none` home row's two-cell `🔷`
(`BackendKind::icon`) unchanged. Plain Unicode keeps each surface's own MicroPython mark too
(`▲` header, `🐍` home). The file panes' `.py` row follows the backend's mark the same way
(Python logo under nerd, `🐍` otherwise); every other file-kind emoji stays in every set. The
two decoration columns that budget a two-cell mark (the home rows' backend mark, the file
panes' kind column) centre a single-cell glyph in the fixed three cells via `ui::icon_column`
(a leading pad, so it rides the second cell of a two-cell mark's span, the caller declaring its
own glyph's width rather than `icon_column` guessing it from the codepoint), keeping both the
marks and the columns after them lined up across backends and file kinds. `ctrl+i` cycles the
three values
(Kitty-protocol terminals only --- legacy sends Ctrl+I as plain Tab, which keeps its
focus-tour meaning), applying and persisting the answer like the theme picker does
(`App::cycle_icon_set`).

Mouse support is the third `[ui]` preference: `mouse = true` opts in to click/wheel reporting
for the session (default off --- reporting on makes the terminal send clicks to the app instead
of selecting text; `terminal::init` enables capture, `TerminalGuard::suspend` re-applies it
after `$EDITOR`, teardown always disables it). `event.rs` narrows the stream to left clicks and
wheel steps (`is_gesture`; motion/drag die at the source), `App::handle` drops every gesture
unless `mouse_enabled` (`set_mouse_enabled` is `main.rs`'s mirror of the guard), and
`app::mouse`'s `on_mouse` meets the *drawn* geometry: `ui::draw` publishes `App::frame_area`,
`ui::layout::dashboard` (the extracted pane-rect tree `draw_dashboard` also consumes) is
recomputed per gesture, and the file panes' clicks map through the scroll offset their
previous frame settled on and published (`WorkspacePanel::files_offset`,
`Browser::local_offset`/`device_offset` --- `drawn_list_row`; the lists seed their `ListState`
from it, so a click on a visible row selects without re-anchoring the view), while the overlay
lists keep the fresh-`ListState` minimal-scroll reproduction (`list_row`), stacked buttons land on their label rows through `run_build_action`/
`run_flash_pane_action`, tab strips are walked by their `Tabs` ranges
(`log_strip_tabs`/`device_strip_tabs`), and the wheel steps the cursor-walked list under the
pointer --- row 1's Environment checklist and row 2's file panes move their cursor one row per
notch, clamped, never taking focus (`wheel_steps_list`, the board picker's wheel grammar); the
actions tab and a non-Ready device pane have no listing to step --- while over row 3 it scrolls
the active tab by `WHEEL_STEP`. An open
overlay routes to `on_overlay_mouse` instead: confirm dialogs answer through their drawn
No/Yes buttons by *synthesizing* `y`/`n` into `on_overlay_key` (every per-variant gate for
free), pickers select without activating, stacked menus press via `Enter`, the SDK checklist
toggles via `Space`, and a click outside the dialog's drawn rect closes it exactly like `Esc`
--- synthesized into `on_overlay_key` (so every per-variant special case --- Help's filtering
step-back, the interrupt/remove-package confirms' "return to Packages", the installer's busy
guard --- applies unchanged) *before* any row/button lookup runs: a
list-row/stacked-button overlay's own click grammar (`list_row`/`button_at_row`) keys on the
row alone, blind to the column, so without this ordering a click on the right row but past
either edge of the popup would quietly answer that row's option instead of closing the dialog
(caught live in Zephyr Actions, SDK toolchains and the directory picker, pinned by
`a_click_on_the_right_row_but_outside_the_box_closes_it_instead_of_answering`). **Where each
variant's box sits is one definition**, `ui::layout::overlay_popup` --- consumed by
`ui::overlay::draw` (which hands each `draw_*` the finished rect instead of letting it centre
itself) and by the hit-testing alike, the contract `layout::packages`/`layout::docs_picker`
always had. Writing the sizes on both sides is what let them drift: an empty device picker
draws a fixed 52x4 "no board" box that the hit-testing sized `64 x len + 2`, so a click *inside*
it dismissed it (`a_click_inside_the_empty_device_picker_does_not_dismiss_it`), and
`ConfirmRemovePackage` had a rect but no entry in the confirm-button family, answering a click
beside its box while ignoring one on `Yes` (`a_click_answers_the_package_removal_confirm`).
`View::Flash`
(structurally separate from `Overlay`) carries the identical check in `on_flash_mouse`,
synthesizing into `on_flash_key` --- `leave_flash_screen`'s one-screen-back behavior for
Options/Online*/CustomUrl applies the same way. The home screen answers clicks too
(`HomeScreen::on_mouse` +
`ui::home::hit_areas`): a launcher row selects *and accepts* (the `Enter` path), the wheel
steps clamped at the ends, and `main.rs`'s `home_loop` forwards gestures only under the same
opt-in. Click tests are render-pinned: they find the label in the drawn frame and click its
column (byte offsets are not columns --- multi-byte borders). The footer `Stop` box is
deliberately not clickable; a text-input dialog (`CreateEntry`/`RenameEntry`), the viewer and
the help window have no click surface *inside* their popup either (their one meaningful gesture
is typing) --- but, like every other overlay, a click outside it still closes them.

Dimming has **two** rules, not one. `ui::content_style` is the *selection* rule --- an unfocused
pane's content goes `Modifier::DIM`, which is what makes the column the cursor sits in obvious ---
and belongs only to panes whose content is a list the cursor walks (the file columns, the Environment
checklist). Panes that carry *output* (the Log feed, the Monitor console) use `ui::output_style`
instead: dimmed only while a dialog owns the screen (`ui::dashboard_behind_dialog`), never merely
because another pane holds the cursor. A log entry does not become less worth reading when the
cursor moves, and the dashboard deliberately parks focus on the build pane while a command streams
(the Monitor tab is *shown*, the cursor waits on `Stop`) --- so dimming on focus alone dimmed
exactly what the user was watching, for the whole length of the build. Their focus indicator is the
pane border and the tab strip. `tests/ui_render.rs`'s
`output_panes_dim_behind_a_dialog_but_never_for_focus_alone` locks both halves.

Besides the border, **the focused pane carries a nearly imperceptible background tint over its
inner area only**: `ui::focused_pane_bg` is the theme's accent blended 1/64 toward the theme's
own background (the shared `backend::blend`, the same integer lerp the home rows' 3/16 tints
use — the denominators are free because the two scales answer different questions: a row-sized
wash must read at a glance, a pane-sized one must stay a whisper, two channel steps at most).
The wash is painted by
`ui::render_pane` (the standard `block → wash → content` pane render) or `ui::paint_focus_wash`
(for panes that hand their block to a `Paragraph`/`PseudoTerminal`), always over `block.inner`
— the borders keep the terminal's own background, so the frame stays a line drawing and the
tint a lit interior. It is rendered *before* the pane's content: anything the content draws
covers what it owns (a selected row's `palette.selection`), and every cell it leaves untouched
keeps the tint. It follows the theme on a switch and disappears while a dialog or the shortcuts
overlay owns the screen (`dashboard_focused` is false everywhere then); the one documented
exception is the live Terminal grid, whose per-cell `Style::reset()` would wipe any wash
under it — the border accent is that tab's focus indicator while a shell lives.
`the_focused_pane_carries_a_subtle_theme_tint` locks the tint, its subtlety (each channel
within 8 of `palette.bg`, and still distinct from it), the untinted borders, and that it
follows focus.

Every destructive confirmation shares one grammar (`SPEC.md` §15, `ui::overlay::Destructive`):
title = the action as a question, target = *what it happens to* in the warning color (board and
port, workspace path, project and build dir --- `board_target`/`chip_target`, which say
`no board selected` rather than inventing one), consequence = what is lost in a plain sentence,
then the literal command muted underneath. It covers `west flash`, `west build -t clean`,
`west update` and esptool's erase/write (`FlashPanel::pending` is what lets the shared
`Overlay::Confirm` name the action it is asking about). `No` is the default in all of them, and
`destructive_confirmations_name_the_action_the_target_and_the_cost` locks the shape.

The
header's `project` field is the project question's other half (`App::header_project`):
empty while the root is not a buildable application, then the picked project's folder name
(the MicroPython pick answers it too). Row 1 itself is a fixed height: both
panes pad their content to four rows (`ui::panels::INFO_ROWS`) in every backend and state
(and the MicroPython cwd note rides `Project path`'s own line as a muted suffix), so the
rows below never shift when a workspace resolves or device details accumulate. The
environment's `versions` (Zephyr + venv Python, read from files) ride the pane's *bottom
border* right edge (`draw_versions_badge`, the same place the Log tab strip rides its top
border) once a workspace resolves; missing tools show as a red `⚠ N` count beside the
backend name in the header (`missing_tools`, over the same `App::tool_status` the startup
warning logs).
The **workspace pane**
(`src/workspace.rs`, `src/ui/workspace.rs`, `src/app/workspace_view.rs`) is the environment
half: it resolves the Zephyr *installation* (`src/backend/zephyr/workspace.rs`) from
configuration and nowhere else --- `chiptui.toml`'s `[zephyr] workspace`, then the user
config `~/.config/chiptui/config.toml` (both parsed by `src/settings.rs`); no directory
conventions, no `$ZEPHYR_BASE`. Startup focus lands on this pane (`App::place_startup_focus`, after
`maybe_scan_devices` in `main.rs`): the environment questions come first --- unless the device pane
carries the Project actions strip (MicroPython), which starts there instead: the tab's stack sizes
the row, its panel is created (no board plugged in means no background query ever will), and the
empty-project prompt's answer (`apply_project_setup`) places focus the same way, being the
backend's first entry too. When nothing is
configured, `main.rs` calls
`maybe_open_workspace_picker` right after: `Overlay::DirPicker` is a real
filesystem browser (`workspace::dir_rows`, starting at `$HOME`) where the user navigates
to the installation and accepts it; descending lands on the "use this directory" row so
the reflex `Enter` accepts the folder just entered. The accepted directory is validated by
the *same* `install_check` the config goes through (`.west/` present, the manifest's
checkout present) --- a failure keeps the picker open with the reason plus the
getting-started link (`workspace::GETTING_STARTED`), and a configured-but-broken location
marks the pane's row `✗ !` with the same message in the log (the pane is a fixed four
rows --- the reason does not get one). A validated pick is persisted
(`settings::save_workspace`, a line-level merge that preserves every other key/section) to
the user config, or to `chiptui.toml` when the project pins its own location --- so the
config stays the single source of truth and later starts never re-ask. Tool reporting
honors the same answer: every `App::report_tools` call site resolves the workspace first
(creating the panel early is what keeps startup from warning about a `west` that was never
on `PATH` because it lives in the workspace venv) and `App::tool_status` is the one
availability definition shared by that warning and the header's `⚠ N` badge. It
names no tool: a resolved workspace declares the tools whose *location* it owns
(`Workspace::tool_locations`, empty when resolution fell through to the bare program name)
and `BackendRegistry::tool_status(kind, located)` judges those files with
`backend::executable_at` --- the same "is it runnable" predicate (metadata: a file, with an
execute bit) a `PATH` lookup uses --- while every unlocated tool keeps the `PATH` answer.
`PATH` lookups skip empty entries, which mean the cwd; that makes the *report* stricter
than `execvp`, never the reverse.
`west update` lives behind the Actions pane's `⚙ Zephyr Actions` button, enabled once the
installation resolves, under `Capability::WorkspaceSync`. Pressing it no
longer runs `west update` outright: it opens `Overlay::ZephyrActions`, a
three-button menu drawn with the Actions pane's own stacked-button widget
(`ui::button`, same icons/labels/selection grammar) --- `↻ Update Zephyr`
leads into the existing confirm (`Overlay::ConfirmBuild`,
which rewrites the shared checkouts and runs through the build panel's one
process slot into the Monitor tab), `⇩ Add SDK toolchains`
calls the same `App::open_sdk_toolchains_shortcut` the dashboard `s` key
uses, landing on `Overlay::SdkToolchains` instead, and `▦ Dashboard` runs
`west build -t dashboard` (Zephyr 4.4's build dashboard: one HTML report
over the configured build directory, opened in the browser by the target
itself) through that same process slot --- focus stays on the panel with
the cursor on `Stop`, the build rule --- with no confirm (nothing
destructive) and no board answer: the report reads an existing build
directory, and a missing one is `west`'s own error to explain in the
Monitor (`BuildAction::Dashboard`, never a row of the stack; the panel
also never adopts progress for it --- the target's own `[0/1]` ninja
counters track its internal helpers, not the report, so the state line
says `Dashboard running · 12s` rather than a meaningless count, and every
other busy command's state names its label the same way when no progress
shape arrives). That
button's slot is *shared*: with nothing resolved it reads `⇩ Install Zephyr`
instead (`BuildAction::list_for`, keyed off `BuildPanel::workspace_installed`,
pushed in by `apply_west_env` --- the one place a panel is seeded from the
resolved environment, which is why `ensure_build_panel` calls it rather than
repeating the seeding). The two are mutually exclusive by nature, so sharing
one row keeps the stack at six buttons and `ui::MIN_HEIGHT` where it is.
`Install Zephyr` opens `DirPurpose::Install` and then the **installer**
(`src/install.rs` + `src/install/{prereq,steps,version}.rs`, `src/ui/install.rs`,
`src/app/install_view.rs`, `Overlay::ZephyrInstall`): the getting-started guide
run as a sequence, with its own process slot and output buffer (not the build
panel's --- the output belongs in its own modal, and there is no resolved
workspace for a build command anyway). The `Zephyr path` row is the other door, and the one
that matters once a workspace is already resolved (the button is `Zephyr Actions`
then): a directory `install_check` refuses no longer merely re-renders the
picker with the reason --- `accept_workspace_dir` opens
`Overlay::ConfirmInstallHere`, whose wording comes from
`zephyr::workspace::install_state` (the three-state classifier `install_check`
was refactored onto, so there is one definition of `.west/`-and-checkout):
*Install* / *Finish* / *Use*. It is its own variant, not the shared
`Overlay::Confirm` (already multiplexed between the flash panel's `pending` and
the installer's start question), and it carries the refusal because the overlay
slot is one deep --- declining has to *rebuild* the picker, not uncover it. The
target is `<folder>/zephyr`, unless the folder already carries `.west/`, which is
resumed in place. A folder whose `zephyr/` is already a complete installation
opens the modal in **adopt** mode (`Installer::mark_already_installed`, decided
once in `open_installer` --- `Step::already_done` cannot answer it, since the
queries and the two idempotent steps never resume, and on a *resumed* run
`install_check` starts passing mid-sequence): the button reads
`✓ Use this installation`, is not gated on the prerequisites, and records the
location without spawning anything. Both endings share
`persist_installation`, which names the switch in the log when it replaces an
earlier installation and only chains into the projects picker while that
question is still open. Prerequisites (`cmake` ≥ 3.28.0, `dtc` ≥ 1.4.6, `pyenv`) are queried
in parallel for their versions --- the only place in this codebase that asks a
tool its version through a subprocess; every other version is read from a file
--- and a failing one dims the action button rather than being installed for
the user, naming instead the command each detected package manager would use.
The system `python3` row is `⚠`-only and never blocks: pyenv provides the
workspace's 3.12 (newest from `pyenv install --list`), and the venv is built by
that interpreter's absolute path under `pyenv root`, since `python3` off `PATH`
does not honour `pyenv local` without shims. Every step's completion is read
off the filesystem (`Step::already_done`), so an interrupted run resumes;
`west packages pip --install` and `west zephyr-export` deliberately never
qualify (no marker, and idempotent). `west sdk install` places the bundle with
`-b <workspace root>` (absolute) --- **never** `-d`, whose argument is the SDK
directory's final *name*, which overrides `-b`, extracts gigabytes inside the
git checkout, and hands `run_setup` the literal path so it runs `../setup.sh`
from the checkout: west dies there, after moving a bundle into place but before
downloading one toolchain or registering anything, which is why a `-d ..` run
looks half-installed and leaves `west sdk list` still answering nothing. The
step's cwd is the manifest checkout for the guide's own reason --- west resolves
the workspace and reads `${ZEPHYR_BASE}/SDK_VERSION` --- not to make `..` mean
something. `steps::installed_sdk` is version-aware off that same pin, so a
mismatched bundle leaves the step pending, and `steps::installed_toolchains`
reads `<sdk>/gnu/` (bundle root on pre-1.0 layouts) because a bundle ships the
*list* of what it offers but unpacks only what was asked for. The command
carries `Installer::pending_toolchains()` --- picked minus installed --- so
adding one toolchain to an existing SDK costs a `setup.sh -t` and no download
(`west sdk install` finds the version registered and skips straight to setup);
that state is `Action::AddToolchains`, and `refresh_sdk_step` flips the SDK step
back to pending on a pick so the checklist agrees with the button. The dashboard's
`s` (`App::open_sdk_toolchains_shortcut`, gated on `Capability::WorkspaceSync`
beside `m`, before the focus dispatch so it works from any pane) opens that picker
over the configured workspace directly; unconfigured, it logs and opens nothing.
Adding it forced the MicroPython `s` (save a captured run's output) to declare
`Capability::Run`, which it always depended on and never stated --- the
duplicate-footer-label test is what caught the collision. `west sdk
list` runs after the install; `west sdk list` runs *after* it
as the confirmation, because that command reads the CMake user package registry
and dies with `FATAL ERROR: No Zephyr SDK installed.` until one is --- it lists
what is installed, never what is available, and nothing enumerates the toolchain
names beforehand, so the picker offers the curated `steps::TOOLCHAINS` anchored
to the checkout's `SDK_VERSION` (a stale name fails loudly: west validates and
prints the list it accepts). The `-t` invocation is one flag carrying every
name, last on the line --- west declares it `nargs="+"`, not `append`, so a
repeated `-t` silently keeps only the final name. Picking is required
(`Installer::sdk_ready`) --- with no `-t` west passes `-t all`, 35 toolchains and
several GB unprompted --- but it is a question about the *last* step and must
never gate the eleven before it: `can_start()` deliberately does **not** include
it (doing so froze the panel before `Find Python 3.12` ever ran). Unanswered, the
button reads `▶ Pick SDK toolchains` and opens the picker; `s` answers it too.
The button's label, enabled state and effect all come from one
`Installer::action()` --- the split between `ui::install`'s label and
`on_install_key`'s dispatch is precisely how a dimmed button with no action
behind it got shipped. `Action::Adopt`/`InstallSdk` are checked *before* the
prerequisite gate, since neither adopting nor `west sdk install` needs cmake or
dtc; `ui::install::state_line` mirrors that order or it contradicts the button
beside it. An adopted workspace missing only its bundle runs
`Installer::start_sdk_only`, which enters at the `SdkInstall` index rather than
`next_step()` --- on an installed tree that answers 0 and would re-run
everything. A step
that fails is fatal unless `Step::optional()` (only the SDK confirmation), and
the auto-advance searches `next_step_from(index + 1)` --- `next_step` reports
failures so `Retry` can resume them, and searching from 0 would re-run the
failure forever. A fatal stop still calls `App::salvage_installation`, which
records the workspace when `install_state` says it is already `Complete`, so a
late failure never discards a good `west init` + `west update`. `esc` is ignored while a step runs --- `Stop` is the way out. A finished
run writes `[zephyr] workspace` (+ `sdk`), re-resolves, and chains into the
projects-folder picker. Its four-state row grammar (`✓ ⚠ ✗ □`) is the shared
`ui::workspace::marked_row`, which `checklist_row` now delegates to. The **Files**
pane (the old workspace pane, `src/ui/workspace.rs`) is the project's own listing, whole:
its title carries the walked path (`Files: proj/src/`, never truncating the
prefix), and the body is the list --- no action menu: `Enter` descends into a
directory and opens a text file straight in `$EDITOR`, `v` views one in the viewer, `Del`
asks through `Overlay::ConfirmDelete` (default No), and a binary/unknown entry ignores both
keys (all through `App::run_file_action`, `Side::Local`); `r` renames the entry under the
cursor --- any kind, via `Overlay::RenameEntry` pre-filled with the current name and a local
`fs::rename` on confirm (`App::rename_entry`; a `/` in the typed name is refused, since a
rename must not silently become a move), and a below-root listing leads with a `[..]` parent row
(`WorkspacePanel::parent_row`), selected after every descent, `Enter`/`→` on it stepping back
up. Every file list (this pane, the browser's local and device panes --- one `render_list` in
`ui::files`) carries the shared one-column scrollbar (`ui::draw_scrollbar`) over a column the
list *always* reserves (`ui::files::list_view`; rows build against that width), so the
flush-right size column never shifts when a bar appears, and the thumb reports the offset the
pane's `ListState` settled on --- seeded from the previous frame's published value (the docs
picker's `docs_list_offset` rule), so a click on a visible row never re-anchors the view; the
click hit-testing maps through that same offset (`drawn_list_row`), not a recomputed one.
The shared bar pins its thumb to both ends of the track (`draw_scrollbar` rescales the offset
into the widget's `content - 1` position scale, which a viewport's own scroll --- topping out
at `content - viewport` --- never reaches raw).
Both `Files:` panes (this one and the browser's local pane) refresh themselves when their
directory changes *outside the program*: the tick polls once a second
(`App::refresh_local_listings`, the hotplug cadence), comparing a fresh `readdir` against the
drawn snapshot (`files::listing_changed` --- names, sizes, kinds; an unreadable directory is
never a change, so a transient failure cannot blank the pane, while an error pane always retries)
and swapping silently --- no log line, no cursor churn when nothing moved. The pane also owns the
environment's second persisted fact: the **projects folder** (`[zephyr] projects`, resolved by
`src/backend/zephyr/projects.rs` from the same two config levels, or picked through the same
`DirPicker` with `DirPurpose::Projects` --- existence-only validation, saved via
`settings::save_projects`); accepting a folder chains straight into the project picker. Under
`Capability::ProjectSelect` every project command (build/clean/rebuild/menuconfig/flash) passes a
gate first (`App::require_buildable_project`): the build panel's root must hold build elements
(`projects::is_buildable` --- a `CMakeLists.txt`, `west build`'s one hard requirement); the
`Project path` checklist row doubles as the gate's explanation when the root does not. A root that
passes (the cwd when it *is* a project) builds without ceremony; one that does not is refused with
the reason and the flow opens --- folder picker when unconfigured, else `Overlay::ProjectPicker`
(all immediate subdirectories listed, each marked buildable or not; accepting a directory without
build elements keeps the picker open with the reason). A valid pick is session-only
(`BuildPanel::set_project`: re-roots every command, resets build dir/cached board/last report,
`ProjectOrigin::Picked` vs `WorkingDir` shown on the workspace checklist row) --- nothing is written. The
resolution feeds
the build panel's commands via
`BuildPanel::set_tool_path`/`set_tool_env`: the venv's `west` (`<workspace>/.venv/bin/west`,
executed directly — a venv console script embeds its interpreter path, so no activation is
needed) plus per-command env (`ZEPHYR_BASE` always, so an app outside the workspace still
finds it; `ZEPHYR_SDK_INSTALL_DIR`/`PATH`/`VIRTUAL_ENV` when applicable —
`process::Command::env`). The **project panel** (`src/build.rs`, `src/ui/build.rs`,
`src/app/build_view.rs`, titled "Actions") is buttons only --- no checklist rows
(the board answer comes from the build dir's CMake cache (`cached_board` ---
`<build-dir>/zephyr/CMakeCache.txt`, falling
back to the sysbuild top-level cache; a hand-picked board is session state no finished
command demotes) or a hand pick, and gates
Zephyr Actions/SDK List/Menuconfig/Clean/Build/Rebuild/Flash (the list's order; while a
command runs `Stop` is appended and appears in the pane's three-row footer, always reserved
--- the pane's height never changes when a command starts --- and every other button dims for
as long as the one process slot is occupied (`App::build_action_enabled`: only `Stop` stays
live, so `x`/`Enter` on a dimmed row is a no-op rather than a refused second command); the
footer hugs the stack's bottom rule
and split horizontally: the state line on the left half, `Stop` as its own half-width button
box on the right, same rows, side by side — never a row of the stack; a pane too short for
both pins the box to the bottom and clips the stack above it)
via
`BuildPanel::lifecycle_ready`, the command state pinned to the pane's last line (skipped
when the rows already fill the pane; a *stopped* command reads as success there and on
the Monitor strip --- `BuildReport::cancelled`, the check and success color with the word
"stopped" (`✓ Build stopped`), never the error `✗`/`failed`, since stopping is what the
user asked for --- while `ok` itself stays `false` so the cursor logic does not treat a
stop as a green light for Flash); starting Build/Rebuild shows the Monitor tab
(`MonitorSource::Build`) but keeps focus on the panel with the cursor on `Stop`, moving it to
`Flash` on a success and back to `Build` on a failure or after a `Clean` (which parks it on
`Build` while running — the step a clean clears the way for),
`Clean`
behind `Overlay::ConfirmBuild` (destructive capability). **Menuconfig** (`west build -t
menuconfig`) is interactive ncurses, so it parks a `pending_command` that `main.rs` runs under
`TerminalGuard::suspend` — the same hand-off as `$EDITOR`. The lifecycle always targets the
conventional `build` directory inside the project (implicit in commands; the `BuildDirPicker`
overlay and `set_build_dir` plumbing remain but no row offers them). Commands come from the
backend (`Backend::build_command`,
`src/backend/zephyr/commands.rs`: `west build`[-`b`][`--shield`]/`-t clean`/`--pristine=always`,
`west update`), run with the project root as cwd — the UI never names `west`
(workspace-scoped commands run in the workspace). The panel's `Board` checklist row (under
`Capability::BoardSelect`) opens `Overlay::BoardPicker`: a
filterable list over a background `west boards` fetch (`Backend::board_list_command`, parsed by
`build::parse_boards`); a pick is persisted in the project's `[[project]]` registry entry
(`settings::ProjectEntry`'s `board`/`shield`, written by `App::persist_board_shield`) and reloaded
on every open, outranking the build cache (`BoardOrigin::Config` vs `Picked` vs `Cache` — the row
says which); nothing is ever written into the project directory. The optional `Shield` row right
below (under `Capability::ShieldSelect`) opens `Overlay::ShieldPicker` over a background `west
shields` fetch — same `ListFetch` machinery as boards — with a leading `(none)` row to clear the
pick (the clearing persists too); the saved answer reaches only first-configuration builds as
`--shield` (never an incremental build of an already-configured directory). Both pickers are
full-frame modals (`src/ui/overlay.rs`'s shared `draw_docs_picker`): the window fills the frame
minus one column per side and two rows above and below, and the geometry is one definition in
`ui::layout::docs_picker` shared with the click hit-testing (like the dashboard's own tree).
Under a search line (the icon set's `⌕` magnifier standing in for the old `filter` label) and the
hint, the body fixes its left column at 32 columns — the west list with the row's *preview* (its
picture rendered inline
(`ratatui-image` + `image`; the terminal protocol is probed once in `main.rs` *before* the TUI
takes over, `Picker::from_query_stdio` falling back to halfblocks) below it, sized for the list's
rows rather than the terminal — and gives every remaining column to the Details pane, so
widening the terminal widens only Details. `Tab` swaps the keyboard between the
list and the details (`DocsFocus` on the overlay variants; the focused pane's border takes the
accent, the shared `border_style` grammar): with the details focused the arrows and pgup/pgdn
scroll its text (a `scroll` field), while printable keys keep filtering and `Enter` keeps
applying the list's row — and a click on either pane hands it the keyboard the same way. Both
list and details carry the shared one-column scrollbar when their content overflows (the column
always reserved beside the pane, so nothing reflows when a bar appears), and long labelled rows
(vendor, description) wrap with a hanging indent instead of truncating.
The enrichment layer is `src/board_docs.rs` (`App::docs`): the boards index (one `a.board-card`
per board/shield, joined onto west names by the *documentation directory* in the card href —
`boards/<vendor>/<id>/` for boards, the prefix before the HWMv2 qualifier — never by display
name), per-entry page text (`articleBody` flattened via `html2text`) and picture bytes, fetched by
`reqwest` blocking on dedicated `std::thread`s and drained through `AppEvent::Docs` beside the
process events. The release is the resolved workspace's own `zephyr/VERSION`
(`WorkspacePanel::zephyr_version`), falling back to `/latest/` when that release has no published
docs; everything lands in `$XDG_CACHE_HOME/chiptui/docs/<label>/` (raw HTML/bytes, no serde), so
a later session costs no network. Selection fetches are debounced by the tick
(`drive_docs_selection` → `BoardDocs::note_selection`/`drive`, ~250ms, one request for the row
the cursor rests on), and every miss is a named state on the right pane (`not in the Zephyr docs
index`, `no picture in the docs`, `docs unavailable`) — the west list is the spine and works
fully offline (tests inject `BoardDocs::set_fetch` and never touch the network). A project switch
re-derives both answers for the project switched to (`App::set_project_root`). `Flash`
(`west flash`, the board's own
runner from `runner.yml` — never a hard-coded programmer) sits last under
`Capability::Flash`, always behind `Overlay::ConfirmBuild` (destructive); the dashboard's `x`
routes a build-panel backend there instead of esptool's dialog, and the "Device Info" pane shows
esptool's report for any backend whose board answers the background `chip-id` query (Zephyr
included; the query itself asks first — see the identification-authorization paragraph below —
and without an answer it falls back to its honest placeholder).
The Zephyr monitor is wired too: `m` runs the monitor the board's *platform* calls for
(`Backend::monitor_command` fed a `MonitorContext` --- the selected port, the
auto-detected firmware verdict, the build's board answer and configuration, the
workspace's west invocation --- and returning `Err(reason)` for a refusal the log
names). There is no `west monitor` (Zephyr's own extensions include none), and the
priority is a cohesive environment, not a monitor at any cost: a board read as
MicroPython gets `mpremote` whatever the project is; a Zephyr board whose target is
ESP32 (`is_espressif`: every Espressif SoC Zephyr names carries the `esp32` token,
even mid-name in HWMv2 vendor-qualified targets) gets `west espressif monitor -p
PORT` through the workspace's west (`WestEnv::apply`, cwd = project root) --- the
extension `hal_espressif`'s own `west-commands.yml` ships into every workspace,
wrapping ESP-IDF's idf_monitor (ELF backtrace decoding, baud from the build's runner
configuration, and no port-probing resets since the port rides along); and every
other platform (nRF, STM32, ...) is *refused* --- Zephyr ships no monitor for them,
a generic serial viewer would be the environment's form nowhere, and anything
outside the Zephyr environment is the user's to run. Missing facts (no port, no
workspace west, no board/build answer, ESP-IDF or erased flash) refuse the same
way, each named. On the Monitor tab **`ctrl+]` is ChipTUI's stop chord** (both key
events the byte arrives as: `]` and crossterm's relabeled `5`) --- the session is
cancelled from here, SIGTERM to the child's group, port released --- because the
child's own exit key cannot be relied on: the idf_monitor `west espressif monitor`
runs (1.1, vendored in hal_espressif) hangs on *any* exit key on kernels without
TIOCSTI (>= 6.2) --- its stop path unblocks the blocked key read by injecting a byte
via TIOCSTI and then joins the reader thread, and the removed ioctl leaves that join
stuck forever (reproduced against the vendored code, pty and all; esp-idf-monitor
1.9 replaced the whole mechanism with a busy read for exactly this reason). Every
other key still forwards, so in-tool exits keep working where the child implements
them. The session is
the same PTY session MicroPython uses, and port discovery for a backend without `mpremote
devs` is `device::usb_serial_ports` — a synchronous `/dev` walk (no subprocess) feeding the
same `DeviceState`/picker flow (`App::scan_serial_devices`, `serial_dir` overridable for
deterministic tests; `home_dir` is the equivalent seam for workspace discovery). Connect/
disconnect feedback covers both device-shaped backends (`check_device_hotplug` gates on
`Filesystem || Monitor`, not `Filesystem` alone): the tick poll counts USB serial ports through
the same `serial_dir` walk and rescans on a change — `mpremote devs` for a filesystem backend,
the `/dev` walk for a monitor-only one — but never while an overlay is open (a picker must not
pop over a dialog mid-answer). Any empty or failed rescan routes through `device_disconnected`,
which drops the departed board's esptool identity *and* the firmware-identification state, so a
replug — even on the same port — refills the identity and re-runs the firmware identification
instead of sitting at a stale answer. With that,
Zephyr's Phase 3 surface (detect, board, build, clean, flash, monitor) plus its environment
layer (workspace/venv/SDK resolution, menuconfig, build dirs, `west update`)
is complete; debug/signing remain Roadmap items.

The dashboard has a **declared minimum of 80x32** (`ui::MIN_WIDTH`/`MIN_HEIGHT`), and it is
measured rather than aspirational: the Zephyr action stack is six buttons (one rule per edge,
a divider between each pair, plus the always-reserved three-row footer = 18 rows with the
pane's borders), which leaves row 3 four content rows of log, and the Device Info pane's chip
line drops its crystal/revision suffixes rather than wrapping so the `Firmware` row keeps its
place in the fixed four. `tests/ui_render.rs`'s `the_declared_minimum_fits_the_whole_dashboard`
locks all three; a seventh button breaks it, which is the point --- growing row 2 means moving
the constant with it.

`lib.rs` + `main.rs`: everything except `terminal` and `ui` is testable without a tty, and `ui` is
testable through ratatui's `TestBackend` (see `tests/ui_render.rs`, `tests/files_view.rs`).

`src/app.rs` holds `struct App`, its `new`, the top-level enums (`View`, `Focus`, `LogTab`,
`DevicePaneTab`, `DocsFocus`, `ProjectRow`, `RunState`, `PendingEdit`) and the trivial
accessors; **every other `impl App` block lives in a `src/app/*.rs` submodule** and is
re-exported from here, so `crate::app::Overlay` and friends still resolve. The split is by
subject: `keys` (the keyboard's front door and the dashboard dispatch), `focus` (the `Tab`
tour, the `ctrl+arrow` chords, the pane digits), `events` (`handle`/`on_process`/the hotplug
poll), `theme`, `monitor_view`, plus the older per-subsystem files. A private method that moves
out of `app.rs` needs `pub(super)` to keep the reach it had over its sibling modules.

`tests/common/mod.rs` is the integration suite's shared scaffolding (`fake`, `key`, `render`,
`pump_until`, `settle_while`, `hermetic_app`, …) --- each `tests/*.rs` is its own crate, so a
`mod common;` declaration is what pulls it in, and `#![allow(dead_code)]` inside it is required
because no single test binary uses all of it. Only helpers that were genuinely identical live
there: a variant that differs in *behaviour* (`board_docs_view`'s docs-draining `pump_until`,
`flash_view`'s deliberately tick-free one, the five shapes of `zephyr_app`) stays local, since
collapsing those would change what the tests do rather than how they read.

## Source of truth

- **`SPEC.md`** — product and architecture reference (goals, non-goals, backend model, UI/UX,
  MVP phasing, acceptance criteria).
- **`AGENTS.md`** — implementation rules and development workflow. It applies in full to Claude Code
  as well; read it before modifying anything.

Both are authoritative. Do not restate their content here — keep `SPEC.md` product-focused,
`AGENTS.md` process-focused, and this file limited to what neither covers.

## Commands

The verification set from `AGENTS.md`:

```bash
cargo fmt --check
cargo check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Single test / focused runs:

```bash
cargo test detect::              # tests whose path matches a substring
cargo test --lib                 # in-crate unit tests only
cargo test --test ui_render      # one integration target
```

Running the TUI: `cargo run` from inside a target embedded project directory (the app is
project-aware and searches upward from the current working directory). It exits with a clear error
when stdout is not a tty, so piping it is a safe smoke test.

To eyeball a rendered frame without a terminal, render into `TestBackend` and print
`terminal.backend().to_string()` — that is how the layout was verified.

Toolchain: stable Rust, edition 2024 (`rustc` 1.97 installed system-wide).

## Architecture: the load-bearing constraints

These are the decisions that shape most code, and getting them wrong causes wide refactors later:

1. **Capabilities, not conditionals.** The UI never branches on `is_micropython()` /
   `is_zephyr()`. Backends declare capabilities (build, upload, filesystem, repl, monitor, flash…)
   and views derive available actions from that set. This is what keeps future backends cheap.

2. **Delegate to external CLIs.** MicroPython goes through `mpremote`/`esptool`; Zephyr through
   `west`/`cmake`/`ninja`. Protocol reimplementation needs a demonstrated limitation first.

3. **Commands are structured, never shell strings.** Command construction is centralized (it is the
   main defense against upstream CLI drift) and executed without a shell. Prefer machine-readable
   tool output over parsing human-readable output.

4. **Nothing long-running touches the UI event loop.** Process execution must stream stdout/stderr,
   report exit status, and support cancellation and cleanup while navigation stays live.
   Async is not assumed — a synchronous event-driven design with processes off the UI thread is
   acceptable and preferred if it works.

5. **Interactive sessions (REPL, serial monitor) are a separate mechanism** from ordinary
   line-oriented subprocess capture, isolated from build/flash so a failure cannot corrupt terminal
   state. Terminal restoration must happen on *every* exit path, including panics and errors.

6. **Detection is weighted and explainable, with manual override.** Multiple signals produce a
   confidence score; ambiguity prompts the user rather than guessing. `pyproject.toml` alone must
   never identify a MicroPython project.

## How those constraints are realized today

- **`Backend` is one trait** (`src/backend/mod.rs`) covering both halves of a backend's identity:
  `detect(&DirScan) -> Vec<Signal>` (weighted evidence, never a boolean) and `capabilities()`.
  `SPEC.md` §6 lists `detect()` alongside `build()`/`flash()`; that ordering is circular, because
  detection must run before any backend instance is chosen. Operations will be added as a
  *separate* trait once there is a process manager to run them.
- **`DirScan`** (`src/project/scan.rs`) is an immutable directory snapshot — entry names plus the
  contents of an allowlist (`TEXT_FILES`). Detection never touches the filesystem itself, which is
  why every scoring rule is unit-testable in memory. A backend cannot trigger arbitrary I/O from
  inside a scoring function; if a new rule needs a file's contents, add it to `TEXT_FILES`.
- **Confidence is `score / saturation`, clamped to 1.0.** Each backend declares its own saturation
  so the number stays explainable ("3.0 of the 4.0 points needed"). Thresholds live in
  `src/project/detect.rs`: `MIN_CONFIDENCE` (below = unknown), `AUTO_CONFIDENCE` (above = selected
  silently), `AMBIGUITY_MARGIN` (candidates this close = ask the user). Changing a weight will move
  fixtures across those thresholds — the detection tests assert against them by name.
- **Override is a UI action; config files stay hand-rolled.** `ProjectManager::set_override`
  survives re-detection and keeps the automatic evidence for display. The config files that do
  exist (`chiptui.toml`'s `project_type` + `[zephyr]`, the user config's `[zephyr]`,
  `[projects]` and `[[project]]` blocks) are parsed by tolerant hand-rolled parsers
  (`src/project/config.rs`, `src/settings.rs`) — still no TOML dependency, per the same bias as
  the other one-shape parsers.
- **Nothing is written into a project directory except its own sources.** ChipTUI *reads* a
  project's `chiptui.toml` (and lets it outrank everything) but never creates one; the persisted
  "this directory is a Zephyr project" lives in the user config's `[[project]]` registry
  (`settings::ProjectRegistry`, fed into `ProjectManager::set_known_projects` and consulted by
  `detect_from_known` as `DetectionSource::Registered`, just under `Config`).
  `App::record_open_project` is the one place a project is recorded, and `main.rs` calls it for
  every route. The registry file is rewritten on every project open, so `settings::write_config`
  is atomic (tmp + rename) — it carries `[zephyr]` too. Tests that answer the empty-project
  prompt **must** `set_home_dir` first, or they write into the developer's real config.
- **The renderer publishes `App::log_viewport`** each frame so page-scrolling matches the drawn
  height (and the wrap width, into `LogStore::set_view_width`, so clamping matches too). Long log
  entries wrap with a hanging indent past the stamp (`logs::PREFIX_WIDTH`); scrolling, the clamp and
  the pane's scrollbar all count *visual* (post-wrap) lines, and the buffer is capped at 1_000
  entries. The Monitor tab scrolls the same way (`App::monitor_scroll`), across its four
  consoles (the Terminal tab reuses that scroll *state* but not this renderer — its rows come
  from a `vt100` scrollback, not a document) — anchored to the *top* of the document so live
  output never shifts a scrolled view,
  gutter reserved via block padding, one `render_console`/`window_console` path doing the row
  windowing. Row 3 is one bordered pane whose top border carries the Log/Monitor/Terminal
  tab strip
  (the Ratatui `Tabs` example pattern, `panels::draw_log_tabs`, drawn *after* the pane so it sits on
  the border; `symbols::DOT` divider, the active tab underlined and bold --- cyan when focused,
  default color otherwise --- vs the dim inactive one; `Terminal` is always offered --- a local
  shell is a UI affordance, not a backend operation --- while `Monitor` needs
  `Capability::Monitor`; `App::switch_log_tab` steps one tab per press, clamped, a no-op at
  the ends). At the
  strip's right edge rides the active tab's status (a leading space keeps the dashes off it): for
  Monitor, the source's title with a live icon
  and the output's row count — an animated spinner (`ui::SPINNER`, keyed off `App::ticks`) while a
  command runs, a green ✓ (red ✗ on failure) for the last finished one — plus `↑N` (rows below the
  view) once the user leaves the tail, mirroring Log's indicator; for Terminal, the shell's OSC
  title when it set one (powerlevel10k puts the working directory there) and its program name
  otherwise, a spinner while it runs and the same count/indicator; for Log, the entry count
   (plus `↑N` while scrolled). The panes themselves are untitled (`pane_border`). Rendering is
  otherwise a pure function of `App`.
- **Processes** (`src/process/`): `spawn` returns immediately; a supervisor thread plus two reader
  threads push `ProcessEvent`s into one channel that `main.rs` drains each frame. Two non-obvious
  rules live here. *Killing reaches the child's whole process group* — every piped child is spawned
  in its own group (`Command::to_std`'s `process_group(0)`), and a cancel/timeout signals
  `kill(-pgid, SIGKILL)` (`kill_tree`), because a bare `Child::kill` left `west`'s helpers (cmake,
  the dashboard generator) running after `Stop` reported the command cancelled; a grandchild that
  *escaped* its group (a setsid daemon) still keeps the pipes open, so a killed process reports
  `Finished` **without** waiting for the readers (otherwise the timeout
  deadlocks on the very hang it exists to escape). A *natural* exit instead waits (bounded,
  `READ_DRAIN_TIMEOUT`) on a reader counter before reporting, keeping the invariant that
  "Finished implies all output arrived" without joining threads. And `ProcessManager` is dropped
  with `cancel_all`, so no child keeps a serial port after the TUI exits. A PTY session's *input
  discipline is the app's to set* (`pty_input_mode`, applied to the master fd — master and slave
  share the one line discipline — right after `openpty`, before any byte is written): a fresh pty
  answers the kernel's canonical default, which swallows control bytes before any child reads them
  (Ctrl+R is VREPRINT, Ctrl+C is VINTR — a signal, never delivered — and every typed key echoes a
  second time), so the interactive children that set their own mode when they start
  (miniterm/idf_monitor's `console.setup()`, a shell's readline) were fine *after* startup but lost
  every key typed into the spawn-to-setup window — and `west espressif monitor` spawns idf_monitor
  as a grandchild, a long one. The app applies the mode those children apply for themselves
  (ECHO/ICANON/ISIG/IEXTEN off; byte mappings untouched, so ICRNL still turns Enter's CR into the LF
  REPLs read and OPOST/ONLCR keep breaking bare-`\n` output). `tests/fixtures/bin/stdin-hex` +
  `monitor_control_keys_reach_the_session_as_their_bytes` lock the whole path: Ctrl+T, Ctrl+R,
  Ctrl+] and a plain letter reach the child as 14 12 1d 41, in order, with no terminal mode of the
  child's own.
- **`Browser` emits, never logs** (`src/browser.rs`): device results come back as `Notice` values
  and a `BrowserUpdate` that `App` forwards to the log and to `DeviceState`. That is what makes the
  whole state machine testable without a UI.
- **One `mpremote` at a time.** `mpremote` opens the serial port exclusively, so `Browser` keeps a
  queue and a single `in_flight` request. Listings are cached per `DevicePath` because each `ls`
  costs seconds over serial; `r` invalidates.
- **The device is chosen before it is used.** `open_files`/`App::maybe_scan_devices` (the latter
  run once at startup, from `main.rs`, so the Dashboard header does not sit on "not scanned" until
  the user opens the file browser) only scan; the first `ls` waits for the scan to name a port.
  Letting `mpremote` auto-connect first would talk to whichever board answers — the guess `SPEC.md`
  §8 forbids. `mpremote devs` lists *every* comport (32 legacy `/dev/ttyS*` on a typical Linux box),
  so `parse_devices` keeps only USB devices, matching mpremote's own auto-connect rule.
- **`=` vs `≈` is a real distinction.** `SameSize` means only that lengths match; `Identical`
  requires a sha256 check (`c`), device side via `mpremote fs sha256sum`, local side via `sha2`.
- **Monitor/run output is VT-interpreted** (`src/console.rs`): a REPL echo is not plain text —
  MicroPython's readline redraws the edited line with `\b`, `\x1b[K` and `\x1b[nD`, which a naive
  append-per-char renderer shows as literal `[K` garbage. `LineConsole` keeps the cursor position
  and escape-parser state across PTY chunks and edits the current line accordingly; sequences it
  does not implement (colors, OSC) are consumed, never rendered.
- **The Terminal tab is a terminal emulator, not a console** (`src/app/terminal.rs`,
  `src/ui/terminal.rs`): row 3's third tab spawns `$SHELL` (`/bin/sh` fallback) in a PTY
  (cwd = the project root) as a **login shell** the moment the tab is entered — entering it
  is the whole start gesture (`Command::as_login_shell`, answered by the PTY spawner's
  `pty_command`: portable-pty's default program, empty argv, which resolves the shell itself
  and execs it with `argv[0] = -<basename>`, so the shell sources its login files
  (`.zprofile`/`.profile`) and the tab carries the full login environment a fresh terminal
  window has — the parent's own variables were always inherited). The shell is also *born into the
  resolved workspace's environment* (`App::terminal_west_env`, the same `WestEnv` env half the build
  panel's commands get: `ZEPHYR_BASE`, `ZEPHYR_SDK_INSTALL_DIR`, `VIRTUAL_ENV` and a venv-first
  `PATH` — no workspace resolved, no injection), so `west`/`python` typed in the tab mean what they
  mean in the Actions pane; a process cannot have its environment edited from outside, so
  `apply_west_env` compares the fresh env against `terminal_shell_env` (what the live session was
  born with) and restarts the shell (`restart_terminal_shell`, same trade as `r`) when a workspace
  resolves or moves under it — an unchanged env restarts nothing. It deliberately does
  **not** reuse the Monitor's machinery. `LineConsole` is a
  single-line editor (`{ parse, col }`, implementing LF/CR/BS/`CSI K`/`CSI D`/`CSI C` and
  dropping every SGR), which is right for MicroPython's readline redraw and wrong for a
  shell: a real prompt — powerlevel10k, here — paints itself in 256 colours, redraws by
  moving the cursor *up*, and places a right-hand segment with `CSI n C`; `vim`/`less`/
  `htop` take the alternate screen. So the tab owns a `TerminalSession`: a `vt100::Parser`
  cell grid fed the PTY's **raw bytes** (`ProcessEvent::Bytes`, selected by
  `ProcessManager::spawn_pty_raw` — the decoded `Output` path's per-1 KiB
  `from_utf8_lossy` turns any character straddling a read boundary into U+FFFD, and a
  powerline separator is three bytes), rendered by `tui_term::widget::PseudoTerminal`.
  **The palette deliberately does not reach the content**: every cell carries the shell's
  own colour and an unstyled cell maps to `Color::Reset`, which is what the same shell
  shows outside ChipTUI — `PseudoTerminal` ignores its own `.style()` anyway, so the
  behind-a-dialog `DIM` is patched over the drawn cells afterwards. `vt100` is a screen
  with no way to write back, so `CSI n`/`CSI c` (a prompt measuring itself sends `CSI 6 n`
  and *blocks* on the answer) reach `TerminalCallbacks::unhandled_csi`, which composes
  the reply that `App::feed_terminal` puts straight back into the PTY in the same turn —
  the callbacks also carry the OSC title, which outranks the program name on the tab
  strip. The pane sizes both halves to itself (`App::resize_terminal`, called every frame
  from `ui::terminal::draw` and a no-op unless the size changed: the emulator's
  `set_size` plus `ProcessManager::resize_pty`, which needs the PTY master kept alive in
  `Running` — dropping it, as `spawn_pty` used to, puts SIGWINCH permanently out of
  reach). Scrolling drives `Screen::set_scrollback` (1 000 rows; disabled under the
  alternate screen, whose viewport belongs to the program). While the tab holds focus the
  shell owns the keyboard (`is_terminal_active`, checked in `on_key` before everything
  else, so `ctrl+c` reaches the shell's foreground job instead of quitting) through
  `terminal::terminal_key_bytes` — a *second* encoder beside `key_to_bytes`, which stays
  the monitor's: this one adds Meta as an ESC prefix (`alt+f`, `alt+backspace` — zsh's
  word motions were unreachable), the editing cluster, F1–F12, xterm-modified arrows, and
  DECCKM-aware arrows (`ESC O A` vs `ESC [ A`, read off `screen().application_cursor()`).
  `shift+pgup`/`shift+pgdn` are kept by the pane for the scrollback the shell would
  otherwise swallow, and any keystroke that does reach the shell snaps back to the tail.
  Bracketed paste is enabled in `terminal::init`, delivered as `AppEvent::Paste` and
  framed by `paste_into_terminal` only when the child asked for it. `ctrl+]` — the
  monitor's own chord, which crossterm relabels Ctrl+5, and which a shell has no use for
  — *detaches*: the shell keeps running and streaming into the tab while the keyboard
  returns (`terminal_detached`; switching back re-attaches, a clamped strip step never
  does, so `→` on the Terminal tab is a no-op). The shell's own `exit`/`ctrl+d` finishes
  the process, frees the keyboard and writes the `[shell …]` epitaph *through* the
  emulator so it lands in the grid; `r` starts another without leaving the tab, and
  entering the tab again starts a fresh session, grid cleared. `App::set_terminal_tool`
  is the test seam pointing the tab at a fake instead of the developer's real shell; the
  child's `TERM` is pinned to `xterm-256color` (the honest promise for `vt100`'s
  capability set — inheriting `xterm-ghostty` advertises protocols nothing here can
  answer) with `COLORTERM=truecolor`, which survives end to end.
- **A running script is never interrupted silently** (`src/app/probe.rs`, `Browser::set_interrupt_gate`):
  mpremote Ctrl-C's whatever is executing to enter raw REPL for *any* device command, so before
  the first listing on a selected port ChipTUI opens a short `mpremote repl` PTY and classifies
  what it sees (`device::monitor_script_activity`: a `>>> ` prompt means idle, output with no
  prompt means running, banners filtered, silence inconclusive — a probe verdict is a *belief*,
  `ScriptState`, reset whenever the selected port changes). A "running" belief turns on the
  browser's interrupt gate: queued requests are held (`held_for_interrupt`) until the user
  confirms; accepting marks the script stopped, resumes the queue and arms the restore question
  for when it drains. Restore deliberately uses no-follow commands (`mpremote reset`, `exec
  --no-follow "import main"`) because a `soft-reset` leaves the script *stopped* — raw-REPL
  reboots skip `main.py`. The monitor updates the same belief live; a script that swallows
  Ctrl-C surfaces as the classified `ReplBlocked` error with its way out. The whole
  identification chain is **authorized before it starts** (`App::identify`, an
  `IdentifyAuth` of Pending/Granted/Declined per port): reading a board's chip
  and firmware means esptool resetting it into its bootloader, a stop/restart
  of whatever it runs, so every device selection (the startup scan's
  auto-pick, a picker choice, `r`'s re-offer after a decline) logs
  `device connected on PORT` and opens `Overlay::ConfirmIdentifyDevice`
  (`App::request_device_identify`/`maybe_ask_identification`, tick- and
  process-polled, never over another overlay or while the probe still holds
  the port) with **No as the default** — declining skips identification for
  that port entirely (the pane reports no verdict --- the not-identified
  hint while nothing was read, `Firmware: undefined` once a MAC exists; the
  listing proceeds and mpremote answers for the board) and a yes while a
  script is believed running *is* the accepted interruption (script marked
  stopped, restore question armed — the same semantics
  `ConfirmInterruptDevice`'s yes has, so the two never stack). The offer is
  also user-initiated: `ctrl+r` is a dashboard-wide chord and `Enter` on an
  empty Device Info pane its in-pane twin (`App::open_identification_question`,
  which re-arms `Pending` even for a port already granted — a fresh capture —
  and the accept path resets `firmware_check_port` so the firmware read
  re-runs too); while the pane is empty it says
  `device connected --- not identified` / `ctrl+r, or Enter here, stops it and
  reads its data` instead of the old shrug, and `Enter` reverts to
  copy-the-MAC once a MAC exists. The background
  `esptool chip-id` identity query (`FlashPanel::query_device_info`, chip not flash — the
  connection banner's identity half; flash geometry stays in the Flash view) runs *first* on a
  newly selected device, and only after that yes: the first device listing is held
  behind the question itself while it is open
  (`App::hold_root_listing_for_chip_identity`/`held_root_listing`), then behind the query
  (released by
  `FlashUpdate::background_chip_query_finished` or, if the query can never start, by the tick's
  `DeferredQuery::Dropped`), so the port changes hands probe → esptool → mpremote instead of
  being contended. The query is also gated on the script belief
  (`maybe_run_deferred_flash_query`, tick-polled): esptool resets the board to read the chip,
  so it waits for an idle device, a closed overlay and a free port instead of racing a restore
  decision or silently resetting a script the user just declined to interrupt. The identity
  question is backend-agnostic where the board can answer it: a non-filesystem backend's
  selection (`scan_serial_devices`' auto-pick, `apply_device_picker`) defers the same query ---
  Zephyr runs on ESP32 boards whose esptool runner answers `chip-id`; on anything else it fails
  harmlessly and the pane keeps its honest placeholder. Such a backend's `FlashPanel` exists
  purely as the query engine (`ensure_flash_panel` skips the `firmware/` dir for a backend
  without `DeviceInfo`/`EraseFlash`), never lists (no browser is created), and 'x' still routes
  to the build panel's flash. A successful identity query chains into the firmware
  *identification* read (`arm_firmware_check`, once per selected port — `App::firmware_check_port`):
  `esptool read-flash 0x0 0x20000` (the bootloader region,
  the partition table and the start of the app area — the Zephyr/MCUboot banner lives in the
  bootloader, below 0x8000 — into a temp file) rides the same authorization the chip query's
  yes granted: esptool already reset the board once to read the chip, so the read adds
  no interruption the chain's own question has not covered. (The two flows that re-identify
  without re-asking are the user's own flashing — erase/write, `west flash`, through
  `reidentify_firmware_after_flash`, and `confirm_erase_for_micropython`, whose erase confirm
  grants the identification outright.) The first
  listing is held behind it too (`hold_root_listing_for_firmware`, driven by
  `drive_held_root_listing` whenever a link of the chain reports back), because only MicroPython
  exposes a filesystem mpremote can walk: a verdict of Zephyr or ESP-IDF refuses the listing with
  `cannot read files — the device runs X, not MicroPython` in the device pane, and erased flash
  says to flash MicroPython first (`non_micropython_block_reason`); MicroPython releases the
  listing, and so does a board that could not be asked at all (a failed chip query — no
  esptool-backed bootloader — or a refused read: mpremote then fails on its own, which is what
  `Overlay::ConfirmEraseForMicroPython` exists for). A board the probe believes is *running a
  script* holds the listing behind the same chain instead of queueing it in the browser: a foreign
  firmware printing its boot banner (any auto-reset ESP32) is indistinguishable from a busy
  script at probe time, so the identification question is what opens for the held listing —
  its one yes covers the whole chain, marking the script stopped and letting chip-id →
  read-flash run to a verdict (which then releases *or refuses* the listing), declining drops
  the listing (`decline_held_listing`) with the same cancelled-script pane state the browser's
  held requests get; a foreign verdict cancels the restore question (`restore_pending`), since
  the "script" was the firmware's boot banner, and `maybe_offer_restore` waits for the whole
  chain to drain before opening, so the queries no longer guard on it (doing so deadlocked the
  chain: the restore question waits for the identification that waits on the belief the question
  itself would clear). The bytes are parsed by
  `src/firmware_id.rs` (Zephyr's `mcuboot`/`slot0_partition` partition labels decide first, then
  case-insensitive banner strings — MicroPython-on-Zephyr reads as Zephyr, the structural
  truth — then the `esp_app_desc_t` magic `0xABCD5432` scanned in the *app* region names a
  plain ESP-IDF app; bootloader bytes never classify anything, since the ESP-IDF bootloader
  is shared by all three firmwares), and the answer lands on its own row of the Device Info
  pane, directly under the MAC, as `Firmware: MicroPython|Zephyr|ESP-IDF`
  (`DeviceDetails::firmware`) --- the verdict carries the version the *same read* found
  (`firmware_id::version`: the `MicroPython v1.28.0 on …` / `*** Booting Zephyr OS build
  v4.0.0 ***` banner strings, or for a plain IDF app the `esp_app_desc_t`'s stamped fields,
  where the IDF build's version outranks a project's), so the row reads e.g.
  `Firmware: Zephyr v4.0.0` / `Firmware: ESP-IDF v5.3.1`; a MicroPython verdict whose read
  found no version string falls back to the REPL-banner fact (`App::mpy_version`), and a
  firmware that names no version stays bare (labels identify without one; a guessed version
  is worse than none). One real layout needs a second answer the identification window alone
  cannot give: a Zephyr *simple boot* image is one contiguous XIP image whose application
  banner lives far past that window, and how far tracks the app's own size (on real hardware, an
  ESP32-C3: a bare sample's kernel strings sit at 0xa00, banner at 0x6053c; a graphics-heavy one
  --- a round display driven by LVGL, same chip --- pushed the banner to 0xd06a8, past even a
  widened 1 MiB byte-window guess). Guessing a bigger window only ever buys one more size before
  the next app outgrows it, so a versionless Zephyr verdict tries a live answer first
  (`App::start_version_capture`, `src/app/version_capture.rs`): `esptool` has already reset the
  board back into run mode to perform the identification read, so the app reboots and prints its
  own boot banner on the UART regardless of image size or where in flash it physically sits ---
  the same trick `App::mpy_version` already uses for MicroPython's live REPL banner, generalized
  to Zephyr's own platform monitor (`west espressif monitor`, not `mpremote`) instead of a second
  flash read. The capture is a short-lived, self-closing PTY session modeled on
  [`DeviceProbe`](`src/app/probe.rs`) --- never the interactive Monitor tab's
  `device_monitor_process` (a background courtesy must not hijack focus, the log tab or the
  monitor source) --- that feeds decoded output into a `LineConsole` and re-scans it with
  `firmware_id::version` after every chunk, cancelling itself (`ProcessManager::cancel`, the same
  host-side stop `ctrl+]` uses, never a written escape byte, since idf_monitor's own exit key
  hangs on kernels without `TIOCSTI`) the instant the banner names a version. It only ever runs
  when `Backend::monitor_command`'s own prerequisites are already met (a resolved workspace, a
  known Espressif board, a configured build directory --- the same facts `App::open_monitor`
  needs, extracted once into `App::monitor_facts`/`MonitorFacts` so both callers ask the backend
  the identical question) and tried once per port (`App::version_capture_port`); a backend with
  those facts unmet, or a capture that times out with no match, falls through to the flash-byte
  hunt exactly as before --- a hybrid, not a replacement, since identification must keep working
  with nothing but a selected port. That fallback arms `FlashPanel::query_firmware_version`
  (`version_hunt_pending`), a follow-up `read-flash 0x20000 0x100000` that only dates the
  standing verdict and never re-judges it (`firmware_id::HUNT_OFFSET`/`HUNT_SIZE`,
  `apply_version_from`) --- driven through the same tick-polled deferral as the other background
  queries (`App::maybe_run_deferred_version_hunt`), refused under an open overlay, dropped with
  the identity it belonged to, and inert by design for ESP-IDF (the descriptor the window already
  read is its only version source), `undefined` when the read failed or recognized nothing — with
  one distinction: a window that is entirely
  `0xFF` is erased flash, reported as `none (erased flash)` in warning color (`firmware_id::
  classify` → `FirmwareVerdict::Erased`), because "no firmware installed" is an answer, not
  an unknown; an empty/truncated read deliberately does not qualify as erased. The read
  itself waits for a free port like the chip query
  (`maybe_run_deferred_firmware_check`) — but not for a script believed running: by the time it
  is armed the chip query has already reset the board. Switching devices clears the old board's
  answer and re-arms the read; a successful erase/write-flash invalidates it
  (`FlashUpdate::firmware_invalidated`) and re-identifies directly --- the same reload `west flash`
  gets (`BuildPanel::take_flash_finished` and the esptool finish both funnel into
  `App::reidentify_firmware_after_flash`): the stale verdict, the REPL-banner version, the probe
  and the script belief are dropped and the read re-arms, because no listing is coming that would
  re-ask on its own (the Zephyr side never has one; the MicroPython pane is usually already
  listed) --- the new verdict arrives on its own once the port frees. `r` on the device pane
  (`reload_device_pane`) still re-runs the identification whenever MicroPython is not confirmed —
  the manual recovery path after re-flashing. The features row is
  one line and the crystal
  rides
  the chip's own row, so the MAC and Firmware rows keep their fixed place in the pane's
  four rows (whose labels are all capitalized — `Chip`/`Crystal`/`Features` beside the `MAC`
  and `Firmware` that always were). That row no longer *truncates* esptool's list, though — a
  plain ESP32's real line (`Wi-Fi, BT, Dual Core + LP Core, 240MHz, Coding Scheme None`, copied
  verbatim from an installed esptool's `targets/esp32.py::get_chip_features` — `AGENTS.md`'s
  "reproduce the tool, not the belief about it" caught a fabricated version of this exact line
  reordering the row on a real board) runs well past the 27 columns the row has at `MIN_WIDTH`,
  so the head was all that ever showed and an ESP32-S3's PSRAM never did.
  `esptool::features::compact` (`src/backend/micropython/esptool/features.rs`) re-expresses it
  as priority-ordered entries instead — radios first (`WiFi`/`WiFi6` from esptool's hyphenated
  `Wi-Fi`/`Wi-Fi 6`; `BT` plain or `BLE5` from esptool's own single already-merged `BT`/`BT 5
  (LE)` token — never two separate tokens to merge; `15.4` for `IEEE802.15.4`), then cores and
  clock fused into one (`Dual Core + LP Core` + `240MHz` → `2x240MHz`, the LP-core detail
  dropped), then embedded memory (`4MB` bare for flash, `PSRAM8MB` named so two sizes are not a
  riddle, the vendor part number dropped), then anything unrecognised verbatim and muted — and
  `ui::panels::features_spans` drops whole entries off the *tail* until they fit, the rule the
  chip line above already follows with its suffixes. So a narrow row loses the `Coding Scheme`
  trivia, never the radios. The shortening is deliberately **verbal, never a glyph**: an icon
  per feature was built and reverted — a symbol standing in for `WiFi` costs the reader more
  than the three columns it saves, and there is no non-PUA character for Bluetooth at all. It
  also keeps every generated entry ASCII, which is what makes the pane's `chars()` budget exact
  (there is no `unicode-width` in this crate, so a wide glyph would count one and draw two);
  `only_ascii_is_ever_generated` locks that. Nothing is lost to the compaction:
  `FlashPanel::complete` logs the raw line whole (`chip features: …`, only when it *changed* —
  every esptool command that reaches the board reprints the same banner), the pairing
  `short_version` and the Firmware row already use.

## Testing

The normal suite must run without hardware. `tests/fixtures/bin/` holds fake executables — a
`mpremote` reproducing the 1.28 output formats, plus `slow` and `noisy` for timeout/cancel/stderr
paths, `mpremote-busy-board`/`mpremote-quiet-board` for a board stuck in a printing/silent
blocking loop (see `tests/busy_device.rs`), and `bursty` guarding output-before-`Finished`
ordering. Tests reference them by **absolute path** (`env!("CARGO_MANIFEST_DIR")`) and point the
browser at them with `Browser::set_tool_path`; nothing mutates `PATH`, so tests stay parallel-safe.
Add fakes for `esptool`, `west`, `cmake` and `ninja` the same way. Hardware tests stay separate and
explicitly documented.

**A fake must reproduce the tool, not the belief about it.** The `west` fixture once treated
`sdk install -d DIR` as "install into DIR" — which is what the flag *looks* like it means, and
what the code under test assumed — so a completely broken invocation passed the suite for a
whole round while failing on real hardware. Where a flag's meaning is load-bearing, read the
tool's source before writing the fake, and make the fake **reject** the wrong form (that
`west` now exits 1 on `-d`, the way real west does) so reintroducing it breaks a test.

If you change a fixture's canned sizes or digest, `tests/files_view.rs` asserts against them — the
`same.py` digest there is the real sha256 of the local fixture's contents, which is what makes the
`Identical` path meaningful.
