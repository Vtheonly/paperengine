//! Command-line interface: command dispatch and shared argument handling.
//!
//! The CLI is an outer adapter: it normalizes arguments, loads validated configuration,
//! and drives the library's bounded runners. Every network-touching command re-checks
//! policy immediately before requests through the library's acquisition gate.

use std::collections::HashMap;
use std::error::Error;

pub mod acquire;
pub mod discover;
pub mod extract;
pub mod inspect;
pub mod lookup;
pub mod rank;
pub mod selftest;

/// Parsed command-line flags: `--key value` pairs plus boolean switches.
pub struct Flags {
    values: HashMap<String, String>,
}

#[derive(Debug)]
pub struct FlagError(pub String);

impl std::fmt::Display for FlagError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, formatter)
    }
}

impl Error for FlagError {}

impl Flags {
    /// Parse `--key value` and boolean `--switch` arguments. Positional words are rejected
    /// with an actionable error: every command uses named flags only.
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Self, FlagError> {
        let mut values = HashMap::new();
        let mut remaining = args.peekable();

        while let Some(arg) = remaining.next() {
            if let Some(key) = arg.strip_prefix("--") {
                if key.is_empty() {
                    return Err(FlagError("empty flag name".into()));
                }
                if let Some(next) = remaining.peek() {
                    if !next.starts_with("--") {
                        let value = remaining.next().expect("peeked value exists");
                        values.insert(key.to_owned(), value);
                    } else {
                        values.insert(key.to_owned(), "true".to_owned());
                    }
                } else {
                    values.insert(key.to_owned(), "true".to_owned());
                }
            } else {
                return Err(FlagError(format!(
                    "unexpected positional argument `{arg}`; use --flags only"
                )));
            }
        }
        Ok(Self { values })
    }

    pub fn require(&self, key: &str) -> Result<String, FlagError> {
        self.values
            .get(key)
            .cloned()
            .ok_or_else(|| FlagError(format!("missing required flag --{key}")))
    }

    pub fn optional(&self, key: &str) -> Option<String> {
        self.values.get(key).cloned()
    }

    pub fn parse_optional<T: std::str::FromStr>(&self, key: &str) -> Result<Option<T>, FlagError> {
        match self.values.get(key) {
            Some(raw) => raw
                .parse::<T>()
                .map(Some)
                .map_err(|_| FlagError(format!("--{key} expects a valid value, got `{raw}`"))),
            None => Ok(None),
        }
    }

    pub fn has(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }
}

pub fn dispatch(command: &str, args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    match command {
        "rank" => rank::run(args),
        "discover" => discover::run(args),
        "lookup" => lookup::run(args),
        "acquire" => acquire::run(args),
        "extract" => extract::run(args),
        "inspect" => inspect::run(args),
        "self-test" => selftest::run(args),
        other => Err(format!(
            "unknown command `{other}`; expected one of: rank, discover, lookup, acquire, extract, inspect, self-test"
        )
        .into()),
    }
}

pub fn usage() -> &'static str {
    "usage: paperengine <command> [flags]

commands:
  rank       rank normalized candidates from stdin (offline, no network)
  discover   search a scholarly source and write metadata JSONL (no downloads)
  lookup     resolve a single DOI through a source adapter (no downloads)
  acquire    download permitted OA PDFs from a discovery file into a local library
  extract    extract text and metadata from a library's objects (offline)
  inspect    list the contents of a local library
  self-test  print each adapter's offline capability report

common flags:
  --config FILE    configuration file (default: ./paperengine.toml when present)
  --email ADDRESS  contact e-mail for polite API pools (or PAPERENGINE_CONTACT_EMAIL)"
}
