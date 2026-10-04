//! cf-ts – interactively switch Cloud Foundry org and space without logging in again.
//!
//! Fetches orgs and spaces via `cf curl` (v3 API JSON), shows a fuzzy
//! picker and then runs `cf target -o ORG -s SPACE`.

use anyhow::{bail, Context, Result};
use console::{Key, Style, Term};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use dialoguer::theme::{ColorfulTheme, Theme};
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{exit, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

const USAGE: &str = "\
Interactively switch Cloud Foundry org and space.

Usage: cf-ts [OPTIONS]

Lists your orgs and spaces via `cf curl`, lets you fuzzy-pick one and
runs `cf target -o ORG -s SPACE`. Requires the cf CLI and an active login.

Press Tab on an org to make it a favorite and give it a name of your own.
Favorites are listed first, as \"★ NAME (ORG)\".

Press Tab on a space to save the org and space together as a target with
a name of your own. Saved targets are listed above the orgs, as
\"◆ NAME (ORG / SPACE)\", and switch to both at once.

Press Esc in the space list to go back to the orgs, and Esc there to quit.

Options:
  -h, --help     Print help
  -V, --version  Print version";

#[derive(Deserialize)]
struct Page {
    resources: Vec<Resource>,
    pagination: Pagination,
}

#[derive(Deserialize)]
struct Pagination {
    next: Option<Link>,
}

#[derive(Deserialize)]
struct Link {
    href: String,
}

#[derive(Deserialize, Clone)]
struct Resource {
    guid: String,
    name: String,
}

/// Current target from the cf config, used to preselect the picker.
#[derive(Deserialize, Default)]
struct CfConfig {
    /// API endpoint, e.g. `https://api.cf.example.com`.
    #[serde(rename = "Target", default)]
    api: String,
    #[serde(rename = "OrganizationFields", default)]
    org: Target,
    #[serde(rename = "SpaceFields", default)]
    space: Target,
}

#[derive(Deserialize, Default)]
struct Target {
    #[serde(rename = "GUID", default)]
    guid: String,
    #[serde(rename = "Name", default)]
    name: String,
}

fn cf_home() -> Option<PathBuf> {
    // cf uses $CF_HOME, otherwise the home directory (Unix: HOME, Windows: USERPROFILE).
    ["CF_HOME", "HOME", "USERPROFILE"]
        .iter()
        .find_map(|v| std::env::var_os(v).filter(|s| !s.is_empty()))
        .map(PathBuf::from)
}

fn read_config() -> CfConfig {
    cf_home()
        .map(|h| h.join(".cf").join("config.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Names the user gave to their favorite orgs and saved targets.
#[derive(Serialize, Deserialize, Default)]
struct Favorites {
    /// Favorite orgs, keyed by org name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    orgs: BTreeMap<String, String>,
    /// Saved org and space combinations, keyed by org name, then space name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    targets: BTreeMap<String, BTreeMap<String, String>>,
}

/// Favorites per API endpoint: the same org name can exist on several.
type AllFavorites = BTreeMap<String, Favorites>;

fn favorites_path() -> Option<PathBuf> {
    cf_home().map(|h| h.join(".cf").join("cf-ts.json"))
}

fn load_favorites() -> Result<AllFavorites> {
    let Some(path) = favorites_path() else {
        return Ok(AllFavorites::default());
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s)
            .with_context(|| format!("invalid favorites file {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AllFavorites::default()),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn save_favorites(favorites: &mut AllFavorites) -> Result<()> {
    // Drop whatever became empty by removing the last name in it.
    for f in favorites.values_mut() {
        f.targets.retain(|_, spaces| !spaces.is_empty());
    }
    favorites.retain(|_, f| !f.orgs.is_empty() || !f.targets.is_empty());

    let path = favorites_path().context("cannot find your home directory")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(favorites)? + "\n")
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Turns cf's auth-related failures into a short, actionable message.
fn login_hint(msg: &str) -> Option<String> {
    let m = msg.to_lowercase();
    let reason = if m.contains("no api endpoint set") {
        "no Cloud Foundry API endpoint set"
    } else if m.contains("not logged in") {
        "not logged in to Cloud Foundry"
    } else if m.contains("token expired")
        || m.contains("was revoked")
        || m.contains("log back in")
        || m.contains("invalid_token")
        || m.contains("invalid auth token")
    {
        "your Cloud Foundry session has expired"
    } else {
        return None;
    };
    Some(format!(
        "{reason}.\n\nLog in first, then run cf-ts again:\n  cf login          (or: cf login --sso)"
    ))
}

/// stdout and stderr of a cf call, without cf's bare "FAILED" line.
fn combined_output(out: &std::process::Output) -> String {
    [
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty() && s != "FAILED")
    .collect::<Vec<_>>()
    .join("\n")
}

/// Fails with a login hint if cf has no usable token.
fn check_login() -> Result<()> {
    let out = Command::new("cf")
        .arg("oauth-token")
        .output()
        .context("failed to start cf – is the cf CLI on your PATH?")?;
    if !out.status.success() {
        let msg = combined_output(&out);
        bail!(login_hint(&msg).unwrap_or_else(|| format!(
            "cf has no valid session:\n{msg}\n\nLog in first, then run cf-ts again:\n  cf login          (or: cf login --sso)"
        )));
    }
    Ok(())
}

fn cf_curl(path: &str) -> Result<Page> {
    let out = Command::new("cf")
        .args(["curl", path])
        .output()
        .context("failed to start cf – is the cf CLI on your PATH?")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let msg = combined_output(&out);
    if !out.status.success() {
        bail!(login_hint(&msg).unwrap_or_else(|| format!("cf curl {path} failed:\n{msg}")));
    }
    let value: serde_json::Value = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(e) => {
            if let Some(hint) = login_hint(&msg) {
                bail!(hint);
            }
            // Some cf versions exit 0 with an empty body when the session is
            // gone; ask cf directly whether we still have a valid token.
            if stdout.trim().is_empty() {
                check_login()?;
            }
            bail!("unexpected response from cf curl {path}: {e}\n{msg}");
        }
    };
    if let Some(errors) = value.get("errors") {
        let errors = errors.to_string();
        bail!(login_hint(&errors).unwrap_or_else(|| format!("API error for {path}: {errors}")));
    }
    Ok(serde_json::from_value(value)?)
}

/// Fetches all pages of a v3 list endpoint.
fn list_all(first: &str) -> Result<Vec<Resource>> {
    let mut items = Vec::new();
    let mut next = Some(first.to_string());
    while let Some(path) = next {
        let page = cf_curl(&path)?;
        items.extend(page.resources);
        // `next.href` is a full URL, cf curl only needs the path.
        next = page.pagination.next.map(|l| match l.href.find("/v3/") {
            Some(i) => l.href[i..].to_string(),
            None => l.href,
        });
    }
    Ok(items)
}

const FAVORITE: char = '★';
const TARGET: char = '◆';

/// Named items first (by their own name), then the rest by their real name.
fn sort_named(items: &mut [Resource], names: &BTreeMap<String, String>) {
    items.sort_by_cached_key(|r| match names.get(&r.name) {
        Some(name) => (0, name.to_lowercase()),
        None => (1, r.name.to_lowercase()),
    });
}

/// Picker labels: `MARK NAME (ITEM)` for named items. With `indent`, the
/// other items line up below the marked ones.
fn labels(
    items: &[Resource],
    names: &BTreeMap<String, String>,
    mark: char,
    indent: bool,
) -> Vec<String> {
    items
        .iter()
        .map(|r| match names.get(&r.name) {
            Some(name) => format!("{mark} {name} ({})", r.name),
            None if indent => format!("  {}", r.name),
            None => r.name.clone(),
        })
        .collect()
}

enum Picked {
    Item(usize),
    /// Tab was pressed on this item to edit its name.
    Tab(usize),
    Cancelled,
}

/// Whether the alternate screen is open, for `close_picker`.
static PICKER_OPEN: AtomicBool = AtomicBool::new(false);

/// The "Prompt: item" lines of what was picked so far. They are shown on the
/// main screen once the alternate screen is left.
static PICKED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Switches to the alternate screen, unless it is open already. It stays
/// open from the first picker until `close_picker`: leaving it between two
/// pickers would let the main screen flash up in between.
fn open_picker(term: &Term) -> Result<()> {
    if PICKER_OPEN.load(Ordering::SeqCst) {
        return Ok(());
    }
    // Asked for while still on the main screen, and only for its side
    // effect: once the terminal was resized with a picker open, Windows
    // (ConPTY) no longer knows where the main screen's cursor is. Until it
    // was asked there, every look at the size from the alternate screen
    // takes half a second, and the picker draws and reads keys that slowly.
    let _ = term.size();
    term.write_str("\x1b[?1049h")?;
    PICKER_OPEN.store(true, Ordering::SeqCst);
    Ok(())
}

/// Puts the terminal back the way the picker found it. Does no harm
/// without a picker open, so it is also what Ctrl+C runs.
fn close_picker() {
    let term = Term::stderr();
    if PICKER_OPEN.swap(false, Ordering::SeqCst) {
        let _ = terminal::disable_raw_mode();
        let _ = term.write_str("\x1b[?1049l");
    }
    let _ = term.show_cursor();
}

/// Leaves the alternate screen for good and shows what was picked.
fn finish_picker() -> Result<()> {
    close_picker();
    let term = Term::stderr();
    for line in PICKED.lock().unwrap().drain(..) {
        term.write_line(&line)?;
    }
    Ok(())
}

/// Fuzzy picker in the style of dialoguer's `FuzzySelect`, which has no way
/// to bind an extra key. Tab returns `Picked::Tab`; `keys` says in a line
/// below the list what the extra keys do.
///
/// The picker is drawn on the terminal's alternate screen: redrawing in
/// place among the scrollback leaves copies behind once the terminal is
/// resized. The picker stays on it as it was until the next one draws over
/// it; the picked item is shown by `finish_picker` on the main screen, as a
/// line of its own.
///
/// `search` is the filter text: what it holds is there from the start, and
/// what was typed is left in it, so a picker can be shown again as it was.
fn pick(
    prompt: &str,
    names: &[String],
    default: usize,
    keys: &str,
    search: &mut String,
) -> Result<Picked> {
    let term = Term::stderr();
    open_picker(&term)?;
    let _ = term.hide_cursor();
    // Raw mode, for crossterm's events: unlike `Term::read_key` they also
    // tell when the terminal was resized.
    let picked = terminal::enable_raw_mode()
        .map_err(Into::into)
        .and_then(|_| pick_on(&term, prompt, names, default, keys, search));
    let _ = terminal::disable_raw_mode();
    if let Ok(Picked::Item(i)) = picked {
        let mut line = String::new();
        ColorfulTheme::default().format_input_prompt_selection(
            &mut line,
            prompt,
            names[i].trim_start(),
        )?;
        PICKED.lock().unwrap().push(line);
    }
    picked
}

/// Group of a picker label: saved targets, favorites, the rest.
fn group(label: &str) -> u8 {
    match label.chars().next() {
        Some(TARGET) => 0,
        Some(FAVORITE) => 1,
        _ => 2,
    }
}

/// Char index of the parenthesis that opens the `(ITEM)` at the end of a
/// label. It is the one matching the last `)`, as the item may have
/// parentheses of its own, like the user's name in front of it.
fn hint_start(text: &str) -> Option<usize> {
    let chars: Vec<char> = text.chars().collect();
    let mut depth = 0usize;
    for (idx, c) in chars.iter().enumerate().rev() {
        match c {
            ')' => depth += 1,
            '(' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

/// A picker row like `ColorfulTheme::format_fuzzy_select_prompt_item` draws
/// it, but with a leading favorite or target mark in a color of its own and
/// the `(ITEM)` behind a name of the user's own toned down.
fn format_item(
    theme: &ColorfulTheme,
    matcher: &SkimMatcherV2,
    text: &str,
    active: bool,
    search: &str,
) -> String {
    let mark = match text.chars().next() {
        Some(FAVORITE) => Some(Style::new().for_stderr().yellow()),
        Some(TARGET) => Some(Style::new().for_stderr().magenta()),
        _ => None,
    };
    let hint = mark.as_ref().and_then(|_| hint_start(text));
    // Char index of the `/` between org and space in there. It is not part
    // of either name, so it keeps the default color.
    let separator = text
        .rfind(" / ")
        .map(|at| text[..at].chars().count() + 1)
        .filter(|&at| hint.is_some_and(|hint| at > hint));
    let matched = matcher
        .fuzzy_indices(text, search)
        .map(|(_, indices)| indices)
        .unwrap_or_default();
    let prefix = match active {
        true => &theme.active_item_prefix,
        false => &theme.inactive_item_prefix,
    };

    let mut line = format!("{prefix} ");
    for (idx, c) in text.chars().enumerate() {
        let c = match matched.contains(&idx) {
            true => theme.fuzzy_match_highlight_style.apply_to(c).to_string(),
            false => c.to_string(),
        };
        match &mark {
            Some(style) if idx == 0 => line.push_str(&style.apply_to(c).to_string()),
            _ if separator == Some(idx) => line.push_str(&c),
            _ if hint.is_some_and(|at| idx >= at) => {
                line.push_str(&theme.hint_style.apply_to(c).to_string())
            }
            _ if active => line.push_str(&theme.active_item_style.apply_to(c).to_string()),
            _ => line.push_str(&c),
        }
    }
    line
}

/// The key help below a picker, toned down except for the `[Key]`s in it,
/// which become key caps: the name on a background, without the brackets.
fn format_hint(theme: &ColorfulTheme, text: &str) -> String {
    let cap = Style::new().for_stderr().reverse();
    let mut key: Option<String> = None;
    let mut hint = String::new();
    for c in text.chars() {
        match (c, &mut key) {
            ('[', None) => key = Some(String::new()),
            (']', Some(name)) => {
                hint.push_str(&cap.apply_to(format!(" {name} ")).to_string());
                key = None;
            }
            (_, Some(name)) => name.push(c),
            (_, None) => hint.push_str(&theme.hint_style.apply_to(c).to_string()),
        }
    }
    hint
}

fn pick_on(
    term: &Term,
    prompt: &str,
    names: &[String],
    default: usize,
    keys: &str,
    search: &mut String,
) -> Result<Picked> {
    let theme = ColorfulTheme::default();
    let matcher = SkimMatcherV2::default();
    // Shown behind the cursor while nothing is typed yet.
    let placeholder = theme.hint_style.apply_to("type to filter").to_string();
    // Indented like the items above it.
    let footer = format!("  {}", format_hint(&theme, keys));
    // Line between the groups, as wide as the longest label.
    let width = names
        .iter()
        .map(|n| console::measure_text_width(n))
        .max()
        .unwrap_or(0)
        .min((term.size().1 as usize).saturating_sub(3));
    let separator = Style::new()
        .for_stderr()
        .dim()
        .apply_to(format!("  {}", "─".repeat(width)))
        .to_string();

    // Row of the selected match. `default` is an item, and with a search
    // already there its row is only known once the matches are.
    let mut sel = 0;
    let mut preselect = Some(default);
    let mut top = 0;

    loop {
        // Matching items, best match first.
        let mut matches: Vec<(usize, i64)> = names
            .iter()
            .enumerate()
            .filter_map(|(i, name)| match search.is_empty() {
                true => Some((i, 0)),
                false => Some((i, matcher.fuzzy_match(name, search)?)),
            })
            .collect();
        matches.sort_by_key(|&(i, score)| (group(&names[i]), Reverse(score)));
        if let Some(item) = preselect.take() {
            sel = matches.iter().position(|&(i, _)| i == item).unwrap_or(0);
        }

        // Lines to draw: the matches, with `None` for a separator wherever
        // the group changes.
        let mut lines: Vec<Option<usize>> = Vec::new();
        for (row, &(i, _)) in matches.iter().enumerate() {
            if row > 0 && group(&names[i]) != group(&names[matches[row - 1].0]) {
                lines.push(None);
            }
            lines.push(Some(row));
        }

        // Leave room for the prompt line, the empty line and the footer.
        // Measured on every draw, as the terminal may have been resized.
        let rows = (term.size().0 as usize).max(4) - 3;
        // Scroll so the selected row stays visible.
        let sel_line = lines.iter().position(|l| *l == Some(sel)).unwrap_or(0);
        if sel_line < top {
            top = sel_line;
        } else if sel_line >= top + rows {
            top = sel_line + 1 - rows;
        }
        // No empty rows below the list while there are lines above it, as
        // after the terminal grew with the end of the list in view.
        top = top.min(lines.len().saturating_sub(rows));

        let mut prompt_line = String::new();
        theme.format_fuzzy_select_prompt(&mut prompt_line, prompt, search, search.len())?;
        if search.is_empty() {
            prompt_line.push_str(&placeholder);
        }
        let mut frame = vec![prompt_line];
        frame.extend(lines.iter().skip(top).take(rows).map(|line| match *line {
            Some(row) => {
                let name = &names[matches[row].0];
                format_item(&theme, &matcher, name, row == sel, search)
            }
            None => separator.clone(),
        }));
        frame.push(String::new());
        frame.push(footer.clone());

        // The whole frame in one write, from the top left corner. Lines are
        // cut to the terminal's width, as a wrapped one would push the frame
        // off the screen; the reset is for a style the cut left open.
        let cols = (term.size().1 as usize).max(1);
        let frame: Vec<String> = frame
            .iter()
            .map(|line| format!("{}\x1b[0m\x1b[K", console::truncate_str(line, cols, "…")))
            .collect();
        term.write_str(&format!("\x1b[H{}\x1b[J", frame.join("\r\n")))?;

        // Wait for a key, or for the terminal to change its size: then the
        // frame is drawn again right away. Not every terminal reports that
        // as an event, hence the look at the size in between.
        let size = term.size();
        let key = loop {
            if !event::poll(Duration::from_millis(100))? {
                if term.size() != size {
                    break None;
                }
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => break Some(key),
                Event::Resize(..) => break None,
                _ => {}
            }
        };
        let Some(key) = key else { continue };
        // AltGr comes as Ctrl+Alt on Windows, so only Ctrl alone is a
        // shortcut and not text.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            // In raw mode Ctrl+C is a key like any other.
            KeyCode::Char('c') if ctrl => {
                close_picker();
                exit(130);
            }
            KeyCode::Esc => return Ok(Picked::Cancelled),
            KeyCode::Enter if !matches.is_empty() => return Ok(Picked::Item(matches[sel].0)),
            KeyCode::Tab if !matches.is_empty() => return Ok(Picked::Tab(matches[sel].0)),
            KeyCode::Up | KeyCode::BackTab if !matches.is_empty() => {
                sel = sel.checked_sub(1).unwrap_or(matches.len() - 1);
            }
            KeyCode::Down if !matches.is_empty() => {
                sel = (sel + 1) % matches.len();
            }
            KeyCode::Backspace => {
                search.pop();
                sel = 0;
            }
            KeyCode::Char(c) if !ctrl && !c.is_ascii_control() => {
                search.push(c);
                sel = 0;
            }
            _ => {}
        }
    }
}

/// One-line text input that leaves nothing on screen. `None` if cancelled
/// with Esc.
fn input(prompt: &str, initial: &str) -> Result<Option<String>> {
    let term = Term::stderr();
    let mut prefix = String::new();
    ColorfulTheme::default().format_input_prompt(&mut prefix, prompt, None)?;
    let cols = (term.size().1 as usize).max(1);
    // In place of the picker it was asked from.
    term.write_str("\x1b[H\x1b[J")?;
    term.show_cursor()?;

    let mut text = initial.to_string();
    // Terminal lines the prompt wrapped onto, above the cursor's line.
    let mut wrapped = 0;
    loop {
        term.clear_line()?;
        term.clear_last_lines(wrapped)?;
        let line = format!("{prefix}{text}");
        term.write_str(&line)?;
        wrapped = console::measure_text_width(&line).saturating_sub(1) / cols;

        let done = match term.read_key()? {
            Key::Escape => Some(None),
            Key::Enter => Some(Some(text.clone())),
            Key::Backspace => {
                text.pop();
                None
            }
            Key::Char(c) if !c.is_ascii_control() => {
                text.push(c);
                None
            }
            _ => None,
        };
        if let Some(result) = done {
            term.clear_line()?;
            term.clear_last_lines(wrapped)?;
            return Ok(result);
        }
    }
}

/// Asks for the user's own name for `what`, an org or an org and a space.
/// `None` if cancelled or left unchanged; an empty name means the entry
/// should be removed.
fn edit_name(what: &[&str], current: &str) -> Result<Option<String>> {
    let theme = ColorfulTheme::default();
    let plain = |text: &str| theme.prompt_style.apply_to(text).to_string();
    let colored = theme.prompt_style.clone().cyan();
    let what: Vec<String> = what
        .iter()
        .map(|name| colored.apply_to(name).to_string())
        .collect();
    let prompt = format!(
        "{}{} {}",
        plain("Name for "),
        what.join(&plain(" / ")),
        format_hint(&theme, "(empty: remove, [Esc] cancel)")
    );
    Ok(input(&prompt, current)?
        .map(|name| name.trim().to_string())
        .filter(|name| name != current))
}

/// Stores `name` under `key`; an empty name removes the entry.
fn set_name(names: &mut BTreeMap<String, String>, key: &str, name: String) {
    if name.is_empty() {
        names.remove(key);
    } else {
        names.insert(key.to_string(), name);
    }
}

/// A row of the first picker.
#[derive(PartialEq)]
enum Row {
    /// A saved org and space combination, by their names.
    Target { org: String, space: String },
    /// An org, by its GUID.
    Org(String),
}

/// What was chosen in the first picker.
enum First {
    Target { org: String, space: String },
    Org(Resource),
}

/// Rows of the first picker with their labels: saved targets (by their own
/// name), then the orgs in the given order.
fn first_rows(orgs: &[Resource], favorites: &Favorites) -> (Vec<Row>, Vec<String>) {
    let mut targets: Vec<(&String, &String, &String)> = favorites
        .targets
        .iter()
        // An org we cannot see (any more) cannot be targeted either.
        .filter(|(org, _)| orgs.iter().any(|o| &o.name == *org))
        .flat_map(|(org, spaces)| spaces.iter().map(move |(space, name)| (name, org, space)))
        .collect();
    targets.sort_by_cached_key(|(name, ..)| name.to_lowercase());

    let indent = !targets.is_empty() || orgs.iter().any(|o| favorites.orgs.contains_key(&o.name));
    let mut names: Vec<String> = targets
        .iter()
        .map(|(name, org, space)| format!("{TARGET} {name} ({org} / {space})"))
        .collect();
    names.extend(labels(orgs, &favorites.orgs, FAVORITE, indent));

    let mut rows: Vec<Row> = targets
        .into_iter()
        .map(|(_, org, space)| Row::Target {
            org: org.clone(),
            space: space.clone(),
        })
        .collect();
    rows.extend(orgs.iter().map(|o| Row::Org(o.guid.clone())));
    (rows, names)
}

/// First picker: saved targets and orgs, with `current` preselected and
/// filtered by `search`. Tab edits the name of the highlighted row and then
/// shows the picker again.
fn pick_first(
    config: &CfConfig,
    all: &mut AllFavorites,
    orgs: &mut [Resource],
    mut current: Row,
    search: &mut String,
) -> Result<Option<First>> {
    loop {
        let favorites = all.entry(config.api.clone()).or_default();
        sort_named(orgs, &favorites.orgs);
        let (mut rows, names) = first_rows(orgs, favorites);
        let default = rows.iter().position(|r| *r == current).unwrap_or(0);
        match pick(
            "Org",
            &names,
            default,
            "[Tab] name selected entry  [Esc] quit",
            search,
        )? {
            Picked::Cancelled => return Ok(None),
            Picked::Item(i) => {
                return Ok(match rows.swap_remove(i) {
                    Row::Target { org, space } => Some(First::Target { org, space }),
                    Row::Org(guid) => orgs
                        .iter()
                        .find(|o| o.guid == guid)
                        .cloned()
                        .map(First::Org),
                })
            }
            Picked::Tab(i) => {
                // Keep the edited row selected when the picker comes back.
                current = rows.swap_remove(i);
                let changed = match &current {
                    Row::Target { org, space } => {
                        let spaces = favorites.targets.entry(org.clone()).or_default();
                        let old = spaces.get(space).cloned().unwrap_or_default();
                        edit_name(&[org, space], &old)?.map(|name| set_name(spaces, space, name))
                    }
                    Row::Org(guid) => {
                        let Some(org) = orgs.iter().find(|o| &o.guid == guid) else {
                            continue;
                        };
                        let old = favorites.orgs.get(&org.name).cloned().unwrap_or_default();
                        edit_name(&[&org.name], &old)?
                            .map(|name| set_name(&mut favorites.orgs, &org.name, name))
                    }
                };
                if changed.is_some() {
                    save_favorites(all)?;
                }
            }
        }
    }
}

/// Space picker. Tab saves the highlighted space together with `org` as a
/// target under a name of the user's own, and then shows the picker again.
/// `None` if left with Esc.
fn pick_space(
    config: &CfConfig,
    all: &mut AllFavorites,
    org: &str,
    mut spaces: Vec<Resource>,
) -> Result<Option<String>> {
    let mut current = config.space.guid.clone();
    let mut search = String::new();
    loop {
        let saved = all
            .entry(config.api.clone())
            .or_default()
            .targets
            .entry(org.to_string())
            .or_default();
        sort_named(&mut spaces, saved);
        let names = labels(&spaces, saved, TARGET, !saved.is_empty());
        let default = spaces.iter().position(|s| s.guid == current).unwrap_or(0);
        let keys = "[Tab] save selected space as target  [Esc] back";
        match pick("Space", &names, default, keys, &mut search)? {
            Picked::Cancelled => return Ok(None),
            Picked::Item(i) => return Ok(Some(spaces.swap_remove(i).name)),
            Picked::Tab(i) => {
                // Keep the edited space selected when the picker comes back.
                current = spaces[i].guid.clone();
                let space = &spaces[i].name;
                let old = saved.get(space).cloned().unwrap_or_default();
                if let Some(name) = edit_name(&[org, space], &old)? {
                    set_name(saved, space, name);
                    save_favorites(all)?;
                }
            }
        }
    }
}

fn target(org: &str, space: Option<&str>) -> Result<i32> {
    finish_picker()?;
    let mut args = vec!["target", "-o", org];
    if let Some(space) = space {
        args.extend(["-s", space]);
    }
    let status = Command::new("cf").args(&args).status()?;
    Ok(status.code().unwrap_or(1))
}

fn run() -> Result<i32> {
    let config = read_config();
    let mut all = load_favorites()?;

    let mut orgs = list_all("/v3/organizations?order_by=name&per_page=5000")?;
    if orgs.is_empty() {
        bail!("no orgs found");
    }
    // Preselect the current target if it is a saved one, otherwise its org.
    let saved = all.get(&config.api).is_some_and(|f| {
        f.targets
            .get(&config.org.name)
            .is_some_and(|spaces| spaces.contains_key(&config.space.name))
    });
    let mut current = match saved {
        true => Row::Target {
            org: config.org.name.clone(),
            space: config.space.name.clone(),
        },
        false => Row::Org(config.org.guid.clone()),
    };

    // What was typed in the org picker, so it is still there after coming
    // back from the spaces.
    let mut search = String::new();
    loop {
        let org = match pick_first(&config, &mut all, &mut orgs, current, &mut search)? {
            None => return Ok(1), // cancelled with Esc
            Some(First::Target { org, space }) => return target(&org, Some(&space)),
            Some(First::Org(org)) => org,
        };

        let spaces = list_all(&format!(
            "/v3/spaces?organization_guids={}&order_by=name&per_page=5000",
            org.guid
        ))?;
        if spaces.is_empty() {
            finish_picker()?;
            eprintln!("Org has no spaces, targeting the org only.");
            return target(&org.name, None);
        }

        // A single space gets the picker too: Tab there is the only way to
        // save it as a target.
        match pick_space(&config, &mut all, &org.name, spaces)? {
            Some(space) => return target(&org.name, Some(&space)),
            // Esc: back to the org picker, with this org selected.
            None => {
                // Forget the "Org: NAME" line the org picker left behind.
                PICKED.lock().unwrap().pop();
                current = Row::Org(org.guid);
            }
        }
    }
}

fn main() {
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-h" | "--help" => println!("{USAGE}"),
            "-V" | "--version" => println!("cf-ts {}", env!("CARGO_PKG_VERSION")),
            _ => {
                eprintln!("error: unexpected argument '{arg}'\n\n{USAGE}");
                exit(2);
            }
        }
        return;
    }

    // The picker and the name input hide the cursor while they are open;
    // Ctrl+C would exit before it gets shown again, leaving the terminal
    // without a cursor.
    let _ = ctrlc::set_handler(|| {
        close_picker();
        exit(130);
    });

    let result = run();
    // Still open after Esc or an error.
    close_picker();
    match result {
        Ok(code) => exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(name: &str) -> Resource {
        Resource {
            guid: format!("guid-{name}"),
            name: name.to_string(),
        }
    }

    fn names(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, name)| (key.to_string(), name.to_string()))
            .collect()
    }

    fn order(items: &[Resource]) -> Vec<&str> {
        items.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn sort_named_puts_named_items_first() {
        let mut items = vec![resource("b-org"), resource("a-org"), resource("c-org")];
        sort_named(&mut items, &names(&[("c-org", "Zeta"), ("b-org", "alpha")]));
        // Named ones by their own name, whatever its case, then the rest.
        assert_eq!(order(&items), ["b-org", "c-org", "a-org"]);
    }

    #[test]
    fn sort_named_without_names_sorts_by_name() {
        let mut items = vec![resource("b"), resource("C"), resource("a")];
        sort_named(&mut items, &BTreeMap::new());
        assert_eq!(order(&items), ["a", "b", "C"]);
    }

    #[test]
    fn labels_mark_named_items() {
        let items = [resource("org-1"), resource("org-2")];
        let named = names(&[("org-1", "Mine")]);
        assert_eq!(
            labels(&items, &named, FAVORITE, true),
            ["★ Mine (org-1)", "  org-2"]
        );
        assert_eq!(
            labels(&items, &named, TARGET, false),
            ["◆ Mine (org-1)", "org-2"]
        );
    }

    #[test]
    fn group_orders_targets_favorites_rest() {
        assert_eq!(group("◆ Dev (org / dev)"), 0);
        assert_eq!(group("★ Mine (org)"), 1);
        assert_eq!(group("  org"), 2);
        assert_eq!(group("org"), 2);
        assert_eq!(group(""), 2);
    }

    #[test]
    fn hint_start_finds_the_item() {
        assert_eq!(hint_start("★ Mine (org)"), Some(7));
        assert_eq!(hint_start("◆ Dev (org / dev)"), Some(6));
        // Parentheses in the user's name or in the item.
        assert_eq!(hint_start("★ Mine (old) (org)"), Some(13));
        assert_eq!(hint_start("★ Mine (org (eu))"), Some(7));
        // Counted in chars, not bytes.
        assert_eq!(hint_start("★ Büro (org)"), Some(7));
        assert_eq!(hint_start("  org"), None);
        assert_eq!(hint_start("★ Mine (org) eu)"), None);
    }

    #[test]
    fn format_item_keeps_the_text() {
        console::set_colors_enabled_stderr(false);
        let theme = ColorfulTheme::default();
        let matcher = SkimMatcherV2::default();
        for text in ["★ Mine (org (eu))", "◆ Dev (org / dev)", "  org"] {
            let line = format_item(&theme, &matcher, text, true, "org");
            assert!(console::strip_ansi_codes(&line).ends_with(text));
        }
    }

    #[test]
    fn format_hint_turns_brackets_into_key_caps() {
        console::set_colors_enabled_stderr(false);
        let hint = format_hint(&ColorfulTheme::default(), "[Tab] name  [Esc] quit");
        assert_eq!(console::strip_ansi_codes(&hint), " Tab  name   Esc  quit");
    }

    #[test]
    fn set_name_inserts_and_removes() {
        let mut map = BTreeMap::new();
        set_name(&mut map, "org", "Mine".to_string());
        assert_eq!(map, names(&[("org", "Mine")]));
        set_name(&mut map, "org", "Other".to_string());
        assert_eq!(map, names(&[("org", "Other")]));
        set_name(&mut map, "org", String::new());
        assert!(map.is_empty());
    }

    #[test]
    fn first_rows_lists_targets_above_orgs() {
        let orgs = [resource("fav-org"), resource("plain-org")];
        let favorites = Favorites {
            orgs: names(&[("fav-org", "Fav")]),
            targets: BTreeMap::from([
                (
                    "plain-org".to_string(),
                    names(&[("dev", "zulu"), ("prod", "Alpha")]),
                ),
                // Not among the orgs, so it is left out.
                ("gone-org".to_string(), names(&[("dev", "Gone")])),
            ]),
        };
        let (rows, labels) = first_rows(&orgs, &favorites);
        assert_eq!(
            labels,
            [
                "◆ Alpha (plain-org / prod)",
                "◆ zulu (plain-org / dev)",
                "★ Fav (fav-org)",
                "  plain-org",
            ]
        );
        let target = |space: &str| Row::Target {
            org: "plain-org".to_string(),
            space: space.to_string(),
        };
        assert!(rows[0] == target("prod"));
        assert!(rows[1] == target("dev"));
        assert!(rows[2] == Row::Org("guid-fav-org".to_string()));
        assert!(rows[3] == Row::Org("guid-plain-org".to_string()));
    }

    #[test]
    fn first_rows_without_favorites_has_no_indent() {
        let orgs = [resource("a"), resource("b")];
        let (rows, labels) = first_rows(&orgs, &Favorites::default());
        assert_eq!(labels, ["a", "b"]);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn favorites_file_format() {
        let json = r#"{
  "https://api.example.com": {
    "orgs": {
      "org-1": "Mine"
    }
  }
}"#;
        let all: AllFavorites = serde_json::from_str(json).unwrap();
        let favorites = &all["https://api.example.com"];
        assert_eq!(favorites.orgs, names(&[("org-1", "Mine")]));
        assert!(favorites.targets.is_empty());
        // Empty maps are left out, so the file reads back as it was written.
        assert_eq!(serde_json::to_string_pretty(&all).unwrap(), json);
    }

    #[test]
    fn login_hint_recognizes_auth_failures() {
        let reason =
            |msg: &str| login_hint(msg).map(|hint| hint.lines().next().unwrap().to_string());
        assert_eq!(
            reason("No API endpoint set. Use 'cf login'").as_deref(),
            Some("no Cloud Foundry API endpoint set.")
        );
        assert_eq!(
            reason("Not logged in. Use 'cf login'").as_deref(),
            Some("not logged in to Cloud Foundry.")
        );
        assert_eq!(
            reason(r#"{"errors":[{"title":"CF-InvalidAuthToken","detail":"Invalid Auth Token"}]}"#)
                .as_deref(),
            Some("your Cloud Foundry session has expired.")
        );
        assert_eq!(login_hint("Organization not found"), None);
    }
}
