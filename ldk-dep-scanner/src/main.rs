use anyhow::{Context, Result};
use base64::Engine;
use clap::Parser;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "ldk-dep-scanner")]
#[command(about = "Scan rust-lightning dependents for std vs no-std usage")]
struct Args {
    /// Path to dependents.json file
    #[arg(short, long, default_value = "dependents.json")]
    input: PathBuf,

    /// Output markdown file path
    #[arg(short, long, default_value = "dependents_report.md")]
    output: PathBuf,

    /// Minimum stars filter
    #[arg(short, long, default_value = "0")]
    min_stars: u32,

    /// GitHub token (or set GITHUB_TOKEN env var)
    #[arg(short, long, env = "GITHUB_TOKEN")]
    token: Option<String>,

    /// Number of concurrent requests
    #[arg(short, long, default_value = "5")]
    concurrency: usize,

    /// Verbose output - show each Cargo.toml being inspected
    #[arg(short, long, default_value = "false")]
    verbose: bool,
}

#[derive(Debug, Deserialize)]
struct DependentsFile {
    dependents: Vec<Dependent>,
}

#[derive(Debug, Deserialize, Clone)]
struct Dependent {
    user: String,
    repo: String,
    stars: u32,
    forks: u32,
}

#[derive(Debug, Serialize)]
struct ScanResult {
    repo: String,
    stars: u32,
    forks: u32,
    mode: String,
    details: String,
    file: String,
    updated_at: String,
}

#[derive(Debug, Deserialize)]
struct GitTreeResponse {
    tree: Vec<TreeEntry>,
}

#[derive(Debug, Deserialize)]
struct TreeEntry {
    path: String,
    #[serde(rename = "type")]
    entry_type: String,
}

#[derive(Debug, Deserialize)]
struct FileContentResponse {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RepoMetadata {
    updated_at: String,
    fork: bool,
    #[serde(default)]
    parent: Option<ParentRepo>,
}

#[derive(Debug, Deserialize)]
struct ParentRepo {
    full_name: String,
}

#[derive(Debug, Clone, PartialEq)]
enum StdMode {
    Std,
    NoStd,
    None,
    Unknown,
    Indirect,
}

impl std::fmt::Display for StdMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StdMode::Std => write!(f, "STD"),
            StdMode::NoStd => write!(f, "NO_STD"),
            StdMode::None => write!(f, "NONE"),
            StdMode::Unknown => write!(f, "UNKNOWN"),
            StdMode::Indirect => write!(f, "INDIRECT"),
        }
    }
}

struct Scanner {
    client: reqwest::Client,
    token: Option<String>,
    verbose: bool,
}

impl Scanner {
    fn new(token: Option<String>, verbose: bool) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("ldk-dep-scanner")
            .build()?;
        Ok(Self { client, token, verbose })
    }

    /// Returns (updated_at, is_rust_lightning_fork)
    async fn get_repo_metadata(&self, user: &str, repo: &str) -> Result<Option<(String, bool)>> {
        let url = format!("https://api.github.com/repos/{}/{}", user, repo);

        let mut request = self.client.get(&url);
        if let Some(ref token) = self.token {
            request = request.header("Authorization", format!("Bearer {}", token));
        }

        let response = request.send().await?;

        if !response.status().is_success() {
            return Ok(None);
        }

        let metadata: RepoMetadata = response.json().await?;
        // Parse ISO 8601 date and return just the date portion
        let updated_at = metadata.updated_at.split('T').next().unwrap_or(&metadata.updated_at).to_string();

        // Check if this is a fork of rust-lightning
        let is_rust_lightning_fork = metadata.fork
            && metadata.parent.as_ref().map_or(false, |p| {
                p.full_name == "lightningdevkit/rust-lightning"
            });

        Ok(Some((updated_at, is_rust_lightning_fork)))
    }

    async fn get_repo_tree(&self, user: &str, repo: &str) -> Result<Vec<String>> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/git/trees/HEAD?recursive=1",
            user, repo
        );

        let mut request = self.client.get(&url);
        if let Some(ref token) = self.token {
            request = request.header("Authorization", format!("Bearer {}", token));
        }

        let response = request.send().await?;
        let status = response.status();

        if !status.is_success() {
            if self.verbose {
                let body = response.text().await.unwrap_or_default();
                eprintln!("    [API] {}/{} tree: {} - {}", user, repo, status, body);
            }
            return Ok(vec![]);
        }

        let tree_response: GitTreeResponse = response.json().await?;

        let cargo_tomls: Vec<String> = tree_response
            .tree
            .into_iter()
            .filter(|e| e.entry_type == "blob" && e.path.ends_with("Cargo.toml"))
            .map(|e| e.path)
            .collect();

        Ok(cargo_tomls)
    }

    async fn get_file_content(&self, user: &str, repo: &str, path: &str) -> Result<Option<String>> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/contents/{}",
            user, repo, path
        );

        let mut request = self.client.get(&url);
        if let Some(ref token) = self.token {
            request = request.header("Authorization", format!("Bearer {}", token));
        }

        let response = request.send().await?;

        if !response.status().is_success() {
            return Ok(None);
        }

        let file_response: FileContentResponse = response.json().await?;

        if let Some(content) = file_response.content {
            let cleaned = content.replace('\n', "");
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&cleaned)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok());
            return Ok(decoded);
        }

        Ok(None)
    }

    fn check_cargo_toml(&self, content: &str) -> (StdMode, String) {
        // Parse the TOML to check for lightning dependencies
        let parsed: Result<toml::Value, _> = content.parse();

        let Ok(toml_value) = parsed else {
            // Fall back to regex-based checking if TOML parsing fails
            return self.check_cargo_toml_regex(content);
        };

        let mut found_lightning = false;
        let mut has_no_std = false;
        let mut has_std_reenabled = false;
        let mut details = String::new();

        // Check [dependencies], [dev-dependencies], [build-dependencies]
        for dep_section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(deps) = toml_value.get(dep_section).and_then(|v| v.as_table()) {
                for (name, value) in deps {
                    if is_lightning_crate(name) {
                        found_lightning = true;
                        let (no_std, std_enabled) = check_dependency_value(value);
                        if no_std {
                            has_no_std = true;
                        }
                        if std_enabled {
                            has_std_reenabled = true;
                        }
                    }
                }
            }
        }

        // Check [workspace.dependencies]
        if let Some(workspace) = toml_value.get("workspace").and_then(|v| v.as_table()) {
            if let Some(deps) = workspace.get("dependencies").and_then(|v| v.as_table()) {
                for (name, value) in deps {
                    if is_lightning_crate(name) {
                        found_lightning = true;
                        let (no_std, std_enabled) = check_dependency_value(value);
                        if no_std {
                            has_no_std = true;
                        }
                        if std_enabled {
                            has_std_reenabled = true;
                        }
                    }
                }
            }
        }

        // Check [target.'cfg(...)'.dependencies] sections
        if let Some(target) = toml_value.get("target").and_then(|v| v.as_table()) {
            for (_cfg, cfg_value) in target {
                if let Some(deps) = cfg_value.get("dependencies").and_then(|v| v.as_table()) {
                    for (name, value) in deps {
                        if is_lightning_crate(name) {
                            found_lightning = true;
                            let (no_std, std_enabled) = check_dependency_value(value);
                            if no_std {
                                has_no_std = true;
                            }
                            if std_enabled {
                                has_std_reenabled = true;
                            }
                        }
                    }
                }
            }
        }

        if !found_lightning {
            return (StdMode::None, "no lightning dependency".to_string());
        }

        if has_no_std && !has_std_reenabled {
            details = "default-features=false".to_string();
            (StdMode::NoStd, details)
        } else if has_no_std && has_std_reenabled {
            details = "default-features=false but std re-enabled".to_string();
            (StdMode::Std, details)
        } else {
            details = "using default features".to_string();
            (StdMode::Std, details)
        }
    }

    fn check_cargo_toml_regex(&self, content: &str) -> (StdMode, String) {
        // Check if it references any lightning crate
        let lightning_pattern = regex_lite::Regex::new(r#"(lightning|lightning-[a-z-]+)\s*="#).unwrap();
        if !lightning_pattern.is_match(content) {
            return (StdMode::None, "no lightning dependency".to_string());
        }

        // Check for default-features = false
        let no_default_pattern = regex_lite::Regex::new(
            r#"lightning.*default-features\s*=\s*false|default-features\s*=\s*false.*lightning"#
        ).unwrap();

        if no_default_pattern.is_match(content) {
            // Check if std is re-enabled
            let std_enabled_pattern = regex_lite::Regex::new(
                r#"lightning.*features\s*=\s*\[.*"std".*\]|features\s*=\s*\[.*"std".*\].*lightning"#
            ).unwrap();

            if std_enabled_pattern.is_match(content) {
                return (StdMode::Std, "default-features=false but std re-enabled".to_string());
            } else {
                return (StdMode::NoStd, "default-features=false".to_string());
            }
        }

        (StdMode::Std, "using default features".to_string())
    }

    async fn scan_repo(&self, dependent: &Dependent) -> Option<ScanResult> {
        let repo_name = format!("{}/{}", dependent.user, dependent.repo);

        // Get repo metadata for updated_at and fork check
        let (updated_at, is_fork) = self
            .get_repo_metadata(&dependent.user, &dependent.repo)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();

        // Skip rust-lightning forks early to avoid unnecessary API calls
        if is_fork {
            if self.verbose {
                eprintln!("\n  [{}] Skipping: fork of rust-lightning", repo_name);
            }
            return None;
        }

        // Get all Cargo.toml files in the repo
        let cargo_files = match self.get_repo_tree(&dependent.user, &dependent.repo).await {
            Ok(files) => files,
            Err(e) => {
                if self.verbose {
                    eprintln!("\n  [{}] ERROR fetching tree: {}", repo_name, e);
                }
                return Some(ScanResult {
                    repo: repo_name,
                    stars: dependent.stars,
                    forks: dependent.forks,
                    mode: StdMode::Unknown.to_string(),
                    details: format!("failed to fetch repo tree: {}", e),
                    file: String::new(),
                    updated_at,
                });
            }
        };

        if cargo_files.is_empty() {
            if self.verbose {
                eprintln!("\n  [{}] No Cargo.toml files found in tree", repo_name);
            }
            return Some(ScanResult {
                repo: repo_name,
                stars: dependent.stars,
                forks: dependent.forks,
                mode: StdMode::Unknown.to_string(),
                details: "no Cargo.toml found".to_string(),
                file: String::new(),
                updated_at,
            });
        }

        if self.verbose {
            eprintln!("\n  [{}] Found {} Cargo.toml files:", repo_name, cargo_files.len());
            for f in &cargo_files {
                eprintln!("    - {}", f);
            }
        }

        let mut final_mode = StdMode::None;
        let mut final_details = String::new();
        let mut final_file = String::new();
        let mut found_lightning = false;

        for cargo_path in cargo_files {
            let content = match self
                .get_file_content(&dependent.user, &dependent.repo, &cargo_path)
                .await
            {
                Ok(Some(c)) => c,
                _ => {
                    if self.verbose {
                        eprintln!("    [SKIP] {}: failed to fetch content", cargo_path);
                    }
                    continue;
                }
            };

            let (mode, details) = self.check_cargo_toml(&content);

            if self.verbose {
                eprintln!("    [CHECK] {}: {} ({})", cargo_path, mode, details);
            }

            if mode != StdMode::None {
                found_lightning = true;
                // Prioritize NO_STD findings
                if mode == StdMode::NoStd || final_mode == StdMode::None {
                    final_mode = mode.clone();
                    final_details = details;
                    final_file = cargo_path;
                }
            }
        }

        if found_lightning {
            Some(ScanResult {
                repo: repo_name,
                stars: dependent.stars,
                forks: dependent.forks,
                mode: final_mode.to_string(),
                details: final_details,
                file: final_file,
                updated_at,
            })
        } else {
            Some(ScanResult {
                repo: repo_name,
                stars: dependent.stars,
                forks: dependent.forks,
                mode: StdMode::Indirect.to_string(),
                details: "no direct lightning dependency".to_string(),
                file: String::new(),
                updated_at,
            })
        }
    }
}

fn is_lightning_crate(name: &str) -> bool {
    name == "lightning"
        || name.starts_with("lightning-")
        || name == "ldk"
        || name.starts_with("ldk-")
}

fn check_dependency_value(value: &toml::Value) -> (bool, bool) {
    let mut has_default_features_false = false;
    let mut has_std_enabled = false;

    if let Some(table) = value.as_table() {
        // Check default-features = false
        if let Some(df) = table.get("default-features") {
            if df.as_bool() == Some(false) {
                has_default_features_false = true;
            }
        }

        // Check if std is in features
        if let Some(features) = table.get("features").and_then(|v| v.as_array()) {
            for f in features {
                if f.as_str() == Some("std") {
                    has_std_enabled = true;
                }
            }
        }
    }

    (has_default_features_false, has_std_enabled)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Load dependents.json
    let file = File::open(&args.input)
        .with_context(|| format!("Failed to open {}", args.input.display()))?;
    let reader = BufReader::new(file);
    let dependents_file: DependentsFile = serde_json::from_reader(reader)
        .with_context(|| "Failed to parse dependents.json")?;

    // Filter by minimum stars and exclude rust-lightning itself
    let dependents: Vec<_> = dependents_file
        .dependents
        .into_iter()
        .filter(|d| d.stars >= args.min_stars)
        .filter(|d| {
            // Exclude rust-lightning and obvious forks by name
            let repo_lower = d.repo.to_lowercase();
            !(d.user == "lightningdevkit" && d.repo == "rust-lightning")
                && !repo_lower.contains("rust-lightning")
        })
        .collect();

    println!(
        "Scanning {} repositories (min stars: {})...\n",
        dependents.len(),
        args.min_stars
    );

    let scanner = Scanner::new(args.token, args.verbose)?;

    // Create progress bar
    let pb = ProgressBar::new(dependents.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
            .unwrap()
            .progress_chars("#>-"),
    );

    // Scan repos concurrently (forks are filtered out during scan)
    let results: Vec<Option<ScanResult>> = stream::iter(dependents)
        .map(|dep| {
            let scanner = &scanner;
            let pb = &pb;
            async move {
                let repo_name = format!("{}/{}", dep.user, dep.repo);
                pb.set_message(repo_name.clone());
                let result = scanner.scan_repo(&dep).await;
                pb.inc(1);
                result
            }
        })
        .buffer_unordered(args.concurrency)
        .collect()
        .await;

    pb.finish_with_message("Done!");

    // Collect results, keeping only NO_STD repos
    let mut results: Vec<_> = results
        .into_iter()
        .flatten() // Remove None (skipped forks)
        .filter(|r| r.mode == "NO_STD")
        .collect();

    // Sort by stars descending
    results.sort_by(|a, b| b.stars.cmp(&a.stars));

    // Write markdown output
    let mut md_content = String::new();
    md_content.push_str("# rust-lightning NO_STD Dependents\n\n");
    md_content.push_str("| Repository | Stars | Last Updated | Cargo.toml |\n");
    md_content.push_str("|------------|-------|--------------|------------|\n");

    for result in &results {
        let cargo_link = format!(
            "[Cargo.toml](https://github.com/{}/blob/HEAD/{})",
            result.repo, result.file
        );
        md_content.push_str(&format!(
            "| [{}](https://github.com/{}) | {} | {} | {} |\n",
            result.repo, result.repo, result.stars, result.updated_at, cargo_link
        ));
    }

    std::fs::write(&args.output, &md_content)?;

    // Print summary
    println!("\n=========================================");
    println!("SUMMARY");
    println!("=========================================\n");
    println!("Results written to: {}\n", args.output.display());

    let mut counts: HashMap<String, usize> = HashMap::new();
    for result in &results {
        *counts.entry(result.mode.clone()).or_insert(0) += 1;
    }

    println!("Breakdown:");
    for mode in ["STD", "NO_STD", "INDIRECT", "UNKNOWN"] {
        println!("  {}: {}", mode, counts.get(mode).unwrap_or(&0));
    }

    // Collect NO_STD repos
    let no_std_repos: Vec<_> = results
        .iter()
        .filter(|r| r.mode == "NO_STD")
        .collect();

    // Output markdown table to console as well
    println!("\n## NO_STD Repositories\n");
    println!("| Repository | Stars | Last Updated | Cargo.toml |");
    println!("|------------|-------|--------------|------------|");
    for result in no_std_repos {
        let github_url = format!(
            "https://github.com/{}/blob/HEAD/{}",
            result.repo, result.file
        );
        println!(
            "| [{}](https://github.com/{}) | {} | {} | [Cargo.toml]({}) |",
            result.repo, result.repo, result.stars, result.updated_at, github_url
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanner() -> Scanner {
        Scanner::new(None, false).unwrap()
    }

    #[test]
    fn test_no_lightning_dependency() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = "1.0"
tokio = { version = "1", features = ["full"] }
"#;
        let (mode, details) = scanner().check_cargo_toml(content);
        assert_eq!(mode, StdMode::None);
        assert_eq!(details, "no lightning dependency");
    }

    #[test]
    fn test_lightning_default_features_std() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
lightning = "0.0.123"
"#;
        let (mode, details) = scanner().check_cargo_toml(content);
        assert_eq!(mode, StdMode::Std);
        assert_eq!(details, "using default features");
    }

    #[test]
    fn test_lightning_explicit_version_std() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
lightning = { version = "0.0.123" }
"#;
        let (mode, details) = scanner().check_cargo_toml(content);
        assert_eq!(mode, StdMode::Std);
        assert_eq!(details, "using default features");
    }

    #[test]
    fn test_lightning_no_std() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
lightning = { version = "0.0.123", default-features = false }
"#;
        let (mode, details) = scanner().check_cargo_toml(content);
        assert_eq!(mode, StdMode::NoStd);
        assert_eq!(details, "default-features=false");
    }

    #[test]
    fn test_lightning_no_std_with_std_reenabled() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
lightning = { version = "0.0.123", default-features = false, features = ["std"] }
"#;
        let (mode, details) = scanner().check_cargo_toml(content);
        assert_eq!(mode, StdMode::Std);
        assert_eq!(details, "default-features=false but std re-enabled");
    }

    #[test]
    fn test_lightning_no_std_with_other_features() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
lightning = { version = "0.0.123", default-features = false, features = ["grind_signatures"] }
"#;
        let (mode, details) = scanner().check_cargo_toml(content);
        assert_eq!(mode, StdMode::NoStd);
        assert_eq!(details, "default-features=false");
    }

    #[test]
    fn test_lightning_invoice_no_std() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
lightning-invoice = { version = "0.34", default-features = false }
"#;
        let (mode, details) = scanner().check_cargo_toml(content);
        assert_eq!(mode, StdMode::NoStd);
        assert_eq!(details, "default-features=false");
    }

    #[test]
    fn test_ldk_node_std() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
ldk-node = "0.3"
"#;
        let (mode, details) = scanner().check_cargo_toml(content);
        assert_eq!(mode, StdMode::Std);
        assert_eq!(details, "using default features");
    }

    #[test]
    fn test_workspace_dependencies_no_std() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[workspace.dependencies]
lightning = { version = "0.0.123", default-features = false }
"#;
        let (mode, details) = scanner().check_cargo_toml(content);
        assert_eq!(mode, StdMode::NoStd);
        assert_eq!(details, "default-features=false");
    }

    #[test]
    fn test_workspace_dependencies_std() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[workspace.dependencies]
lightning = { version = "0.0.123" }
"#;
        let (mode, details) = scanner().check_cargo_toml(content);
        assert_eq!(mode, StdMode::Std);
        assert_eq!(details, "using default features");
    }

    #[test]
    fn test_dev_dependencies_no_std() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dev-dependencies]
lightning = { version = "0.0.123", default-features = false }
"#;
        let (mode, details) = scanner().check_cargo_toml(content);
        assert_eq!(mode, StdMode::NoStd);
        assert_eq!(details, "default-features=false");
    }

    #[test]
    fn test_target_cfg_dependencies() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[target.'cfg(not(target_arch = "wasm32"))'.dependencies]
lightning = { version = "0.0.123", default-features = false }
"#;
        let (mode, details) = scanner().check_cargo_toml(content);
        assert_eq!(mode, StdMode::NoStd);
        assert_eq!(details, "default-features=false");
    }

    #[test]
    fn test_multiple_lightning_crates_mixed() {
        // If any crate is no-std without std re-enabled, report NO_STD
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
lightning = { version = "0.0.123" }
lightning-invoice = { version = "0.34", default-features = false }
"#;
        let (mode, _) = scanner().check_cargo_toml(content);
        // One uses std (default), one uses no-std - we should detect the no-std
        assert_eq!(mode, StdMode::NoStd);
    }

    #[test]
    fn test_git_dependency_no_std() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
lightning = { git = "https://github.com/lightningdevkit/rust-lightning", default-features = false }
"#;
        let (mode, details) = scanner().check_cargo_toml(content);
        assert_eq!(mode, StdMode::NoStd);
        assert_eq!(details, "default-features=false");
    }

    #[test]
    fn test_path_dependency_std() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
lightning = { path = "../lightning" }
"#;
        let (mode, details) = scanner().check_cargo_toml(content);
        assert_eq!(mode, StdMode::Std);
        assert_eq!(details, "using default features");
    }

    #[test]
    fn test_is_lightning_crate() {
        assert!(is_lightning_crate("lightning"));
        assert!(is_lightning_crate("lightning-invoice"));
        assert!(is_lightning_crate("lightning-block-sync"));
        assert!(is_lightning_crate("lightning-rapid-gossip-sync"));
        assert!(is_lightning_crate("ldk"));
        assert!(is_lightning_crate("ldk-node"));
        assert!(!is_lightning_crate("serde"));
        assert!(!is_lightning_crate("tokio"));
        assert!(!is_lightning_crate("lightningcss")); // should not match
    }
}
