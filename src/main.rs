//! cf-ts – interactively switch Cloud Foundry org and space without logging in again.
//!
//! Fetches orgs and spaces via `cf curl` (v3 API JSON), shows a fuzzy
//! picker and then runs `cf target -o ORG -s SPACE`.

use anyhow::{bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, FuzzySelect};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::{exit, Command};

const USAGE: &str = "\
Interactively switch Cloud Foundry org and space.

Usage: cf-ts [OPTIONS]

Lists your orgs and spaces via `cf curl`, lets you fuzzy-pick one and
runs `cf target -o ORG -s SPACE`. Requires the cf CLI and an active login.

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

#[derive(Deserialize)]
struct Resource {
    guid: String,
    name: String,
}

/// Current target from the cf config, used to preselect the picker.
#[derive(Deserialize, Default)]
struct CfConfig {
    #[serde(rename = "OrganizationFields", default)]
    org: Target,
    #[serde(rename = "SpaceFields", default)]
    space: Target,
}

#[derive(Deserialize, Default)]
struct Target {
    #[serde(rename = "GUID", default)]
    guid: String,
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

fn cf_curl(path: &str) -> Result<Page> {
    let out = Command::new("cf")
        .args(["curl", path])
        .output()
        .context("failed to start cf – is the cf CLI on your PATH?")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        bail!(
            "cf curl {path} failed (are you logged in?):\n{}{}",
            stdout.trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .with_context(|| format!("unexpected response from cf curl {path}:\n{stdout}"))?;
    if let Some(errors) = value.get("errors") {
        bail!("API error for {path}: {errors}");
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

fn pick(prompt: &str, items: &[Resource], current_guid: &str) -> Result<Option<usize>> {
    let names: Vec<&str> = items.iter().map(|r| r.name.as_str()).collect();
    let default = items
        .iter()
        .position(|r| r.guid == current_guid)
        .unwrap_or(0);
    Ok(FuzzySelect::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .items(&names)
        .default(default)
        .interact_opt()?)
}

fn run() -> Result<i32> {
    let config = read_config();

    let orgs = list_all("/v3/organizations?order_by=name&per_page=5000")?;
    if orgs.is_empty() {
        bail!("no orgs found");
    }
    let Some(o) = pick("Org", &orgs, &config.org.guid)? else {
        return Ok(1); // cancelled with Esc
    };
    let org = &orgs[o];

    let spaces = list_all(&format!(
        "/v3/spaces?organization_guids={}&order_by=name&per_page=5000",
        org.guid
    ))?;

    let mut args = vec!["target", "-o", org.name.as_str()];
    match spaces.len() {
        0 => eprintln!("Org has no spaces, targeting the org only."),
        1 => args.extend(["-s", spaces[0].name.as_str()]),
        _ => {
            let Some(s) = pick("Space", &spaces, &config.space.guid)? else {
                return Ok(1);
            };
            args.extend(["-s", spaces[s].name.as_str()]);
        }
    }

    let status = Command::new("cf").args(&args).status()?;
    Ok(status.code().unwrap_or(1))
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

    match run() {
        Ok(code) => exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            exit(1);
        }
    }
}
