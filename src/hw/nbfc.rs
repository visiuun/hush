//! Finds the nbfc-linux model config for this laptop.
//!
//! nbfc-linux names every config after the SMBIOS product name of the laptop it was written
//! for, plus a small alias table (`model_support.json`) for models that share one. Matching is
//! exact (case and spacing aside): a config for a merely similar model writes the wrong EC
//! registers. The whole index is downloaded and matched locally, so the model name never leaves
//! the machine. A match is only offered, never switched on by itself: `install` (Settings →
//! Hardware, `hush laptop use`) copies it to the path the EC backend reads.

use super::ec;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const TREE: &str = "https://api.github.com/repos/nbfc-linux/nbfc-linux/git/trees/main?recursive=1";
const RAW: &str = "https://raw.githubusercontent.com/nbfc-linux/nbfc-linux/main/";
const CONFIGS: &str = "share/nbfc/configs/";
const ALIASES: &str = "share/nbfc/model_support.json";

/// The last search, cached in `%ProgramData%\Hush\nbfc-lookup.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Lookup {
    pub product: String,
    /// Unix seconds.
    pub checked: i64,
    /// Name of the matching config (its file name without `.json`).
    pub found: Option<String>,
    /// Why the match can't be used, or why the search failed.
    pub error: Option<String>,
}

impl Lookup {
    /// A config that is downloaded, valid and ready to install.
    pub fn usable(&self) -> Option<&str> {
        self.found.as_deref().filter(|_| self.error.is_none() && candidate_path().is_file())
    }

    /// Whether a new search is due: another machine, a failed search after a day, no match after a week.
    fn stale(&self, product: &str) -> bool {
        let age = chrono::Utc::now().timestamp() - self.checked;
        self.product != product || (self.found.is_none() && age > if self.error.is_some() { 86_400 } else { 7 * 86_400 })
    }
}

fn dir() -> PathBuf {
    std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| ".".into()).join("Hush")
}

fn lookup_path() -> PathBuf {
    dir().join("nbfc-lookup.json")
}

/// The downloaded match, waiting to be installed.
pub fn candidate_path() -> PathBuf {
    dir().join("nbfc-candidate.json")
}

pub fn cached() -> Option<Lookup> {
    std::fs::read_to_string(lookup_path()).ok().and_then(|s| serde_json::from_str(&s).ok())
}

/// Whether a search is due for this machine.
pub fn due() -> bool {
    cached().is_none_or(|l| l.stale(&ec::system_product()))
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("curl.exe")
        .args(["-fsSL", "--max-time", "30", "-H", "User-Agent: hush", url])
        .creation_flags(crate::winutil::CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(format!("could not reach GitHub ({})", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out.stdout)
}

/// Percent-encodes a path segment (config names have spaces, parentheses, `+`, …).
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Exact match of the product name (or its alias) against the config names.
fn pick(product: &str, names: &[String], aliases: &BTreeMap<String, String>) -> Option<String> {
    let p = norm(product);
    let want = aliases.iter().find(|(k, _)| norm(k) == p).map(|(_, v)| norm(v)).unwrap_or(p);
    names.iter().find(|n| norm(n) == want).cloned()
}

/// The matching config's name and text, `None` when nbfc-linux has none (network, blocking).
fn download(product: &str) -> Result<Option<(String, String)>, String> {
    if product.is_empty() {
        return Err("the firmware reports no product name".into());
    }
    let tree: serde_json::Value = serde_json::from_slice(&fetch(TREE)?).map_err(|e| format!("config index: {e}"))?;
    let names: Vec<String> = tree["tree"]
        .as_array()
        .ok_or("config index: unexpected format")?
        .iter()
        .filter_map(|e| e["path"].as_str()?.strip_prefix(CONFIGS)?.strip_suffix(".json").map(String::from))
        .collect();
    let aliases: BTreeMap<String, String> = fetch(&format!("{RAW}{ALIASES}")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let Some(name) = pick(product, &names, &aliases) else { return Ok(None) };
    let url = format!("{RAW}{CONFIGS}{}.json", encode(&name));
    let text = String::from_utf8(fetch(&url)?).map_err(|e| e.to_string())?;
    Ok(Some((name, text)))
}

/// Searches nbfc-linux for this machine's config and caches the result (network, blocking).
pub fn search() -> Lookup {
    let product = ec::system_product();
    let mut l = Lookup { product: product.clone(), checked: chrono::Utc::now().timestamp(), ..Default::default() };
    let _ = std::fs::remove_file(candidate_path());
    match download(&product) {
        Ok(Some((name, text))) => {
            match ec::validate(&text) {
                Ok(_) => {
                    let _ = std::fs::create_dir_all(dir());
                    if let Err(e) = std::fs::write(candidate_path(), &text) {
                        l.error = Some(format!("could not save it: {e}"));
                    }
                }
                Err(e) => l.error = Some(e),
            }
            l.found = Some(name);
        }
        Ok(None) => {}
        Err(e) => l.error = Some(e),
    }
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(lookup_path(), serde_json::to_vec_pretty(&l).unwrap_or_default());
    l
}

/// Puts the found config in place for the EC backend.
pub fn install() -> Result<String, String> {
    let l = cached().ok_or("no search has run yet")?;
    let name = l.usable().ok_or_else(|| match (&l.found, &l.error) {
        (Some(n), Some(e)) => format!("the config for {n} can't be used: {e}"),
        (None, Some(e)) => format!("search failed: {e}"),
        _ => format!("nbfc-linux has no config for {}", l.product),
    })?;
    let text = std::fs::read_to_string(candidate_path()).map_err(|e| e.to_string())?;
    ec::validate(&text)?;
    std::fs::write(ec::config_path(), text).map_err(|e| e.to_string())?;
    Ok(name.to_string())
}

/// Removes the laptop config (the EC backend stops at the next hardware detection).
pub fn uninstall() -> Result<(), String> {
    match std::fs::remove_file(ec::config_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("no laptop config is in use".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// One status line for Settings → Hardware and `hush status`.
pub fn describe(l: &Lookup) -> Result<String, String> {
    if ec::config_path().is_file() {
        return Ok("laptop config in use (hush laptop off to remove it)".into());
    }
    match (&l.found, &l.error) {
        (Some(n), None) => Ok(format!("found the config for {n}: Settings → Hardware → Use it, or hush laptop use")),
        (Some(n), Some(e)) => Err(format!("found the config for {n}, but {e}")),
        (None, Some(e)) => Err(format!("search failed: {e}")),
        (None, None) => Err(format!("nbfc-linux has no config for {}", l.product)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_alias_matches_only() {
        let names: Vec<String> = ["HP 250 G8 Notebook PC", "ASUS VivoBook X505ZA_X505ZA", "Acer Aspire 5"].map(String::from).to_vec();
        let aliases = BTreeMap::from([("HP 250 15.6 inch G9 Notebook PC".to_string(), "HP 250 G8 Notebook PC".to_string())]);
        assert_eq!(pick("asus vivobook  X505ZA_X505ZA", &names, &aliases).as_deref(), Some("ASUS VivoBook X505ZA_X505ZA"));
        assert_eq!(pick("HP 250 15.6 inch G9 Notebook PC", &names, &aliases).as_deref(), Some("HP 250 G8 Notebook PC"));
        assert_eq!(pick("Acer Aspire 5 A515-54", &names, &aliases), None);
        assert_eq!(pick("ASUS Zenbook 14 UM3406KA_UM3406KA", &names, &aliases), None);
    }

    /// Hits GitHub: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn live_index() {
        let (name, text) = download("HP 250 15.6 inch G9 Notebook PC").unwrap().expect("alias match");
        assert_eq!(name, "HP 250 G8 Notebook PC");
        assert!(ec::validate(&text).is_err(), "this one uses ACPI methods");
        let (name, text) = download("asus vivobook X505ZA_X505ZA").unwrap().expect("exact match");
        assert_eq!(name, "ASUS VivoBook X505ZA_X505ZA");
        ec::validate(&text).unwrap();
        assert!(download("ASUS Zenbook 14 UM3406KA_UM3406KA").unwrap().is_none());
    }
}
