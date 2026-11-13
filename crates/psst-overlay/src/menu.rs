//! The quick menu: the pages it shows and how keys and the pointer move through them. The Wayland
//! thread owns a [`Menu`] and reports its [`MenuOutcome`]s; [`crate::Renderer::render_menu`] draws it.

/// Logical pixels, as are all of the layout's sizes.
pub const MENU_WIDTH: u32 = 280;
/// Wider, for more of each dictation's words.
pub const HISTORY_WIDTH: u32 = 400;
pub(crate) const PAD: f32 = 6.0;
pub(crate) const ROW: f32 = 36.0;
/// Long enough for any term; the server checks the rest.
const MAX_WORD: usize = 100;
/// The history page doesn't scroll.
const HISTORY_ROWS: usize = 10;

#[derive(Debug, Clone, PartialEq)]
pub struct MenuSpec {
    /// The profile dictations use now.
    pub profile: String,
    pub profiles: Listing,
    /// `None` until the server says whether its model is loaded.
    pub model: Option<ModelChoice>,
    /// Dictations that never went in, newest first: transcribed since, or still waiting for the
    /// server.
    pub saved: Vec<HistoryEntry>,
    /// This machine's earlier dictations as the server has them, newest first, saved ones aside.
    pub history: Listing<HistoryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing<T = String> {
    Loading,
    Ready(Vec<T>),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// How the daemon knows it again.
    pub key: String,
    /// When it was said, as the row shows it.
    pub when: String,
    /// `None` while the server hasn't transcribed it.
    pub text: Option<String>,
}

/// What the model item offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelChoice {
    Unload,
    Load,
}

/// The history's actions name an entry by its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    SwitchProfile(String),
    AddWord(String),
    UnloadModel,
    LoadModel,
    OpenWebUi,
    /// Into the window under the menu.
    Paste(String),
    Copy(String),
    Discard(String),
}

/// `Chose` and `Closed` are done with the menu, which should be hidden; `Kept` leaves it open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuOutcome {
    Chose(MenuAction),
    Kept(MenuAction),
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Tab,
    BackTab,
    Enter,
    Escape,
    Backspace,
    Delete,
    DeleteWord,
    /// Everything before the cursor, as Ctrl+U does in a shell.
    ClearToStart,
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Main,
    History,
    Profiles,
    AddWord,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Act {
    History,
    Profiles,
    AddWord,
    Model(ModelChoice),
    WebUi,
    Profile(String),
    Saved(String),
    Past(String),
    Nothing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark {
    None,
    Chevron,
    Check,
}

/// What comes before a history row's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lead {
    None,
    /// Lines the row up with those that have a mark.
    Space,
    /// Transcribed, but never went in.
    Dot,
    /// Still waiting for the server.
    Ring,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ink {
    Normal,
    Quiet,
    Alert,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    pub(crate) label: String,
    pub(crate) detail: Option<String>,
    pub(crate) mark: Mark,
    pub(crate) lead: Lead,
    pub(crate) ink: Ink,
    act: Act,
}

impl Item {
    fn new(label: &str, act: Act) -> Item {
        Item { label: label.into(), detail: None, mark: Mark::None, lead: Lead::None, ink: Ink::Normal, act }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.act != Act::Nothing
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Line {
    /// A subpage's title; clicking it goes back.
    Header(&'static str),
    Item(Item),
    Note(String),
    Field,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    spec: MenuSpec,
    page: Page,
    /// An index into `lines()`, always an enabled item.
    selected: Option<usize>,
    word: String,
    /// A byte offset into `word`, on a char boundary.
    cursor: usize,
    /// The saved dictation whose audio a second Delete discards.
    confirm: Option<String>,
}

impl Menu {
    pub fn new(spec: MenuSpec) -> Menu {
        let mut menu = Menu { spec, page: Page::Main, selected: None, word: String::new(), cursor: 0, confirm: None };
        menu.selected = menu.selectable().next();
        menu
    }

    pub fn spec(&self) -> &MenuSpec {
        &self.spec
    }

    /// New facts, such as the profiles arriving; the page and the selected item stay. When the
    /// selected dictation is gone, the one that took its row is selected.
    pub fn update(&mut self, spec: MenuSpec) {
        let before = self.selected;
        let selected = before.and_then(|i| self.act(i));
        self.spec = spec;
        if self.confirm.as_ref().is_some_and(|key| self.saved(key).is_none()) {
            self.confirm = None;
        }
        let lines = self.lines();
        self.selected = selected
            .and_then(|act| lines.iter().position(|line| matches!(line, Line::Item(item) if item.act == act)))
            .filter(|&i| self.selectable().any(|s| s == i));
        if self.selected.is_some() {
            return;
        }
        match before.filter(|_| self.page == Page::History) {
            Some(row) => self.selected = self.selectable().find(|&i| i >= row).or_else(|| self.selectable().last()),
            None => self.select_default(),
        }
    }

    /// Logical pixels.
    pub fn size(&self) -> (u32, u32) {
        let width = if self.page == Page::History { HISTORY_WIDTH } else { MENU_WIDTH };
        (width, (2.0 * PAD + self.lines().len() as f32 * ROW) as u32)
    }

    pub fn key(&mut self, key: Key) -> Option<MenuOutcome> {
        if self.page == Page::AddWord {
            return self.edit(key);
        }
        // Any other key calls off a discard waiting for a second Delete.
        let confirming = self.confirm.take();
        let history = self.page == Page::History;
        match key {
            Key::Up | Key::BackTab => self.step(-1),
            Key::Down | Key::Tab => self.step(1),
            Key::Home => self.selected = self.selectable().next(),
            Key::End => self.selected = self.selectable().last(),
            Key::Enter | Key::Right => return self.selected.and_then(|i| self.activate(i)),
            Key::Escape if self.page == Page::Main => return Some(MenuOutcome::Closed),
            Key::Escape | Key::Left | Key::Backspace => self.back(),
            Key::Delete if history => return self.discard(confirming),
            Key::Text(text) if history && text.eq_ignore_ascii_case("c") => return self.copy(),
            Key::Text(text) => {
                let digit = text.chars().next().and_then(|c| c.to_digit(10)).filter(|&d| d > 0 && text.len() == 1);
                let line = digit.and_then(|d| {
                    let items = self.lines().into_iter().enumerate().filter(|(_, line)| matches!(line, Line::Item(_)));
                    items.map(|(i, _)| i).nth(d as usize - 1)
                });
                if let Some(i) = line {
                    return self.activate(i);
                }
            }
            _ => {}
        }
        None
    }

    /// Logical coordinates. Whether the menu looks different now.
    pub fn hover(&mut self, x: f64, y: f64) -> bool {
        let Some(i) = self.line_at(x, y).filter(|&i| self.selectable().any(|s| s == i)) else { return false };
        let changed = self.selected != Some(i);
        self.selected = Some(i);
        changed
    }

    pub fn click(&mut self, x: f64, y: f64) -> Option<MenuOutcome> {
        let i = self.line_at(x, y)?;
        self.confirm = None;
        match self.lines().get(i)? {
            Line::Header(_) => {
                self.back();
                None
            }
            Line::Item(item) if item.enabled() => self.activate(i),
            _ => None,
        }
    }

    pub(crate) fn lines(&self) -> Vec<Line> {
        match self.page {
            Page::Main => {
                let ready = self.spec.saved.iter().filter(|entry| entry.text.is_some()).count();
                let waiting = self.spec.saved.len() - ready;
                let history = Item {
                    detail: match (ready, waiting) {
                        (0, 0) => None,
                        (0, waiting) => Some(format!("{waiting} waiting")),
                        (ready, _) => Some(format!("{ready} new")),
                    },
                    mark: Mark::Chevron,
                    ..Item::new("History", Act::History)
                };
                let profile = Item {
                    detail: Some(self.spec.profile.clone()),
                    mark: Mark::Chevron,
                    ..Item::new("Profile", Act::Profiles)
                };
                let model = match self.spec.model {
                    Some(choice @ ModelChoice::Unload) => Item::new("Unload model", Act::Model(choice)),
                    Some(choice @ ModelChoice::Load) => Item::new("Load model", Act::Model(choice)),
                    None => Item { detail: Some("\u{2026}".into()), ..Item::new("Model", Act::Nothing) },
                };
                let add_word = Item { mark: Mark::Chevron, ..Item::new("Add to dictionary", Act::AddWord) };
                [history, profile, add_word, model, Item::new("Open web UI", Act::WebUi)]
                    .into_iter()
                    .map(Line::Item)
                    .collect()
            }
            Page::History => self.history_lines(),
            Page::Profiles => {
                let mut lines = vec![Line::Header("Profile")];
                match &self.spec.profiles {
                    Listing::Loading => lines.push(Line::Note("Loading\u{2026}".into())),
                    Listing::Failed(reason) => lines.push(Line::Note(reason.clone())),
                    Listing::Ready(names) if names.is_empty() => lines.push(Line::Note("No profiles".into())),
                    Listing::Ready(names) => lines.extend(names.iter().map(|name| {
                        let mark = if *name == self.spec.profile { Mark::Check } else { Mark::None };
                        Line::Item(Item { mark, ..Item::new(name, Act::Profile(name.clone())) })
                    })),
                }
                lines
            }
            Page::AddWord => vec![Line::Header("Add to dictionary"), Line::Field],
        }
    }

    /// The saved dictations first: they're the ones that still need doing.
    fn history_lines(&self) -> Vec<Line> {
        let mut lines = vec![Line::Header("History")];
        let saved = &self.spec.saved;
        for entry in saved.iter().take(HISTORY_ROWS) {
            let (label, lead, ink) = match &entry.text {
                _ if self.confirm.as_ref() == Some(&entry.key) => {
                    ("Delete again to discard the recording".to_string(), Lead::Ring, Ink::Alert)
                }
                Some(text) => (text.clone(), Lead::Dot, Ink::Normal),
                None => ("Waiting for the server".into(), Lead::Ring, Ink::Quiet),
            };
            let act = Act::Saved(entry.key.clone());
            lines.push(Line::Item(Item { detail: Some(entry.when.clone()), lead, ink, ..Item::new(&label, act) }));
        }
        if saved.len() > HISTORY_ROWS {
            lines.push(Line::Note(format!("{} more saved: see psst status", saved.len() - HISTORY_ROWS)));
        }
        let room = HISTORY_ROWS.saturating_sub(saved.len());
        match &self.spec.history {
            Listing::Loading => lines.push(Line::Note("Loading\u{2026}".into())),
            Listing::Failed(reason) => lines.push(Line::Note(reason.clone())),
            Listing::Ready(past) if past.is_empty() && saved.is_empty() => {
                lines.push(Line::Note("Nothing dictated yet".into()));
            }
            Listing::Ready(past) => lines.extend(past.iter().take(room).filter_map(|entry| {
                let text = entry.text.as_deref()?;
                let item = Item {
                    detail: Some(entry.when.clone()),
                    lead: Lead::Space,
                    ..Item::new(text, Act::Past(entry.key.clone()))
                };
                Some(Line::Item(item))
            })),
        }
        if lines.iter().any(|line| matches!(line, Line::Item(_))) {
            let discards = if saved.is_empty() { "" } else { " \u{b7} Delete discards" };
            lines.push(Line::Note(format!("Enter pastes \u{b7} C copies{discards}")));
        }
        lines
    }

    pub(crate) fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub(crate) fn word(&self) -> (&str, usize) {
        (&self.word, self.cursor)
    }

    fn act(&self, i: usize) -> Option<Act> {
        match self.lines().into_iter().nth(i)? {
            Line::Item(item) => Some(item.act),
            _ => None,
        }
    }

    fn saved(&self, key: &str) -> Option<&HistoryEntry> {
        self.spec.saved.iter().find(|entry| entry.key == key)
    }

    fn selectable(&self) -> impl Iterator<Item = usize> + use<> {
        let lines = self.lines();
        (0..lines.len()).filter(move |&i| matches!(&lines[i], Line::Item(item) if item.enabled()))
    }

    fn select_default(&mut self) {
        let current = self.lines().iter().position(|line| matches!(line, Line::Item(item) if item.mark == Mark::Check));
        self.selected = current.or_else(|| self.selectable().next());
    }

    fn step(&mut self, by: isize) {
        let selectable: Vec<usize> = self.selectable().collect();
        if selectable.is_empty() {
            return;
        }
        let at = self.selected.and_then(|s| selectable.iter().position(|&i| i == s));
        let next = match at {
            Some(at) => (at as isize + by).rem_euclid(selectable.len() as isize) as usize,
            None if by < 0 => selectable.len() - 1,
            None => 0,
        };
        self.selected = Some(selectable[next]);
    }

    fn activate(&mut self, i: usize) -> Option<MenuOutcome> {
        let chose = |action| Some(MenuOutcome::Chose(action));
        match self.act(i)? {
            Act::History => {
                self.page = Page::History;
                self.selected = self.selectable().next();
                None
            }
            Act::Profiles => {
                self.page = Page::Profiles;
                self.select_default();
                None
            }
            Act::AddWord => {
                self.page = Page::AddWord;
                self.selected = None;
                self.word.clear();
                self.cursor = 0;
                None
            }
            Act::Model(ModelChoice::Unload) => chose(MenuAction::UnloadModel),
            Act::Model(ModelChoice::Load) => chose(MenuAction::LoadModel),
            Act::WebUi => chose(MenuAction::OpenWebUi),
            Act::Profile(name) => chose(MenuAction::SwitchProfile(name)),
            // One still waiting for the server has nothing to paste yet.
            Act::Saved(key) if self.saved(&key)?.text.is_none() => None,
            Act::Saved(key) | Act::Past(key) => chose(MenuAction::Paste(key)),
            Act::Nothing => None,
        }
    }

    fn copy(&self) -> Option<MenuOutcome> {
        match self.act(self.selected?)? {
            Act::Saved(key) if self.saved(&key)?.text.is_none() => None,
            Act::Saved(key) | Act::Past(key) => Some(MenuOutcome::Chose(MenuAction::Copy(key))),
            _ => None,
        }
    }

    /// Only saved dictations are this machine's to discard. One still waiting for the server is
    /// nothing but audio, gone for good, so it takes a second Delete.
    fn discard(&mut self, confirming: Option<String>) -> Option<MenuOutcome> {
        let Act::Saved(key) = self.act(self.selected?)? else { return None };
        if self.saved(&key)?.text.is_none() && confirming.as_ref() != Some(&key) {
            self.confirm = Some(key);
            return None;
        }
        Some(MenuOutcome::Kept(MenuAction::Discard(key)))
    }

    /// To the main page, with the item that opened this one selected.
    fn back(&mut self) {
        let from = match self.page {
            Page::Main => return,
            Page::History => Act::History,
            Page::Profiles => Act::Profiles,
            Page::AddWord => Act::AddWord,
        };
        self.page = Page::Main;
        self.selected = self.lines().iter().position(|line| matches!(line, Line::Item(item) if item.act == from));
    }

    fn edit(&mut self, key: Key) -> Option<MenuOutcome> {
        let before = |word: &str, at: usize| word[..at].chars().next_back().map_or(at, |c| at - c.len_utf8());
        let after = |word: &str, at: usize| word[at..].chars().next().map_or(at, |c| at + c.len_utf8());
        match key {
            Key::Text(text) => {
                let room = MAX_WORD.saturating_sub(self.word.chars().count());
                let text: String = text.chars().filter(|c| !c.is_control()).take(room).collect();
                self.word.insert_str(self.cursor, &text);
                self.cursor += text.len();
            }
            Key::Backspace => {
                let from = before(&self.word, self.cursor);
                self.word.replace_range(from..self.cursor, "");
                self.cursor = from;
            }
            Key::Delete => {
                let to = after(&self.word, self.cursor);
                self.word.replace_range(self.cursor..to, "");
            }
            Key::DeleteWord => {
                let head = self.word[..self.cursor].trim_end();
                let from = head
                    .rfind(char::is_whitespace)
                    .map_or(0, |i| i + head[i..].chars().next().map_or(1, char::len_utf8));
                self.word.replace_range(from..self.cursor, "");
                self.cursor = from;
            }
            Key::ClearToStart => {
                self.word.replace_range(..self.cursor, "");
                self.cursor = 0;
            }
            Key::Left => self.cursor = before(&self.word, self.cursor),
            Key::Right => self.cursor = after(&self.word, self.cursor),
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = self.word.len(),
            Key::Enter => {
                let word = self.word.trim();
                if !word.is_empty() {
                    return Some(MenuOutcome::Chose(MenuAction::AddWord(word.into())));
                }
            }
            Key::Escape => self.back(),
            Key::Up | Key::Down | Key::Tab | Key::BackTab => {}
        }
        None
    }

    fn line_at(&self, x: f64, y: f64) -> Option<usize> {
        let (w, h) = self.size();
        let inside =
            (PAD as f64..w as f64 - PAD as f64).contains(&x) && (PAD as f64..h as f64 - PAD as f64).contains(&y);
        inside.then(|| ((y - PAD as f64) / ROW as f64) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> MenuSpec {
        MenuSpec {
            profile: "en".into(),
            profiles: Listing::Loading,
            model: None,
            saved: vec![],
            history: Listing::Loading,
        }
    }

    fn ready(names: &[&str]) -> Listing {
        Listing::Ready(names.iter().map(|n| n.to_string()).collect())
    }

    fn entry(key: &str, text: Option<&str>) -> HistoryEntry {
        HistoryEntry { key: key.into(), when: "14:02".into(), text: text.map(String::from) }
    }

    fn text(menu: &mut Menu, text: &str) -> Option<MenuOutcome> {
        menu.key(Key::Text(text.into()))
    }

    fn chose(action: MenuAction) -> Option<MenuOutcome> {
        Some(MenuOutcome::Chose(action))
    }

    fn labels(menu: &Menu) -> Vec<String> {
        menu.lines()
            .into_iter()
            .map(|line| match line {
                Line::Header(title) => format!("< {title}"),
                Line::Item(item) => item.label,
                Line::Note(note) => format!("({note})"),
                Line::Field => "[field]".into(),
            })
            .collect()
    }

    #[test]
    fn arrows_skip_the_model_until_its_state_is_known_and_wrap() {
        let mut menu = Menu::new(spec());
        assert_eq!(menu.selected(), Some(0));
        for _ in 0..3 {
            menu.key(Key::Down);
        }
        assert_eq!(menu.selected(), Some(4), "the model item waits for the server");
        menu.key(Key::Down);
        assert_eq!(menu.selected(), Some(0));
        menu.key(Key::Up);
        assert_eq!(menu.selected(), Some(4));

        menu.update(MenuSpec { model: Some(ModelChoice::Unload), ..spec() });
        assert_eq!(menu.selected(), Some(4), "an update keeps the selected item");
        menu.key(Key::Up);
        assert_eq!(menu.key(Key::Enter), chose(MenuAction::UnloadModel));
    }

    #[test]
    fn digits_choose_items_by_position() {
        let mut menu = Menu::new(MenuSpec { model: Some(ModelChoice::Load), ..spec() });
        assert_eq!(text(&mut menu, "4"), chose(MenuAction::LoadModel));
        assert_eq!(text(&mut menu, "5"), chose(MenuAction::OpenWebUi));
        assert_eq!(text(&mut menu, "9"), None);
        assert_eq!(text(&mut menu, "0"), None);
    }

    #[test]
    fn the_profile_page_starts_on_the_current_profile_even_when_the_list_arrives_late() {
        let mut menu = Menu::new(spec());
        assert_eq!(text(&mut menu, "2"), None);
        assert_eq!(menu.lines()[1], Line::Note("Loading\u{2026}".into()));
        assert_eq!(menu.selected(), None);
        assert_eq!(menu.key(Key::Enter), None);

        menu.update(MenuSpec { profiles: ready(&["cs", "en", "work"]), ..spec() });
        assert_eq!(menu.selected(), Some(2));
        menu.key(Key::Down);
        assert_eq!(menu.key(Key::Enter), chose(MenuAction::SwitchProfile("work".into())));
    }

    #[test]
    fn escape_goes_back_a_page_then_closes() {
        let mut menu = Menu::new(MenuSpec { profiles: ready(&["en"]), ..spec() });
        text(&mut menu, "3");
        assert_eq!(menu.lines(), vec![Line::Header("Add to dictionary"), Line::Field]);
        assert_eq!(menu.key(Key::Escape), None);
        assert_eq!(menu.selected(), Some(2), "back on the item that opened the page");
        menu.key(Key::Up);
        menu.key(Key::Right);
        assert_eq!(menu.key(Key::Left), None);
        assert_eq!(menu.selected(), Some(1));
        assert_eq!(menu.key(Key::Escape), Some(MenuOutcome::Closed));
    }

    #[test]
    fn the_word_field_edits_like_a_text_field() {
        let mut menu = Menu::new(spec());
        text(&mut menu, "3");
        assert_eq!(menu.key(Key::Enter), None, "nothing to add yet");
        for key in ["K", "u", "b", "e", "r", "n", "ě", "t"] {
            text(&mut menu, key);
        }
        assert_eq!(menu.word(), ("Kubernět", "Kubernět".len()));
        menu.key(Key::Backspace);
        menu.key(Key::Backspace);
        text(&mut menu, "etes");
        menu.key(Key::Home);
        menu.key(Key::Delete);
        text(&mut menu, "\u{7f}k\n");
        menu.key(Key::End);
        text(&mut menu, " hub ");
        menu.key(Key::DeleteWord);
        assert_eq!(menu.word().0, "kubernetes ");
        text(&mut menu, " ");
        assert_eq!(menu.key(Key::Enter), chose(MenuAction::AddWord("kubernetes".into())));
        menu.key(Key::Left);
        menu.key(Key::ClearToStart);
        assert_eq!(menu.word(), (" ", 0));
    }

    #[test]
    fn the_pointer_selects_and_chooses_and_its_header_goes_back() {
        let mut menu = Menu::new(MenuSpec { profiles: ready(&["cs", "en"]), ..spec() });
        let row = |i: usize| (PAD + ROW * (i as f32 + 0.5)) as f64;
        assert!(!menu.hover(100.0, row(0)), "already selected");
        assert!(!menu.hover(100.0, row(3)), "the model item can't be selected yet");
        assert!(menu.hover(100.0, row(4)));
        assert!(!menu.hover(2.0, row(1)), "the margin isn't a row");
        assert_eq!(menu.click(100.0, row(1)), None);
        assert_eq!(menu.size(), (MENU_WIDTH, (2.0 * PAD + 3.0 * ROW) as u32));
        assert_eq!(menu.click(100.0, row(0)), None, "the header");
        assert_eq!(menu.click(100.0, row(4)), chose(MenuAction::OpenWebUi));
    }

    #[test]
    fn the_history_lists_saved_dictations_first_then_the_servers() {
        let saved = vec![entry("a", Some("Hi Jan, I looked at it.")), entry("b", None)];
        let past = vec![entry("c", Some("Sounds good.")), entry("d", None)];
        let mut menu = Menu::new(MenuSpec { saved, ..spec() });
        assert_eq!(menu.lines()[0], {
            let item = Item { detail: Some("1 new".into()), mark: Mark::Chevron, ..Item::new("History", Act::History) };
            Line::Item(item)
        });
        text(&mut menu, "1");
        assert_eq!(
            labels(&menu),
            [
                "< History",
                "Hi Jan, I looked at it.",
                "Waiting for the server",
                "(Loading\u{2026})",
                "(Enter pastes \u{b7} C copies \u{b7} Delete discards)",
            ]
        );
        assert_eq!(menu.size().0, HISTORY_WIDTH);
        assert_eq!(menu.selected(), Some(1));

        menu.update(MenuSpec { history: Listing::Ready(past), ..menu.spec().clone() });
        assert_eq!(labels(&menu)[3..], ["Sounds good.", "(Enter pastes \u{b7} C copies \u{b7} Delete discards)"]);
        assert_eq!(menu.key(Key::Enter), chose(MenuAction::Paste("a".into())));
        menu.key(Key::Down);
        assert_eq!(menu.key(Key::Enter), None, "nothing to paste while it waits");
        assert_eq!(text(&mut menu, "c"), None);
        menu.key(Key::Down);
        assert_eq!(text(&mut menu, "C"), chose(MenuAction::Copy("c".into())));
        assert_eq!(menu.key(Key::Delete), None, "the server's history isn't this machine's to discard");
        assert_eq!(text(&mut menu, "3"), chose(MenuAction::Paste("c".into())));
        menu.key(Key::Escape);
        assert_eq!(menu.selected(), Some(0));
        assert_eq!(menu.size().0, MENU_WIDTH);
    }

    #[test]
    fn a_transcribed_dictation_is_discarded_at_once_and_a_waiting_one_on_a_second_delete() {
        let saved = vec![entry("a", Some("Kept.")), entry("b", None), entry("c", None)];
        let mut menu = Menu::new(MenuSpec { saved, history: Listing::Ready(vec![]), ..spec() });
        text(&mut menu, "1");
        assert_eq!(menu.key(Key::Delete), Some(MenuOutcome::Kept(MenuAction::Discard("a".into()))));

        menu.key(Key::Down);
        assert_eq!(menu.key(Key::Delete), None);
        assert_eq!(labels(&menu)[2], "Delete again to discard the recording");
        menu.key(Key::Down);
        assert_eq!(menu.key(Key::Delete), None, "another row's Delete asks for that one");
        assert_eq!(labels(&menu)[2], "Waiting for the server");
        menu.key(Key::Up);
        menu.key(Key::Delete);
        menu.key(Key::Down);
        menu.key(Key::Up);
        assert_eq!(menu.key(Key::Delete), None, "any other key calls it off");
        assert_eq!(menu.key(Key::Delete), Some(MenuOutcome::Kept(MenuAction::Discard("b".into()))));

        let after = vec![entry("a", Some("Kept.")), entry("c", None)];
        menu.update(MenuSpec { saved: after, ..menu.spec().clone() });
        assert_eq!(menu.selected(), Some(2), "the next one takes the discarded one's place");
        menu.update(MenuSpec { saved: vec![], ..menu.spec().clone() });
        assert_eq!(labels(&menu), ["< History", "(Nothing dictated yet)"]);
        assert_eq!(menu.selected(), None);
    }

    #[test]
    fn the_history_shows_ten_dictations_at_most() {
        let saved: Vec<_> = (0..12).map(|i| entry(&format!("s{i}"), None)).collect();
        let mut menu = Menu::new(MenuSpec { saved, history: Listing::Ready(vec![entry("p", Some("Old."))]), ..spec() });
        text(&mut menu, "1");
        let labels = labels(&menu);
        assert_eq!(labels.len(), 1 + HISTORY_ROWS + 2);
        assert_eq!(labels[HISTORY_ROWS + 1], "(2 more saved: see psst status)");
    }
}
