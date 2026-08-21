//! Documentation metadata for the Zephyr board/shield pickers.
//!
//! `west boards`/`west shields` name every target the workspace can build
//! but say almost nothing *about* them. docs.zephyrproject.org publishes
//! the other half --- one card per board with vendor, architecture, a
//! picture, and a per-board documentation page. This module fetches that
//! index (and, per selected entry, the picture and the page text) on
//! background threads and joins it onto the west lists.
//!
//! The join key is the board's directory, not its display name: the docs
//! card for `nrf52840dk/nrf52840` is named "nRF52840 DK" but lives at
//! `boards/nordic/nrf52840dk/doc/index.html`, and that path segment is
//! exactly the west name's prefix before the HWMv2 qualifier. Shields
//! document the same way under `boards/shields/<id>/`.
//!
//! The docs site is versioned (`/4.1/`, `/latest/`), and the workspace
//! owns its version (`zephyr/VERSION`), so the index is fetched from the
//! workspace's own release first and falls back to `latest` when that
//! release has no published docs --- the picker must never offer
//! documentation for boards the installed tree cannot build, which is why
//! the *list* stays `west boards` and only the enrichment is online.
//!
//! Like the reference browser this was modeled on, raw fetches are cached
//! on disk (the parsed index HTML, each entry's extracted text and image
//! bytes) under a per-version directory, so a later session --- or a
//! re-selected board --- costs nothing. The transport is a plain blocking
//! HTTP client on dedicated threads (`std::thread` + channel, drained by
//! the main loop exactly like `ProcessManager`'s events); no async runtime
//! enters the app.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use image::DynamicImage;

/// Re-exported so callers (and tests) can build the decoded pictures the
/// events carry without depending on the image crate a second time.
pub use image;

/// Where the Zephyr documentation lives.
pub const DOCS_BASE: &str = "https://docs.zephyrproject.org";

/// The label used when no workspace version names a release.
pub const LATEST: &str = "latest";

/// One board request must not hold a picker hostage for minutes.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// How much of a board's documentation page the details pane keeps. The
/// pages run long (pin tables, programming instructions); the pane wants
/// the overview, which the front of the page holds.
const DETAIL_LIMIT: usize = 4_000;

/// The transport seam: URL in, body out. Production is the blocking HTTP
/// client below; tests point it at fixture files. Returning `Err` is the
/// caller's "unavailable" --- every consumer here degrades, never crashes.
pub type Fetch = Arc<dyn Fn(&str) -> io::Result<Vec<u8>> + Send + Sync + 'static>;

/// One card of the boards index: a board or a shield, identified by its
/// documentation directory (`id`), which is what joins it to a west name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocEntry {
    /// The documentation directory name, e.g. `nrf52840dk` or
    /// `mikroe_accel13_click` --- the join key for a west list entry.
    pub id: String,
    /// The human-facing name from the card ("nRF52840 DK").
    pub name: String,
    pub vendor: String,
    /// The card's architecture tag; shields carry none.
    pub arch: String,
    /// The entry's documentation page, absolute.
    pub href: String,
    /// The card's picture, absolute; many cards have none.
    pub img_url: Option<String>,
    pub shield: bool,
}

/// What a fetch produced for the main loop to apply. These travel over the
/// module's channel and are applied by [`BoardDocs::apply`], the same
/// event-driven split `Browser` uses --- the state machine stays testable
/// without threads. `PartialEq` stops at the decoded picture (float pixels
/// are `PartialEq` but never `Eq`), which is all `AppEvent` asks of it.
#[derive(Debug, Clone, PartialEq)]
pub enum DocsEvent {
    IndexLoaded {
        label: String,
        entries: Vec<DocEntry>,
    },
    IndexFailed {
        label: String,
        error: String,
    },
    Entry {
        id: String,
        detail: Option<String>,
        image: Option<DynamicImage>,
    },
}

/// The index fetch's lifecycle, mirroring `build::ListState`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexState {
    Idle,
    Loading,
    Loaded,
    Failed(String),
}

/// The workspace name of a west board target reduced to its docs id: the
/// prefix before the HWMv2 qualifier (`nrf52840dk/nrf52840` ->
/// `nrf52840dk`). A qualifier-less name (HWMv1, or the unqualified HWMv2
/// default) is already the id.
pub fn board_doc_id(west_name: &str) -> &str {
    west_name.split('/').next().unwrap_or(west_name)
}

/// The boards index URL for a docs release label (`"4.1"`/`"latest"`).
pub fn index_url(label: &str) -> String {
    format!("{DOCS_BASE}/{label}/boards/index.html")
}

/// Turns an index-page-relative reference (`../boards/...`,
/// `../_images/...`) into an absolute URL under the same release. The
/// cards always use `../` references off `/boards/`; anything else is
/// passed through untouched.
fn resolve_relative(reference: &str, label: &str) -> String {
    match reference.strip_prefix("../") {
        Some(rest) => format!("{DOCS_BASE}/{label}/{rest}"),
        None => reference.to_string(),
    }
}

/// Reads the boards index page into entries, tolerating odd rows the way
/// the west-list parsers do: a card without a usable board directory is
/// skipped, not fatal --- the index is 1400+ cards and one strange href
/// must not empty it.
pub fn parse_index(html: &str, label: &str) -> Vec<DocEntry> {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);
    let Ok(cards) = Selector::parse("a.board-card") else {
        return Vec::new();
    };
    let Ok(vendors) = Selector::parse("div.vendor") else {
        return Vec::new();
    };
    let Ok(pictures) = Selector::parse("img.picture") else {
        return Vec::new();
    };
    let Ok(names) = Selector::parse("div.board-name") else {
        return Vec::new();
    };
    let Ok(archs) = Selector::parse("div.arch") else {
        return Vec::new();
    };

    let mut entries = Vec::new();
    for card in document.select(&cards) {
        let href = card.value().attr("href").unwrap_or_default();
        let Some(id) = href_board_id(href) else {
            continue;
        };
        let shield = href.contains("/shields/")
            || card
                .value()
                .attr("class")
                .is_some_and(|c| c.contains("shield"));
        let img_url = card
            .select(&pictures)
            .next()
            .and_then(|img| img.value().attr("src"))
            .map(|src| resolve_relative(src, label));
        entries.push(DocEntry {
            id: id.to_string(),
            name: card
                .select(&names)
                .next()
                .map(|name| name.text().collect::<String>().trim().to_string())
                .unwrap_or_default(),
            vendor: card
                .select(&vendors)
                .next()
                .map(|vendor| vendor.text().collect::<String>().trim().to_string())
                .unwrap_or_default(),
            arch: card
                .select(&archs)
                .next()
                .map(|arch| arch.text().collect::<String>().trim().to_string())
                .unwrap_or_default(),
            href: resolve_relative(href, label),
            img_url,
            shield,
        });
    }
    entries
}

/// The board directory out of a card href: the second segment after
/// `boards/` --- `.../boards/nordic/nrf52840dk/doc/...` (vendor, board) and
/// `.../boards/shields/mikroe_.../doc/...` (`shields`, shield) share the
/// shape. `None` when the href is not a board page at all.
fn href_board_id(href: &str) -> Option<&str> {
    let after_boards = href.split("boards/").nth(1)?;
    after_boards.split('/').nth(1).filter(|id| !id.is_empty())
}

/// The text the details pane shows for one board's documentation page: the
/// `articleBody` div flattened to wrapped plain text. Anything
/// unrecognized yields an empty string --- "no details" is a state the
/// pane can show, not an error worth surfacing elsewhere.
pub fn parse_detail(html: &str) -> String {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);
    let Ok(body) = Selector::parse("div[itemprop=\"articleBody\"]") else {
        return String::new();
    };
    let Some(main) = document.select(&body).next() else {
        return String::new();
    };
    let text = html2text::from_read(main.inner_html().as_bytes(), 72).unwrap_or_default();
    let text = collapse_blank_lines(&text);
    text.chars().take(DETAIL_LIMIT).collect()
}

/// Drops runs of blank lines the HTML-to-text conversion leaves behind
/// (empty `figure`/`aside` blocks), so the pane's first screen is content.
fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blanks = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            blanks += 1;
        } else {
            blanks = 0;
        }
        if blanks <= 1 {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The production transport: a shared blocking client with a timeout, so
/// one wedged request cannot pin a picker thread forever. The client is
/// built once; `blocking` reqwest already serializes per-connection state
/// internally.
pub fn http_fetch() -> Fetch {
    let client = reqwest::blocking::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());
    Arc::new(move |url: &str| {
        let response = client
            .get(url)
            .send()
            .map_err(io::Error::other)?
            .error_for_status()
            .map_err(io::Error::other)?;
        let bytes = response.bytes().map_err(io::Error::other)?;
        Ok(bytes.to_vec())
    })
}

struct PendingEntry {
    id: String,
    since: u64,
}

/// The picker's documentation half: index lifecycle, per-entry results,
/// the debounced selection that drives them, and the disk cache --- a pure
/// state machine plus a channel, applied and driven by `App` exactly like
/// process events.
pub struct BoardDocs {
    fetch: Option<Fetch>,
    cache_dir: Option<PathBuf>,
    label: String,
    state: IndexState,
    entries: Vec<DocEntry>,
    by_id: HashMap<String, DocEntry>,
    index_started: bool,
    index_in_flight: bool,
    /// A versioned release had no published docs; `latest` was tried once.
    latest_fallback_tried: bool,
    sender: Sender<DocsEvent>,
    receiver: Receiver<DocsEvent>,
    entry_in_flight: HashSet<String>,
    /// Entries already fetched (or known to have nothing to fetch): keeps a
    /// board without a picture from being re-requested on every reselect.
    entry_done: HashSet<String>,
    pending: Option<PendingEntry>,
    last_requested: Option<String>,
    pub details: HashMap<String, String>,
    images: HashMap<String, DynamicImage>,
    protocols: HashMap<String, ratatui_image::protocol::StatefulProtocol>,
    picker: ratatui_image::picker::Picker,
}

impl Default for BoardDocs {
    fn default() -> Self {
        Self::new()
    }
}

impl BoardDocs {
    pub fn new() -> Self {
        let (sender, receiver) = channel();
        Self {
            fetch: None,
            cache_dir: None,
            label: LATEST.to_string(),
            state: IndexState::Idle,
            entries: Vec::new(),
            by_id: HashMap::new(),
            index_started: false,
            index_in_flight: false,
            latest_fallback_tried: false,
            sender,
            receiver,
            entry_in_flight: HashSet::new(),
            entry_done: HashSet::new(),
            pending: None,
            last_requested: None,
            details: HashMap::new(),
            images: HashMap::new(),
            protocols: HashMap::new(),
            // The font-size probe queries the terminal, which only the
            // binary may do (before the TUI takes over); halfblocks work
            // everywhere and are the correct default for every other
            // construction site (tests included).
            picker: ratatui_image::picker::Picker::halfblocks(),
        }
    }

    pub fn set_fetch(&mut self, fetch: Fetch) {
        self.fetch = Some(fetch);
    }

    pub fn set_cache_dir(&mut self, dir: impl Into<PathBuf>) {
        self.cache_dir = Some(dir.into());
    }

    /// The binary's override of the rendering protocol: a picker probed
    /// against the real terminal (kitty/sixel when supported) beats the
    /// halfblocks default.
    pub fn set_image_picker(&mut self, picker: ratatui_image::picker::Picker) {
        self.picker = picker;
    }

    pub fn state(&self) -> &IndexState {
        &self.state
    }

    /// The docs release label in use (for the pane's provenance line).
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn entry(&self, id: &str) -> Option<&DocEntry> {
        self.by_id.get(id)
    }

    /// How many boards/shields the loaded index carries, for the pane.
    pub fn index_counts(&self) -> (usize, usize) {
        let boards = self.entries.iter().filter(|e| !e.shield).count();
        let shields = self.entries.len() - boards;
        (boards, shields)
    }

    /// Whether `id`'s picture/text are still on their way. The pane shows
    /// this instead of a stale "no picture" while a fetch runs.
    pub fn entry_loading(&self, id: &str) -> bool {
        self.entry_in_flight.contains(id)
    }

    /// Whether `id`'s fetch already concluded (with whatever it found ---
    /// including "no picture in the docs"): the difference between "still
    /// fetching" and a final answer, for the pane's placeholders.
    pub fn entry_settled(&self, id: &str) -> bool {
        self.entry_done.contains(id)
    }

    /// Whether `id`'s picture has arrived (decoded and renderable) --- the
    /// state tests wait on and the pane renders.
    pub fn has_image(&self, id: &str) -> bool {
        self.images.contains_key(id)
    }

    /// The renderable picture for `id`, creating its resize protocol on
    /// first use (the protocol owns the fit-to-area state; re-creating it
    /// per frame would restart that work every draw).
    pub fn protocol_for(
        &mut self,
        id: &str,
    ) -> Option<&mut ratatui_image::protocol::StatefulProtocol> {
        if !self.images.contains_key(id) {
            self.protocols.remove(id);
            return None;
        }
        if !self.protocols.contains_key(id) {
            let image = self.images.get(id)?.clone();
            let protocol = self.picker.new_resize_protocol(image);
            self.protocols.insert(id.to_string(), protocol);
        }
        self.protocols.get_mut(id)
    }

    /// Starts the index fetch unless one already ran. The disk cache is
    /// honored synchronously --- a second session opens the picker with
    /// the index already in hand and no request at all.
    pub fn ensure_index(&mut self, label: &str) {
        if self.index_started || self.fetch.is_none() {
            return;
        }
        self.index_started = true;
        self.label = label.to_string();
        if let Some(cache) = self.cache_dir.as_ref()
            && let Ok(html) = std::fs::read_to_string(cache.join(&self.label).join("index.html"))
        {
            let entries = parse_index(&html, &self.label);
            let _ = self.sender.send(DocsEvent::IndexLoaded {
                label: self.label.clone(),
                entries,
            });
            return;
        }
        self.state = IndexState::Loading;
        self.index_in_flight = true;
        let url = index_url(label);
        let cache_path = self
            .cache_dir
            .as_ref()
            .map(|dir| index_cache_path(dir, &self.label));
        spawn_fetch(
            label.to_string(),
            url,
            cache_path,
            self.fetch.clone(),
            self.sender.clone(),
            move |label, html| {
                let entries = parse_index(&html, &label);
                DocsEvent::IndexLoaded { label, entries }
            },
        );
    }

    /// Records the picker's current selection, arming the debounced fetch:
    /// the reference browser waits 300ms after the cursor stops, and one
    /// tick (250ms) is this app's own equivalent --- fast scrolling must
    /// not spawn a request per row.
    pub fn note_selection(&mut self, id: Option<&str>, ticks: u64) {
        let Some(id) = id else {
            self.pending = None;
            return;
        };
        if self.last_requested.as_deref() != Some(id) {
            self.last_requested = Some(id.to_string());
            self.pending = Some(PendingEntry {
                id: id.to_string(),
                since: ticks,
            });
        }
    }

    /// Fires the armed selection once a tick has passed. Split from
    /// [`Self::note_selection`] so the app drives both from its own tick.
    pub fn drive(&mut self, ticks: u64) {
        if let Some(pending) = &self.pending
            && ticks.saturating_sub(pending.since) >= 1
        {
            let id = pending.id.clone();
            self.pending = None;
            self.request_entry(&id);
        }
    }

    fn request_entry(&mut self, id: &str) {
        if self.fetch.is_none() || self.entry_done.contains(id) || self.entry_in_flight.contains(id)
        {
            return;
        }
        let Some(entry) = self.by_id.get(id).cloned() else {
            return;
        };
        if self.details.contains_key(id)
            && (self.images.contains_key(id) || entry.img_url.is_none())
        {
            // Everything the entry has to offer is already here.
            self.entry_done.insert(id.to_string());
            return;
        }
        self.entry_in_flight.insert(id.to_string());
        spawn_entry_fetch(
            entry,
            self.label.clone(),
            self.fetch.clone(),
            self.cache_dir.clone(),
            self.sender.clone(),
        );
    }

    /// Non-blocking drain of finished fetches, for the main loop to feed
    /// back through `AppEvent::Docs` --- the same shape as
    /// `ProcessManager::drain`.
    pub fn drain(&mut self) -> Vec<DocsEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.receiver.try_recv() {
            events.push(event);
        }
        events
    }

    /// Applies one fetch result, returning a log line when the state
    /// change deserves one (index load/failure --- per-entry results are
    /// pane-only, far too chatty for the log).
    pub fn apply(&mut self, event: DocsEvent) -> Option<(crate::logs::Level, String)> {
        match event {
            DocsEvent::IndexLoaded { label, entries } => {
                self.state = IndexState::Loaded;
                self.index_in_flight = false;
                self.label = label;
                self.entries = entries;
                self.by_id = self
                    .entries
                    .iter()
                    .cloned()
                    .map(|e| (e.id.clone(), e))
                    .collect();
                // Whatever is selected right now has not been requested
                // against this index yet; dropping the marker lets the next
                // tick arm it.
                self.last_requested = None;
                let (boards, shields) = self.index_counts();
                Some((
                    crate::logs::Level::Info,
                    format!("board docs loaded ({boards} boards, {shields} shields)"),
                ))
            }
            DocsEvent::IndexFailed { label, error } => {
                if label != LATEST && !self.latest_fallback_tried {
                    // A workspace release (or a dev checkout) with no
                    // published docs: try `latest` once before giving up.
                    self.latest_fallback_tried = true;
                    self.index_started = false;
                    self.index_in_flight = false;
                    self.state = IndexState::Idle;
                    self.ensure_index(LATEST);
                    return Some((
                        crate::logs::Level::Warn,
                        format!("no docs for Zephyr {label}: {error}; trying {LATEST}"),
                    ));
                }
                self.state = IndexState::Failed(error.clone());
                self.index_in_flight = false;
                Some((
                    crate::logs::Level::Warn,
                    format!("could not load the Zephyr board docs: {error}"),
                ))
            }
            DocsEvent::Entry { id, detail, image } => {
                self.entry_in_flight.remove(&id);
                self.entry_done.insert(id.clone());
                if let Some(detail) = detail {
                    self.details.insert(id.clone(), detail);
                }
                if let Some(image) = image {
                    self.images.insert(id.clone(), image);
                    self.protocols.remove(&id);
                }
                None
            }
        }
    }
}

/// Fetches `url` on a thread and sends the parsed result; failures send
/// `DocsEvent::IndexFailed`. The raw body is cached at `cache_path` when
/// given, so a later session parses the disk copy instead of the network.
/// One thread per fetch, detached: the receiver outlives the picker (it
/// lives in `App`), and a dropped `App` closes the channel, which ends the
/// thread at its send.
fn spawn_fetch(
    label: String,
    url: String,
    cache_path: Option<PathBuf>,
    fetch: Option<Fetch>,
    sender: Sender<DocsEvent>,
    make: impl FnOnce(String, String) -> DocsEvent + Send + 'static,
) {
    let Some(fetch) = fetch else {
        return;
    };
    std::thread::spawn(move || {
        let event = match fetch(&url) {
            Ok(body) => {
                if let Some(path) = &cache_path
                    && std::fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).is_ok()
                {
                    let _ = std::fs::write(path, &body);
                }
                let html = String::from_utf8_lossy(&body).into_owned();
                make(label, html)
            }
            Err(err) => DocsEvent::IndexFailed {
                label,
                error: err.to_string(),
            },
        };
        let _ = sender.send(event);
    });
}

/// One entry's picture + page text, honoring the disk cache for each half
/// independently (a board whose picture was fetched last session still
/// needs only its text fetched now).
fn spawn_entry_fetch(
    entry: DocEntry,
    label: String,
    fetch: Option<Fetch>,
    cache_dir: Option<PathBuf>,
    sender: Sender<DocsEvent>,
) {
    let Some(fetch) = fetch else {
        return;
    };
    std::thread::spawn(move || {
        let cache = cache_dir.map(|dir| dir.join(&label));
        let detail = cached_text(cache.as_deref(), &entry.id).or_else(|| {
            let text = fetch(&entry.href)
                .ok()
                .map(|body| parse_detail(&String::from_utf8_lossy(&body)))
                .filter(|text| !text.is_empty())?;
            write_cache(cache.as_deref(), &entry.id, "txt", text.as_bytes());
            Some(text)
        });
        let image = cached_image(cache.as_deref(), &entry.id).or_else(|| {
            let url = entry.img_url.as_deref()?;
            let bytes = fetch(url).ok()?;
            let image = image::load_from_memory(&bytes).ok()?;
            write_cache(cache.as_deref(), &entry.id, "img", &bytes);
            Some(image)
        });
        let _ = sender.send(DocsEvent::Entry {
            id: entry.id,
            detail,
            image,
        });
    });
}

fn cached_text(cache: Option<&Path>, id: &str) -> Option<String> {
    let text = std::fs::read_to_string(cache?.join(format!("{id}.txt"))).ok()?;
    (!text.is_empty()).then_some(text)
}

fn cached_image(cache: Option<&Path>, id: &str) -> Option<DynamicImage> {
    let bytes = std::fs::read(cache?.join(format!("{id}.img"))).ok()?;
    image::load_from_memory(&bytes).ok()
}

fn write_cache(cache: Option<&Path>, id: &str, kind: &str, bytes: &[u8]) {
    let Some(cache) = cache else { return };
    if std::fs::create_dir_all(cache).is_ok() {
        let _ = std::fs::write(cache.join(format!("{id}.{kind}")), bytes);
    }
}

/// The index's disk cache path (public so `App` can keep the cache with
/// the app's own directory conventions).
pub fn index_cache_path(cache_dir: &Path, label: &str) -> PathBuf {
    cache_dir.join(label).join("index.html")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The card markup of the real index (docs.zephyrproject.org/latest/
    /// boards/index.html), abridged to the shapes that matter: a board
    /// with a picture, one without, a shield, and a card whose href is not
    /// a board page (skipped, not fatal).
    const INDEX_HTML: &str = r#"
<html><body><div id="catalog">
<a class="board-card" href="../boards/nordic/nrf52840dk/doc/index.html"
   data-vendor="nordic">
  <div class="vendor">Nordic Semiconductor</div>
  <img alt="A picture of the nRF52840 DK board" src="../_images/nrf52840dk_nrf52840.jpg" class="picture" />
  <div class="board-name">nRF52840 DK</div>
  <div class="arch">arm</div>
</a>
<a class="board-card" href="../boards/aconno/acn52832/doc/index.html">
  <div class="vendor">aconno GmbH</div>
  <div class="board-name">acn52832</div>
  <div class="arch">arm</div>
</a>
<a class="board-card shield" href="../boards/shields/mikroe_accel13_click/doc/index.html">
  <div class="vendor">MikroElektronika d.o.o.</div>
  <img src="../_images/mikroe_accel13_click.webp" class="picture" />
  <div class="board-name">ACCEL 13 Click</div>
</a>
<a class="board-card" href="https://example.org/not-a-board-page">
  <div class="vendor">Nobody</div>
  <div class="board-name">strange</div>
</a>
</div></body></html>
"#;

    #[test]
    fn the_index_parses_into_joinable_entries() {
        let entries = parse_index(INDEX_HTML, "4.1");
        assert_eq!(entries.len(), 3, "the non-board href must be skipped");

        let nrf = entries.iter().find(|e| e.id == "nrf52840dk").unwrap();
        assert_eq!(nrf.name, "nRF52840 DK");
        assert_eq!(nrf.vendor, "Nordic Semiconductor");
        assert_eq!(nrf.arch, "arm");
        assert!(!nrf.shield);
        assert_eq!(
            nrf.href,
            "https://docs.zephyrproject.org/4.1/boards/nordic/nrf52840dk/doc/index.html"
        );
        assert_eq!(
            nrf.img_url.as_deref(),
            Some("https://docs.zephyrproject.org/4.1/_images/nrf52840dk_nrf52840.jpg")
        );

        let aconno = entries.iter().find(|e| e.id == "acn52832").unwrap();
        assert_eq!(aconno.img_url, None, "a card without a picture");

        let shield = entries
            .iter()
            .find(|e| e.id == "mikroe_accel13_click")
            .unwrap();
        assert!(shield.shield);
        assert_eq!(shield.arch, "", "shields carry no arch tag");
        assert!(
            shield
                .href
                .contains("/boards/shields/mikroe_accel13_click/")
        );
    }

    #[test]
    fn the_west_name_joins_on_its_qualified_prefix() {
        assert_eq!(board_doc_id("nrf52840dk/nrf52840"), "nrf52840dk");
        assert_eq!(
            board_doc_id("esp32_devkitc_wroom/esp32/procpu"),
            "esp32_devkitc_wroom"
        );
        assert_eq!(board_doc_id("acn52832"), "acn52832");
    }

    #[test]
    fn urls_are_built_per_release_label() {
        assert_eq!(
            index_url("4.1"),
            "https://docs.zephyrproject.org/4.1/boards/index.html"
        );
        assert_eq!(
            index_url(LATEST),
            "https://docs.zephyrproject.org/latest/boards/index.html"
        );
    }

    #[test]
    fn detail_pages_flatten_to_trimmed_text() {
        let html = r#"
<html><body><div itemprop="articleBody">
<h1>nRF52840 DK</h1>
<section id="description">
<p>The nRF52840 DK is a single-board development kit for Bluetooth 5, NFC, and 802.15.4.</p>
<figure><img src="x.jpg" /><figcaption>nRF52840 DK</figcaption></figure>
<dl><dt>Vendor</dt><dd>Nordic Semiconductor</dd></dl>
</section></div></body></html>
"#;
        let text = parse_detail(html);
        assert!(text.contains("nRF52840 DK"), "heading text: {text}");
        // html2text wraps at 72 columns, so the sentence survives wrapped,
        // not on one line.
        assert!(text.contains("Bluetooth 5, NFC,"), "paragraph text: {text}");
        assert!(text.contains("802.15.4"), "paragraph tail: {text}");
        assert!(!text.contains("<p>"), "tags must not survive: {text}");
        assert!(
            !text.contains("x.jpg"),
            "image targets must not survive: {text}"
        );
    }

    #[test]
    fn detail_pages_without_a_body_yield_nothing() {
        assert_eq!(parse_detail("<html><body>nothing here</body></html>"), "");
    }

    fn fixture_fetch() -> (Fetch, Arc<std::sync::Mutex<Vec<String>>>) {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        (
            Arc::new(move |url: &str| {
                calls.lock().unwrap().push(url.to_string());
                if url.ends_with("/boards/index.html") {
                    Ok(INDEX_HTML.as_bytes().to_vec())
                } else {
                    Err(io::Error::other("offline"))
                }
            }),
            recorded,
        )
    }

    /// The fetches run on real threads, so applying is a poll until the
    /// state machine settles --- the main loop's own drain/tick cadence,
    /// compressed.
    fn pump_until(docs: &mut BoardDocs, done: impl Fn(&BoardDocs) -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            for event in docs.drain() {
                docs.apply(event);
            }
            if done(docs) {
                return true;
            }
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn the_index_loads_through_the_fetch_seam() {
        let (fetch, calls) = fixture_fetch();
        let mut docs = BoardDocs::new();
        docs.set_fetch(fetch);
        docs.ensure_index("4.1");
        assert_eq!(*docs.state(), IndexState::Loading);
        assert!(pump_until(&mut docs, |docs| *docs.state() == IndexState::Loaded));
        assert_eq!(docs.label(), "4.1");
        assert!(docs.entry("nrf52840dk").is_some());
        let (boards, shields) = docs.index_counts();
        assert_eq!((boards, shields), (2, 1));
        assert_eq!(calls.lock().unwrap().len(), 1, "one index fetch, ever");

        // Re-ensuring is a no-op.
        docs.ensure_index("latest");
        assert!(pump_until(&mut docs, |docs| *docs.state() == IndexState::Loaded));
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_unreleased_version_falls_back_to_latest() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let fetch: Fetch = Arc::new(move |url: &str| {
            recorded.lock().unwrap().push(url.to_string());
            if url.contains("/latest/") {
                Ok(INDEX_HTML.as_bytes().to_vec())
            } else {
                Err(io::Error::other("404"))
            }
        });
        let mut docs = BoardDocs::new();
        docs.set_fetch(fetch);
        docs.ensure_index("4.99");
        assert!(pump_until(&mut docs, |docs| *docs.state() == IndexState::Loaded));
        // The failed 4.99 fetch and the latest retry both happened.
        assert_eq!(calls.lock().unwrap().len(), 2);
        assert_eq!(docs.label(), "latest");
    }

    #[test]
    fn a_hard_index_failure_lands_in_the_state() {
        let fetch: Fetch = Arc::new(|_: &str| Err(io::Error::other("offline")));
        let mut docs = BoardDocs::new();
        docs.set_fetch(fetch);
        docs.ensure_index(LATEST);
        assert!(pump_until(&mut docs, |docs| matches!(
            *docs.state(),
            IndexState::Failed(_)
        )));
        assert!(matches!(*docs.state(), IndexState::Failed(ref e) if e.contains("offline")));
    }

    #[test]
    fn the_selection_debounce_fires_once_and_not_while_scrolling() {
        let (fetch, _) = fixture_fetch();
        let mut docs = BoardDocs::new();
        docs.set_fetch(fetch);
        docs.ensure_index(LATEST);
        assert!(pump_until(&mut docs, |docs| *docs.state() == IndexState::Loaded));

        // Scrolling past entries keeps re-arming the same debounce; only
        // the row the cursor rests on is fetched.
        docs.note_selection(Some("nrf52840dk"), 10);
        docs.note_selection(Some("acn52832"), 10);
        docs.note_selection(Some("mikroe_accel13_click"), 10);
        docs.drive(10);
        assert!(
            docs.entry_in_flight.is_empty(),
            "one tick has not passed yet"
        );
        docs.drive(11);
        assert!(
            docs.entry_in_flight.contains("mikroe_accel13_click"),
            "the resting selection is the one requested"
        );
        assert!(!docs.entry_in_flight.contains("nrf52840dk"));

        let image = DynamicImage::new_rgb8(4, 4);
        docs.apply(DocsEvent::Entry {
            id: "mikroe_accel13_click".to_string(),
            detail: Some("a shield".to_string()),
            image: Some(image),
        });
        // A reselect of the done entry never fetches again.
        docs.note_selection(Some("mikroe_accel13_click"), 20);
        docs.drive(21);
        assert!(!docs.entry_in_flight.contains("mikroe_accel13_click"));
        assert!(docs.protocol_for("mikroe_accel13_click").is_some());
        assert!(docs.protocol_for("nrf52840dk").is_none());
    }

    #[test]
    fn an_entry_without_a_picture_does_not_loop() {
        let fetch: Fetch = Arc::new(move |url: &str| {
            if url.ends_with("/boards/index.html") {
                Ok(INDEX_HTML.as_bytes().to_vec())
            } else {
                // The docs page itself is unreachable too: both halves
                // come back empty.
                Err(io::Error::other("offline"))
            }
        });
        let mut docs = BoardDocs::new();
        docs.set_fetch(fetch);
        docs.ensure_index(LATEST);
        assert!(pump_until(&mut docs, |docs| *docs.state() == IndexState::Loaded));

        docs.note_selection(Some("acn52832"), 0);
        docs.drive(1);
        assert!(pump_until(&mut docs, |docs| !docs.entry_loading("acn52832")));

        // Selecting it again does not re-request a known-empty entry.
        docs.note_selection(Some("acn52832"), 5);
        docs.drive(6);
        assert!(!docs.entry_in_flight.contains("acn52832"));
    }

    #[test]
    fn the_disk_cache_answers_before_the_network() {
        let dir = std::env::temp_dir().join(format!("chiptui-docs-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("latest")).unwrap();

        let (fetch, calls) = fixture_fetch();
        let mut docs = BoardDocs::new();
        docs.set_fetch(fetch);
        docs.set_cache_dir(&dir);
        docs.ensure_index(LATEST);
        assert!(pump_until(&mut docs, |docs| *docs.state() == IndexState::Loaded));
        assert_eq!(calls.lock().unwrap().len(), 1, "first run hits the network");

        // A later session with the same cache: no network at all. The
        // cache hit sends the loaded event without a fetch thread, so the
        // state stays idle until the event drains.
        let (fetch, calls) = fixture_fetch();
        let mut cached = BoardDocs::new();
        cached.set_fetch(fetch);
        cached.set_cache_dir(&dir);
        cached.ensure_index(LATEST);
        assert!(calls.lock().unwrap().is_empty(), "no fetch even started");
        assert!(pump_until(&mut cached, |docs| *docs.state() == IndexState::Loaded));
        assert!(cached.entry("nrf52840dk").is_some());
        assert!(calls.lock().unwrap().is_empty(), "the cache answered");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_a_fetch_seam_everything_stays_idle() {
        let mut docs = BoardDocs::new();
        docs.ensure_index(LATEST);
        docs.note_selection(Some("nrf52840dk"), 0);
        docs.drive(1);
        assert_eq!(*docs.state(), IndexState::Idle);
        assert_eq!(docs.drain().len(), 0);
    }
}
