use std::collections::{HashMap, VecDeque};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

const TARGET_NAME: &str = "chanmon_consistency_target";
const TEST_NAME: &str = "run_test_cases";
const CASE_DIR: &str = "test_cases/chanmon_consistency";
const DEFAULT_SINCE: &str = "3 months ago";
const DEFAULT_CACHE_DIR: &str = ".chanmon-bisect-cache";
const DEFAULT_FAKE_HASHES_RUSTFLAGS: &str = "--cfg=fuzzing --cfg=secp256k1_fuzz --cfg=hashes_fuzz";
const DEFAULT_REAL_HASHES_RUSTFLAGS: &str = "--cfg=fuzzing --cfg=secp256k1_fuzz";
const DEFAULT_TOOLCHAIN: &str = "1.75.0";
const DEFAULT_BRANCH: &str = "HEAD";
const SPLIT_REAL_HASHES_TARGET_PATH: &str =
	"fuzz/fuzz-real-hashes/src/bin/chanmon_consistency_target.rs";
const REPLAY_RUNS: usize = 10;
const REPLAY_TIMEOUT_SECS: u64 = 10;
const REPLAY_TIMEOUT_POLL_MS: u64 = 50;
const ETA_WINDOW_CASES: usize = 32;
const ETA_MIN_SAMPLES: usize = 8;

fn main() {
	if let Err(err) = real_main() {
		eprintln!("error: {err}");
		std::process::exit(1);
	}
}

fn real_main() -> Result<(), String> {
	require_fuzz_dir()?;

	let mut args = env::args_os();
	let _program = args.next();
	let Some(cmd) = args.next() else {
		print_usage();
		return Err("missing command".to_string());
	};

	match cmd.to_string_lossy().as_ref() {
		"cache-commit" => {
			let (options, revs) = parse_build_options(args.collect(), "cache-commit")?;
			if revs.is_empty() {
				return Err("cache-commit needs at least one revision".to_string());
			}
			let total = revs.len();
			for (idx, rev) in revs.iter().enumerate() {
				cache_commit(rev.as_os_str(), Some((idx + 1, total)), &options)?;
			}
			Ok(())
		},
		"replay" => {
			let (options, args) = parse_build_options(args.collect(), "replay")?;
			let mut args = args.into_iter();
			let Some(rev) = args.next() else {
				return Err("replay needs <rev> <case-name>".to_string());
			};
			let Some(case_name) = args.next() else {
				return Err("replay needs <rev> <case-name>".to_string());
			};
			if args.next().is_some() {
				return Err("replay needs exactly <rev> <case-name>".to_string());
			}
			let case_path = case_path_from_name(Path::new(&case_name))?;
			let outcome = replay_commit(&rev, &case_path, true, &options)?;
			std::process::exit(outcome.exit_code);
		},
		"bisect" => {
			let (options, args) = parse_build_options(args.collect(), "bisect")?;
			let (branch, since) = parse_branch_and_optional_since(args, "bisect", true)?;
			let since = since.unwrap_or_else(|| OsString::from(DEFAULT_SINCE));
			bisect_cached(&branch, &since, &options)
		},
		"list-merges" => {
			let args: Vec<OsString> = args.collect();
			let (branch, since) = parse_branch_and_optional_since(args, "list-merges", true)?;
			let since = since.unwrap_or_else(|| OsString::from(DEFAULT_SINCE));
			for commit in merge_commits_since(&since, &branch)? {
				println!("{commit}");
			}
			Ok(())
		},
		"list-cache" => {
			let (options, args) = parse_build_options(args.collect(), "list-cache")?;
			if !args.is_empty() {
				return Err("list-cache accepts only optional --cfg <name> arguments".to_string());
			}
			for entry in cached_commits_sorted(OsStr::new(DEFAULT_BRANCH), &options)? {
				println!("{} {}", entry.commit, entry.variant.summary());
			}
			Ok(())
		},
		"help" | "-h" | "--help" => {
			print_usage();
			Ok(())
		},
		_ => {
			print_usage();
			Err(format!("unknown command: {}", cmd.to_string_lossy()))
		},
	}
}

fn print_usage() {
	eprintln!(
		"Usage:
  cargo run --bin chanmon_consistency_bisect -- cache-commit [--cfg <name>] <rev> [<rev>...]
  cargo run --bin chanmon_consistency_bisect -- replay [--cfg <name>] <rev> <case-name>
  cargo run --bin chanmon_consistency_bisect -- bisect [--cfg <name>] [<branch>] [<since>]
  cargo run --bin chanmon_consistency_bisect -- list-merges [<branch>] [<since>]
  cargo run --bin chanmon_consistency_bisect -- list-cache [--cfg <name>]

Run this from the fuzz directory.
If <branch> is omitted, {DEFAULT_BRANCH} is used. If <since> is omitted, {DEFAULT_SINCE} is used.
<branch> may also be provided as --branch <rev>.
--cfg <name> may be repeated. Requested cfgs are only enabled for commits whose Cargo manifests
declare the cfg in check-cfg.
`bisect` automatically caches any missing first-parent merge revisions in the selected window, plus the selected tip.

Environment:
  CHANMON_CACHE_DIR   Override the cache root, default {DEFAULT_CACHE_DIR}
  CHANMON_RUSTFLAGS   Override build RUSTFLAGS for both legacy and split fuzz crates
  CHANMON_TOOLCHAIN   Override rustup toolchain, default {DEFAULT_TOOLCHAIN}"
	);
}

fn require_fuzz_dir() -> Result<(), String> {
	let cwd = env::current_dir().map_err(|err| format!("failed to get cwd: {err}"))?;
	let legacy_target = cwd.join("src/bin").join(format!("{TARGET_NAME}.rs"));
	let split_target = cwd.join("fuzz-real-hashes/src/bin").join(format!("{TARGET_NAME}.rs"));
	if !cwd.join("Cargo.toml").is_file() || (!legacy_target.is_file() && !split_target.is_file()) {
		return Err("run this tool from the fuzz directory".to_string());
	}
	Ok(())
}

#[derive(Clone)]
struct BuildOptions {
	requested_cfgs: Vec<String>,
}

fn parse_build_options(
	args: Vec<OsString>, command_name: &str,
) -> Result<(BuildOptions, Vec<OsString>), String> {
	let mut requested_cfgs = Vec::new();
	let mut remaining = Vec::new();
	let mut idx = 0usize;

	while idx < args.len() {
		let arg = &args[idx];
		if arg == "--cfg" {
			idx += 1;
			let Some(value) = args.get(idx) else {
				return Err(format!("{command_name} requires a cfg name after --cfg"));
			};
			requested_cfgs.push(parse_cfg_name(value, command_name)?);
		} else if let Some(value) = arg.to_string_lossy().strip_prefix("--cfg=") {
			requested_cfgs.push(validate_cfg_name(value, command_name)?);
		} else {
			remaining.push(arg.clone());
		}
		idx += 1;
	}

	requested_cfgs.sort();
	requested_cfgs.dedup();
	Ok((BuildOptions { requested_cfgs }, remaining))
}

fn parse_cfg_name(value: &OsStr, command_name: &str) -> Result<String, String> {
	let Some(value) = value.to_str() else {
		return Err(format!("{command_name} cfg names must be valid utf-8"));
	};
	validate_cfg_name(value, command_name)
}

fn validate_cfg_name(value: &str, command_name: &str) -> Result<String, String> {
	let mut chars = value.chars();
	let Some(first) = chars.next() else {
		return Err(format!("{command_name} cfg names must not be empty"));
	};
	if !(first == '_' || first.is_ascii_alphabetic()) {
		return Err(format!("{command_name} invalid cfg name: {value}"));
	}
	if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
		return Err(format!("{command_name} invalid cfg name: {value}"));
	}
	Ok(value.to_string())
}

fn parse_branch_and_optional_since(
	args: Vec<OsString>, command_name: &str, allow_since: bool,
) -> Result<(OsString, Option<OsString>), String> {
	let mut branch = OsString::from(DEFAULT_BRANCH);
	let mut branch_was_set = false;
	let mut since = None;
	let mut idx = 0usize;

	while idx < args.len() {
		let arg = &args[idx];
		if arg == "--branch" {
			idx += 1;
			let Some(value) = args.get(idx) else {
				return Err(format!("{command_name} requires a revision after --branch"));
			};
			branch = value.clone();
			branch_was_set = true;
		} else if allow_since {
			if !branch_was_set && resolve_commit(arg).is_ok() {
				branch = arg.clone();
				branch_was_set = true;
			} else {
				if since.is_some() {
					return Err(format!(
						"{command_name} accepts at most one <branch> and one <since> argument"
					));
				}
				since = Some(arg.clone());
			}
		} else {
			return Err(format!("{command_name} only accepts optional --branch <rev>"));
		}
		idx += 1;
	}

	Ok((branch, since))
}

fn repo_root() -> Result<PathBuf, String> {
	let output = run_capture(
		Command::new("git").arg("rev-parse").arg("--show-toplevel"),
		"git rev-parse --show-toplevel",
	)?;
	Ok(PathBuf::from(output.trim()))
}

fn cache_root() -> Result<PathBuf, String> {
	let cwd = env::current_dir().map_err(|err| format!("failed to get cwd: {err}"))?;
	let raw = env::var_os("CHANMON_CACHE_DIR").unwrap_or_else(|| OsString::from(DEFAULT_CACHE_DIR));
	let path = PathBuf::from(raw);
	let abs = if path.is_absolute() { path } else { cwd.join(path) };
	Ok(abs)
}

fn commits_root() -> Result<PathBuf, String> {
	Ok(cache_root()?.join("commits"))
}

fn shared_target_dir() -> Result<PathBuf, String> {
	Ok(cache_root()?.join("cargo-target").join(toolchain_dir_name()))
}

fn target_dir_for_variant(variant: &BuildVariant) -> Result<PathBuf, String> {
	if variant.enabled_cfgs.is_empty() {
		return shared_target_dir();
	}
	Ok(cache_root()?.join("cargo-target").join(format!(
		"{}-cfg-{}",
		toolchain_dir_name(),
		variant.enabled_cfgs.join("-")
	)))
}

fn builder_repo_dir() -> Result<PathBuf, String> {
	Ok(cache_root()?.join("builder-repo"))
}

fn toolchain() -> OsString {
	env::var_os("CHANMON_TOOLCHAIN").unwrap_or_else(|| OsString::from(DEFAULT_TOOLCHAIN))
}

fn toolchain_dir_name() -> String {
	toolchain()
		.to_string_lossy()
		.chars()
		.map(|ch| if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' { ch } else { '_' })
		.collect()
}

fn rustup_tool_command(tool: &str) -> Command {
	let mut command = Command::new("rustup");
	command.arg("run").arg(toolchain()).arg(tool);
	command
}

fn ensure_cache_dirs() -> Result<(), String> {
	fs::create_dir_all(commits_root()?)
		.map_err(|err| format!("failed to create cache dir: {err}"))?;
	fs::create_dir_all(shared_target_dir()?)
		.map_err(|err| format!("failed to create cargo target dir: {err}"))?;
	Ok(())
}

fn ensure_builder_repo() -> Result<PathBuf, String> {
	let root = repo_root()?;
	let builder_repo = builder_repo_dir()?;
	if !builder_repo.join(".git").exists() {
		fs::create_dir_all(cache_root()?)
			.map_err(|err| format!("failed to create cache root: {err}"))?;
		run_status(
			Command::new("git")
				.arg("clone")
				.arg("--quiet")
				.arg("--no-checkout")
				.arg(&root)
				.arg(&builder_repo),
			"git clone builder repo",
		)?;
	}
	run_status(
		Command::new("git").current_dir(&builder_repo).arg("fetch").arg("--quiet").arg("origin"),
		"git fetch builder repo",
	)?;
	if source_has_remote_tracking_refs(&root)? {
		run_status(
			Command::new("git")
				.current_dir(&builder_repo)
				.arg("fetch")
				.arg("--quiet")
				.arg("origin")
				.arg("+refs/remotes/*:refs/remotes/*"),
			"git fetch builder repo remote-tracking refs",
		)?;
	}
	Ok(builder_repo)
}

fn source_has_remote_tracking_refs(root: &Path) -> Result<bool, String> {
	let output = run_capture(
		Command::new("git")
			.current_dir(root)
			.arg("for-each-ref")
			.arg("--format=%(refname)")
			.arg("refs/remotes"),
		"git for-each-ref refs/remotes",
	)?;
	Ok(!lines(&output).is_empty())
}

fn merge_commits_since(since: &OsStr, branch: &OsStr) -> Result<Vec<String>, String> {
	let root = repo_root()?;
	let output = run_capture(
		Command::new("git")
			.current_dir(root)
			.arg("rev-list")
			.arg("--reverse")
			.arg("--first-parent")
			.arg("--merges")
			.arg(format!("--since={}", since.to_string_lossy()))
			.arg(branch),
		"git rev-list merges",
	)?;
	let mut commits = lines(&output);
	let tip = resolve_commit(branch)?;
	if !commits.iter().any(|commit| commit == &tip) {
		commits.push(tip);
	}
	Ok(commits)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetLayout {
	LegacyFakeHashes,
	SplitRealHashes,
}

impl TargetLayout {
	fn as_str(self) -> &'static str {
		match self {
			Self::LegacyFakeHashes => "legacy-fake-hashes",
			Self::SplitRealHashes => "split-real-hashes",
		}
	}

	fn manifest_path(self) -> &'static str {
		match self {
			Self::LegacyFakeHashes => "fuzz/Cargo.toml",
			Self::SplitRealHashes => "fuzz/fuzz-real-hashes/Cargo.toml",
		}
	}

	fn default_rustflags(self) -> &'static str {
		match self {
			Self::LegacyFakeHashes => DEFAULT_FAKE_HASHES_RUSTFLAGS,
			Self::SplitRealHashes => DEFAULT_REAL_HASHES_RUSTFLAGS,
		}
	}

	fn replay_current_dir(self, replay_dir: &Path) -> PathBuf {
		match self {
			Self::LegacyFakeHashes => replay_dir.to_path_buf(),
			Self::SplitRealHashes => replay_dir.join("fuzz-real-hashes"),
		}
	}
}

#[derive(Clone)]
struct BuildVariant {
	layout: TargetLayout,
	enabled_cfgs: Vec<String>,
	cache_key: String,
}

impl BuildVariant {
	fn summary(&self) -> String {
		if self.enabled_cfgs.is_empty() {
			format!("({})", self.layout.as_str())
		} else {
			format!("({}, cfg {})", self.layout.as_str(), self.enabled_cfgs.join(","))
		}
	}
}

fn rustflags_for_variant(variant: &BuildVariant) -> OsString {
	let mut rustflags = env::var_os("CHANMON_RUSTFLAGS")
		.unwrap_or_else(|| OsString::from(variant.layout.default_rustflags()));
	for cfg in &variant.enabled_cfgs {
		rustflags.push(" --cfg=");
		rustflags.push(cfg);
	}
	rustflags
}

fn target_layout_for_commit(commit: &str) -> Result<TargetLayout, String> {
	if commit_has_path(commit, SPLIT_REAL_HASHES_TARGET_PATH)? {
		Ok(TargetLayout::SplitRealHashes)
	} else {
		Ok(TargetLayout::LegacyFakeHashes)
	}
}

fn build_variant_for_commit(commit: &str, options: &BuildOptions) -> Result<BuildVariant, String> {
	let layout = target_layout_for_commit(commit)?;
	let mut enabled_cfgs = Vec::new();
	for cfg in &options.requested_cfgs {
		if commit_supports_cfg(commit, cfg)? {
			enabled_cfgs.push(cfg.clone());
		}
	}
	let cache_key = cache_key_for_commit(commit, &enabled_cfgs);
	Ok(BuildVariant { layout, enabled_cfgs, cache_key })
}

fn cache_key_for_commit(commit: &str, enabled_cfgs: &[String]) -> String {
	if enabled_cfgs.is_empty() {
		commit.to_string()
	} else {
		format!("{commit}-{}", enabled_cfgs.join("-"))
	}
}

fn commit_supports_cfg(commit: &str, cfg: &str) -> Result<bool, String> {
	let root = repo_root()?;
	let pattern = format!("\"cfg({cfg})\"");
	let status = Command::new("git")
		.current_dir(root)
		.arg("grep")
		.arg("--fixed-strings")
		.arg("--quiet")
		.arg(&pattern)
		.arg(commit)
		.arg("--")
		.arg("Cargo.toml")
		.arg("fuzz/Cargo.toml")
		.arg("fuzz/fuzz-real-hashes/Cargo.toml")
		.arg("fuzz/fuzz-fake-hashes/Cargo.toml")
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.status()
		.map_err(|err| format!("failed to run git grep for cfg({cfg}) in {commit}: {err}"))?;
	match status.code() {
		Some(0) => Ok(true),
		Some(1) => Ok(false),
		_ => Err(format!(
			"git grep for cfg({cfg}) in {commit} failed with status {}",
			exit_code(status)
		)),
	}
}

fn commit_has_path(commit: &str, path: &str) -> Result<bool, String> {
	let root = repo_root()?;
	let status = Command::new("git")
		.current_dir(root)
		.arg("cat-file")
		.arg("-e")
		.arg(format!("{commit}:{path}"))
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.status()
		.map_err(|err| format!("failed to run git cat-file -e for {commit}:{path}: {err}"))?;
	Ok(status.success())
}

fn resolve_commit(rev: &OsStr) -> Result<String, String> {
	let root = repo_root()?;
	run_capture(
		Command::new("git")
			.current_dir(root)
			.arg("rev-parse")
			.arg("--verify")
			.arg(format!("{}^{{commit}}", rev.to_string_lossy())),
		"git rev-parse --verify",
	)
	.map(|value| value.trim().to_string())
}

fn commit_cache_dir(cache_key: &str) -> Result<PathBuf, String> {
	Ok(commits_root()?.join(cache_key))
}

fn commit_binary_path(cache_key: &str) -> Result<PathBuf, String> {
	Ok(commit_cache_dir(cache_key)?.join(TARGET_NAME))
}

fn commit_log_path(cache_key: &str) -> Result<PathBuf, String> {
	Ok(commit_cache_dir(cache_key)?.join("build.log"))
}

fn commit_build_summary_path(cache_key: &str) -> Result<PathBuf, String> {
	Ok(commit_cache_dir(cache_key)?.join("build-summary.txt"))
}

fn commit_info_path(cache_key: &str) -> Result<PathBuf, String> {
	Ok(commit_cache_dir(cache_key)?.join("build-info.txt"))
}

fn commit_results_dir(cache_key: &str) -> Result<PathBuf, String> {
	Ok(commit_cache_dir(cache_key)?.join("results"))
}

fn replay_result_path(cache_key: &str, case_hex: &str) -> Result<PathBuf, String> {
	Ok(commit_results_dir(cache_key)?.join(case_hex))
}

fn write_build_info(
	commit: &str, variant: &BuildVariant, options: &BuildOptions,
) -> Result<(), String> {
	let root = repo_root()?;
	let short = run_capture(
		Command::new("git").current_dir(&root).arg("rev-parse").arg("--short").arg(commit),
		"git rev-parse --short",
	)?;
	let subject = run_capture(
		Command::new("git").current_dir(&root).arg("log").arg("-1").arg("--format=%s").arg(commit),
		"git log --format=%s",
	)?;
	let rustc =
		run_capture(rustup_tool_command("rustc").arg("--version"), "rustup run rustc --version")?;
	let cargo =
		run_capture(rustup_tool_command("cargo").arg("--version"), "rustup run cargo --version")?;
	let built_at =
		run_capture(Command::new("date").arg("-u").arg("+%Y-%m-%dT%H:%M:%SZ"), "date -u")?;
	let target_dir = target_dir_for_variant(variant)?;
	let contents = format!(
		"commit={commit}\ncache_key={}\nshort={}\nsubject={}\nbuilt_at={}\nrustc={}\ncargo={}\nlayout={}\nmanifest={}\nrequested_cfgs={}\nenabled_cfgs={}\nrustflags={}\ntoolchain={}\ntarget_dir={}\n",
		variant.cache_key,
		short.trim(),
		subject.trim(),
		built_at.trim(),
		rustc.trim(),
		cargo.trim(),
		variant.layout.as_str(),
		variant.layout.manifest_path(),
		cfgs_to_string(&options.requested_cfgs),
		cfgs_to_string(&variant.enabled_cfgs),
		rustflags_for_variant(variant).to_string_lossy(),
		toolchain().to_string_lossy(),
		target_dir.display(),
	);
	fs::write(commit_info_path(&variant.cache_key)?, contents)
		.map_err(|err| format!("failed to write build info for {commit}: {err}"))
}

fn cfgs_to_string(cfgs: &[String]) -> String {
	if cfgs.is_empty() {
		"(none)".to_string()
	} else {
		cfgs.join(",")
	}
}

fn cache_commit(
	rev: &OsStr, progress: Option<(usize, usize)>, options: &BuildOptions,
) -> Result<CachedCommit, String> {
	let commit = resolve_commit(rev)?;
	let variant = build_variant_for_commit(&commit, options)?;
	let binary_path = commit_binary_path(&variant.cache_key)?;
	let target_dir = target_dir_for_variant(&variant)?;
	let progress_prefix =
		progress.map(|(current, total)| format!("[{current}/{total}] ")).unwrap_or_default();
	if binary_path.is_file() {
		println!("{progress_prefix}cached {commit} {}", variant.summary());
		return Ok(CachedCommit { commit, variant });
	}

	ensure_cache_dirs()?;
	let builder_repo = ensure_builder_repo()?;
	let commit_dir = commit_cache_dir(&variant.cache_key)?;
	fs::create_dir_all(&commit_dir)
		.map_err(|err| format!("failed to create cache entry for {commit}: {err}"))?;
	fs::create_dir_all(&target_dir).map_err(|err| {
		format!("failed to create cargo target dir {}: {err}", target_dir.display())
	})?;

	println!("{progress_prefix}building {commit} {}", variant.summary());
	run_status(
		Command::new("git")
			.current_dir(&builder_repo)
			.arg("checkout")
			.arg("--quiet")
			.arg("--detach")
			.arg(&commit),
		"git checkout builder repo",
	)?;

	let output = run_output(
		rustup_tool_command("cargo")
			.current_dir(&builder_repo)
			.env("CARGO_TARGET_DIR", &target_dir)
			.env("RUSTFLAGS", rustflags_for_variant(&variant))
			.arg("test")
			.arg("--manifest-path")
			.arg(variant.layout.manifest_path())
			.arg("--bin")
			.arg(TARGET_NAME)
			.arg("--no-run")
			.arg("--message-format=json"),
		"rustup run cargo test --no-run",
	)?;
	fs::write(
		commit_log_path(&variant.cache_key)?,
		[&output.stdout[..], &output.stderr[..]].concat(),
	)
	.map_err(|err| format!("failed to write build log for {commit}: {err}"))?;
	if !output.status.success() {
		let summary = cargo_failure_summary(&output.stdout, &output.stderr);
		fs::write(commit_build_summary_path(&variant.cache_key)?, format!("{summary}\n"))
			.map_err(|err| format!("failed to write build summary for {commit}: {err}"))?;
		return Err(format!(
			"build failed for {commit}: {summary}\nsee {}",
			commit_log_path(&variant.cache_key)?.display(),
		));
	}

	let executable = cargo_executable_from_stdout(&output.stdout)?;
	fs::copy(&executable, &binary_path).map_err(|err| {
		format!(
			"failed to copy built binary from {} to {}: {err}",
			executable.display(),
			binary_path.display()
		)
	})?;
	write_build_info(&commit, &variant, options)?;
	println!("{progress_prefix}stored {}", binary_path.display());
	Ok(CachedCommit { commit, variant })
}

fn cargo_executable_from_stdout(stdout: &[u8]) -> Result<PathBuf, String> {
	let stdout = String::from_utf8_lossy(stdout);
	let mut executable = None;
	for line in stdout.lines() {
		let message: CargoMessage = serde_json::from_str(line)
			.map_err(|err| format!("failed to parse cargo json message: {err}"))?;
		if message.reason != "compiler-artifact" {
			continue;
		}
		let Some(target) = message.target else {
			continue;
		};
		if target.name != TARGET_NAME || !target.test {
			continue;
		}
		if let Some(path) = message.executable {
			executable = Some(PathBuf::from(path));
		}
	}
	executable.ok_or_else(|| "cargo did not report a test executable path".to_string())
}

#[derive(Deserialize)]
struct CargoMessage {
	reason: String,
	target: Option<CargoTarget>,
	executable: Option<PathBuf>,
	message: Option<CargoDiagnosticMessage>,
}

#[derive(Deserialize)]
struct CargoTarget {
	name: String,
	test: bool,
}

#[derive(Deserialize)]
struct CargoDiagnosticMessage {
	level: String,
	message: String,
	rendered: Option<String>,
}

#[derive(Clone, Copy)]
struct ReplayOutcome {
	verdict: ReplayVerdict,
	exit_code: i32,
	cached: bool,
}

enum ReplayRunResult {
	Exited(ExitStatus),
	TimedOut,
}

impl ReplayRunResult {
	fn success(&self) -> bool {
		matches!(self, Self::Exited(status) if status.success())
	}

	fn exit_code(&self) -> i32 {
		match self {
			Self::Exited(status) => exit_code(*status),
			Self::TimedOut => ReplayVerdict::Fail.exit_code(),
		}
	}
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplayVerdict {
	Pass,
	Fail,
	Flake,
}

impl ReplayVerdict {
	fn is_pass(self) -> bool {
		matches!(self, Self::Pass)
	}

	fn as_str(self) -> &'static str {
		match self {
			Self::Pass => "pass",
			Self::Fail => "fail",
			Self::Flake => "flake",
		}
	}

	fn exit_code(self) -> i32 {
		match self {
			Self::Pass => 0,
			Self::Fail => 101,
			Self::Flake => 102,
		}
	}
}

fn replay_commit(
	rev: &OsStr, input_file: &Path, inherit_stdio: bool, options: &BuildOptions,
) -> Result<ReplayOutcome, String> {
	let cached = cache_commit(rev, None, options)?;
	replay_cached_commit(&cached, input_file, inherit_stdio, REPLAY_RUNS)
}

fn replay_cached_commit(
	cached: &CachedCommit, input_file: &Path, inherit_stdio: bool, runs: usize,
) -> Result<ReplayOutcome, String> {
	let commit = cached.commit.as_str();
	let variant = &cached.variant;
	let binary_path = commit_binary_path(&variant.cache_key)?;
	let input_abs = absolute_path(input_file)?;
	if !input_abs.is_file() {
		return Err(format!("input file not found: {}", input_abs.display()));
	}
	let input_hex = case_hex(&input_abs)?;

	if !inherit_stdio {
		if let Some(outcome) = read_replay_result(&variant.cache_key, &input_hex)? {
			return Ok(outcome);
		}
	}

	if inherit_stdio {
		let result = run_replay_once(variant.layout, &binary_path, &input_abs, true)?;
		let verdict = if result.success() { ReplayVerdict::Pass } else { ReplayVerdict::Fail };
		return Ok(ReplayOutcome { verdict, exit_code: result.exit_code(), cached: false });
	}

	let mut handles = Vec::with_capacity(runs);
	for _ in 0..runs {
		let binary_path = binary_path.clone();
		let input_abs = input_abs.clone();
		let layout = variant.layout;
		handles
			.push(thread::spawn(move || run_replay_once(layout, &binary_path, &input_abs, false)));
	}

	let mut saw_pass = false;
	let mut saw_fail = false;
	for handle in handles {
		let result =
			handle.join().map_err(|_| format!("replay thread panicked for {commit}"))??;
		if result.success() {
			saw_pass = true;
		} else {
			saw_fail = true;
		}
	}
	let verdict = match (saw_pass, saw_fail) {
		(true, true) => ReplayVerdict::Flake,
		(true, false) => ReplayVerdict::Pass,
		(false, true) => ReplayVerdict::Fail,
		(false, false) => {
			return Err(format!(
				"internal error: no replay outcomes recorded for {} on {commit}",
				input_abs.display()
			))
		},
	};
	write_replay_result(&variant.cache_key, &input_hex, verdict)?;
	Ok(ReplayOutcome { verdict, exit_code: verdict.exit_code(), cached: false })
}

fn run_replay_once(
	layout: TargetLayout, binary_path: &Path, input_abs: &Path, inherit_stdio: bool,
) -> Result<ReplayRunResult, String> {
	let replay_dir = create_replay_dir()?;
	let replay_case_dir = replay_dir.join(CASE_DIR);
	fs::create_dir_all(&replay_case_dir).map_err(|err| {
		format!("failed to create replay case dir {}: {err}", replay_case_dir.display())
	})?;
	fs::copy(&input_abs, replay_case_dir.join("repro")).map_err(|err| {
		format!(
			"failed to copy input file {} into replay dir {}: {err}",
			input_abs.display(),
			replay_case_dir.display()
		)
	})?;

	let replay_current_dir = layout.replay_current_dir(&replay_dir);
	fs::create_dir_all(&replay_current_dir).map_err(|err| {
		format!("failed to create replay cwd {}: {err}", replay_current_dir.display())
	})?;

	let mut command = Command::new(&binary_path);
	command.current_dir(&replay_current_dir);
	command.arg(TEST_NAME).arg("--exact");
	if inherit_stdio {
		command.arg("--nocapture");
		command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
	} else {
		command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
	}

	let run_result = run_replay_command_with_timeout(&mut command, binary_path, inherit_stdio);
	let cleanup_result = remove_dir_all_if_exists(&replay_dir);
	match (run_result, cleanup_result) {
		(Ok(result), Ok(())) => Ok(result),
		(Err(err), _) => Err(err),
		(Ok(_), Err(err)) => Err(err),
	}
}

fn run_replay_command_with_timeout(
	command: &mut Command, binary_path: &Path, inherit_stdio: bool,
) -> Result<ReplayRunResult, String> {
	let mut child = command
		.spawn()
		.map_err(|err| format!("failed to run cached binary {}: {err}", binary_path.display()))?;
	let timeout = Duration::from_secs(REPLAY_TIMEOUT_SECS);
	let poll_interval = Duration::from_millis(REPLAY_TIMEOUT_POLL_MS);
	let started_at = Instant::now();
	loop {
		if let Some(status) = child.try_wait().map_err(|err| {
			format!("failed to wait on cached binary {}: {err}", binary_path.display())
		})? {
			return Ok(ReplayRunResult::Exited(status));
		}
		if started_at.elapsed() >= timeout {
			if inherit_stdio {
				eprintln!("replay timed out after {REPLAY_TIMEOUT_SECS}s");
			}
			if let Err(err) = child.kill() {
				if child
					.try_wait()
					.map_err(|err| {
						format!(
							"failed to wait on timed out replay {}: {err}",
							binary_path.display()
						)
					})?
					.is_none()
				{
					return Err(format!(
						"failed to kill timed out replay {}: {err}",
						binary_path.display()
					));
				}
			}
			child.wait().map_err(|err| {
				format!("failed to reap timed out replay {}: {err}", binary_path.display())
			})?;
			return Ok(ReplayRunResult::TimedOut);
		}
		thread::sleep(poll_interval);
	}
}

fn read_replay_result(cache_key: &str, case_hex: &str) -> Result<Option<ReplayOutcome>, String> {
	let path = replay_result_path(cache_key, case_hex)?;
	if !path.is_file() {
		return Ok(None);
	}
	let contents = fs::read_to_string(&path)
		.map_err(|err| format!("failed to read replay result {}: {err}", path.display()))?;
	let trimmed = contents.trim();
	let verdict = match trimmed {
		"pass" => ReplayVerdict::Pass,
		"fail" => ReplayVerdict::Fail,
		"flake" => ReplayVerdict::Flake,
		_ => {
			return Err(format!(
				"invalid replay result {}: expected 'pass', 'fail', or 'flake', found {trimmed:?}",
				path.display()
			))
		},
	};
	Ok(Some(ReplayOutcome { verdict, exit_code: verdict.exit_code(), cached: true }))
}

fn write_replay_result(
	cache_key: &str, case_hex: &str, verdict: ReplayVerdict,
) -> Result<(), String> {
	let dir = commit_results_dir(cache_key)?;
	fs::create_dir_all(&dir)
		.map_err(|err| format!("failed to create replay result dir {}: {err}", dir.display()))?;
	let path = replay_result_path(cache_key, case_hex)?;
	let contents = format!("{}\n", verdict.as_str());
	fs::write(&path, contents)
		.map_err(|err| format!("failed to write replay result {}: {err}", path.display()))
}

struct CachedCommit {
	commit: String,
	variant: BuildVariant,
}

struct CaseResult {
	first_nonpass_commit: Option<String>,
	first_nonpass_verdict: Option<ReplayVerdict>,
	nonpassing_from_start: bool,
	commits_tested: usize,
	cached_commits_tested: usize,
}

struct CaseSummary {
	case_hex: String,
	result: CaseResult,
}

fn bisect_cached(branch: &OsStr, since: &OsStr, options: &BuildOptions) -> Result<(), String> {
	let expected_commits = merge_commits_since(since, branch)?;
	cache_missing_binaries(&expected_commits, options)?;

	let cached = cached_commits_for_expected(&expected_commits, options)?;
	if cached.is_empty() {
		return Err("no cached commits found for the selected bisect window".to_string());
	}

	let cases = current_cases()?;
	if cases.is_empty() {
		return Err(format!("no test cases found in {CASE_DIR}"));
	}

	let mut summaries = Vec::new();
	let total_cases = cases.len();
	let max_case_hex_len = cases
		.iter()
		.map(|case_path| case_hex(case_path).map(|hex| hex.len()))
		.collect::<Result<Vec<_>, _>>()?
		.into_iter()
		.max()
		.unwrap_or(0);
	let mut recent_case_durations = VecDeque::new();
	for (idx, case_path) in cases.iter().enumerate() {
		let case_start = Instant::now();
		let case_hex = case_hex(case_path)?;
		let result = first_breaking_commit_for_case(case_path, &cached)?;
		let case_elapsed = case_start.elapsed();
		recent_case_durations.push_back(case_elapsed);
		if recent_case_durations.len() > ETA_WINDOW_CASES {
			recent_case_durations.pop_front();
		}
		let completed_cases = idx + 1;
		let remaining_cases = total_cases - completed_cases;
		let eta = if remaining_cases == 0 || recent_case_durations.len() < ETA_MIN_SAMPLES {
			None
		} else {
			let average_case_secs =
				recent_case_durations.iter().map(Duration::as_secs_f64).sum::<f64>()
					/ recent_case_durations.len() as f64;
			Some(Duration::from_secs_f64(average_case_secs * remaining_cases as f64))
		};
		let eta_suffix =
			eta.map(|eta| format!(" (eta {})", format_duration(eta))).unwrap_or_default();
		println!(
			"[{}/{}] {:width$} {:<5} {} (commits {}, cached {}){}",
			completed_cases,
			total_cases,
			case_hex,
			case_result_status(&result),
			describe_case_result_detail(&result)?,
			result.commits_tested,
			result.cached_commits_tested,
			eta_suffix,
			width = max_case_hex_len
		);
		summaries.push(CaseSummary { case_hex, result });
	}

	println!("\noverview:");
	let mut printed_failure_section = false;
	for entry in &cached {
		let matching_cases: Vec<&CaseSummary> = summaries
			.iter()
			.filter(|summary| {
				summary.result.first_nonpass_commit.as_deref() == Some(entry.commit.as_str())
			})
			.collect();
		if matching_cases.is_empty() {
			continue;
		}
		printed_failure_section = true;
		let heading = if matching_cases.iter().any(|summary| summary.result.nonpassing_from_start) {
			format!(
				"{} {} (already non-passing at oldest cached merge commit)",
				short_commit(&entry.commit)?,
				commit_summary(&entry.commit)?
			)
		} else {
			format!("{} {}", short_commit(&entry.commit)?, commit_summary(&entry.commit)?)
		};
		println!("\n{heading}");
		println!("{}", "-".repeat(heading.len()));
		for summary in matching_cases {
			let status_suffix = summary
				.result
				.first_nonpass_verdict
				.map(|verdict| format!(" [{}]", verdict.as_str()))
				.unwrap_or_default();
			println!("{}{}", summary.case_hex, status_suffix);
		}
	}

	if !printed_failure_section {
		println!("\nNo failing or flaky testcases across cached merge commits.");
	}
	Ok(())
}

fn first_breaking_commit_for_case(
	case_path: &Path, cached: &[CachedCommit],
) -> Result<CaseResult, String> {
	let mut outcomes = HashMap::new();
	let mut commits_tested = 0usize;
	let mut cached_commits_tested = 0usize;
	let mut replay_at = |idx: usize, runs: usize| -> Result<ReplayOutcome, String> {
		if let Some(result) = outcomes.get(&(idx, runs)) {
			return Ok(*result);
		}
		let outcome = replay_cached_commit(&cached[idx], case_path, false, runs)?;
		commits_tested += 1;
		if outcome.cached {
			cached_commits_tested += 1;
		}
		outcomes.insert((idx, runs), outcome);
		Ok(outcome)
	};

	let latest_idx = cached.len() - 1;
	let latest = replay_at(latest_idx, REPLAY_RUNS)?;
	if latest.verdict.is_pass() {
		return Ok(CaseResult {
			first_nonpass_commit: None,
			first_nonpass_verdict: None,
			nonpassing_from_start: false,
			commits_tested,
			cached_commits_tested,
		});
	}

	let bisect_runs = REPLAY_RUNS;

	let oldest = replay_at(0, bisect_runs)?;
	if !oldest.verdict.is_pass() {
		return Ok(CaseResult {
			first_nonpass_commit: Some(cached[0].commit.clone()),
			first_nonpass_verdict: Some(oldest.verdict),
			nonpassing_from_start: true,
			commits_tested,
			cached_commits_tested,
		});
	}

	let mut transition_idx = latest_idx;
	for idx in (0..latest_idx).rev() {
		let outcome = replay_at(idx, bisect_runs)?;
		if outcome.verdict.is_pass() {
			break;
		}
		transition_idx = idx;
	}

	let nonpass = replay_at(transition_idx, bisect_runs)?;
	Ok(CaseResult {
		first_nonpass_commit: Some(cached[transition_idx].commit.clone()),
		first_nonpass_verdict: Some(nonpass.verdict),
		nonpassing_from_start: false,
		commits_tested,
		cached_commits_tested,
	})
}

fn case_result_status(result: &CaseResult) -> &'static str {
	if result.first_nonpass_commit.is_none() {
		return "PASS";
	}

	match result.first_nonpass_verdict {
		Some(ReplayVerdict::Pass) | None => "PASS",
		Some(ReplayVerdict::Fail) => "FAIL",
		Some(ReplayVerdict::Flake) => "FLAKE",
	}
}

fn describe_case_result_detail(result: &CaseResult) -> Result<String, String> {
	if result.first_nonpass_commit.is_none() {
		return Ok("latest".to_string());
	}

	let commit = result
		.first_nonpass_commit
		.as_deref()
		.ok_or_else(|| "missing first non-pass commit".to_string())?;
	result.first_nonpass_verdict.ok_or_else(|| "missing first non-pass verdict".to_string())?;
	if result.nonpassing_from_start {
		Ok(format!("oldest {}", short_commit(commit)?))
	} else {
		Ok(short_commit(commit)?)
	}
}

fn format_duration(duration: Duration) -> String {
	let total_secs = duration.as_secs();
	let hours = total_secs / 3600;
	let minutes = (total_secs % 3600) / 60;
	let seconds = total_secs % 60;
	if hours > 0 {
		format!("{hours}h{minutes:02}m{seconds:02}s")
	} else if minutes > 0 {
		format!("{minutes}m{seconds:02}s")
	} else {
		format!("{seconds}s")
	}
}

fn cached_commits_sorted(
	branch: &OsStr, options: &BuildOptions,
) -> Result<Vec<CachedCommit>, String> {
	let commits = all_first_parent_commits_with_tip(branch)?;
	cached_commits_for_expected(&commits, options)
}

fn cached_commits_for_expected(
	expected_commits: &[String], options: &BuildOptions,
) -> Result<Vec<CachedCommit>, String> {
	let root = commits_root()?;
	if !root.is_dir() {
		return Ok(Vec::new());
	}

	let mut commits = Vec::with_capacity(expected_commits.len());
	for commit in expected_commits {
		let variant = build_variant_for_commit(commit, options)?;
		if commit_binary_path(&variant.cache_key)?.is_file() {
			commits.push(CachedCommit { commit: commit.clone(), variant });
		}
	}
	Ok(commits)
}

fn missing_cached_binaries(
	expected_commits: &[String], options: &BuildOptions,
) -> Result<Vec<String>, String> {
	let mut missing = Vec::new();
	for commit in expected_commits {
		let variant = build_variant_for_commit(commit, options)?;
		if !commit_binary_path(&variant.cache_key)?.is_file() {
			missing.push(commit.clone());
		}
	}
	Ok(missing)
}

fn cache_missing_binaries(
	expected_commits: &[String], options: &BuildOptions,
) -> Result<(), String> {
	let missing_commits = missing_cached_binaries(expected_commits, options)?;
	if missing_commits.is_empty() {
		return Ok(());
	}

	println!("caching {} missing commit(s) required for bisect", missing_commits.len());
	let total = missing_commits.len();
	for (idx, commit) in missing_commits.iter().enumerate() {
		cache_commit(OsStr::new(commit), Some((idx + 1, total)), options)?;
	}
	Ok(())
}

fn all_first_parent_commits_with_tip(branch: &OsStr) -> Result<Vec<String>, String> {
	let root = repo_root()?;
	let output = run_capture(
		Command::new("git")
			.current_dir(root)
			.arg("rev-list")
			.arg("--reverse")
			.arg("--first-parent")
			.arg("--merges")
			.arg(branch),
		"git rev-list --reverse --first-parent --merges",
	)?;
	let mut commits = lines(&output);
	let tip = resolve_commit(branch)?;
	if !commits.iter().any(|commit| commit == &tip) {
		commits.push(tip);
	}
	Ok(commits)
}

fn commit_summary(commit: &str) -> Result<String, String> {
	let root = repo_root()?;
	run_capture(
		Command::new("git").current_dir(root).arg("log").arg("-1").arg("--format=%s").arg(commit),
		"git log --format=%s",
	)
	.map(|value| value.trim().to_string())
}

fn short_commit(commit: &str) -> Result<String, String> {
	let root = repo_root()?;
	run_capture(
		Command::new("git").current_dir(root).arg("rev-parse").arg("--short").arg(commit),
		"git rev-parse --short",
	)
	.map(|value| value.trim().to_string())
}

fn create_replay_dir() -> Result<PathBuf, String> {
	let root = cache_root()?;
	fs::create_dir_all(&root)
		.map_err(|err| format!("failed to create cache root {}: {err}", root.display()))?;
	let nanos = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_err(|err| format!("system clock before UNIX_EPOCH: {err}"))?
		.as_nanos();
	for attempt in 0..1000_u32 {
		let candidate = root.join(format!("replay.{nanos}.{attempt}"));
		match fs::create_dir(&candidate) {
			Ok(()) => return Ok(candidate),
			Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
			Err(err) => {
				return Err(format!("failed to create replay dir {}: {err}", candidate.display()));
			},
		}
	}
	Err("failed to allocate a replay dir".to_string())
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
	let abs = if path.is_absolute() {
		path.to_path_buf()
	} else {
		env::current_dir().map_err(|err| format!("failed to get cwd: {err}"))?.join(path)
	};
	Ok(abs)
}

fn case_root() -> Result<PathBuf, String> {
	Ok(env::current_dir().map_err(|err| format!("failed to get cwd: {err}"))?.join(CASE_DIR))
}

fn current_cases() -> Result<Vec<PathBuf>, String> {
	let root = case_root()?;
	if !root.is_dir() {
		return Ok(Vec::new());
	}

	let mut cases = Vec::new();
	for entry in fs::read_dir(&root)
		.map_err(|err| format!("failed to read testcase dir {}: {err}", root.display()))?
	{
		let entry = entry
			.map_err(|err| format!("failed to read testcase entry in {}: {err}", root.display()))?;
		let path = entry.path();
		if path.is_file() {
			cases.push(path);
		}
	}
	cases.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
	Ok(cases)
}

fn case_path_from_name(case_name: &Path) -> Result<PathBuf, String> {
	let file_name = case_name
		.file_name()
		.ok_or_else(|| format!("invalid testcase name {}", case_name.display()))?;
	let path = case_root()?.join(file_name);
	if !path.is_file() {
		return Err(format!("testcase not found: {}", path.display()));
	}
	Ok(path)
}

fn case_hex(path: &Path) -> Result<String, String> {
	let bytes = fs::read(path)
		.map_err(|err| format!("failed to read testcase {}: {err}", path.display()))?;
	let mut hex = String::with_capacity(bytes.len() * 2);
	for byte in bytes {
		use std::fmt::Write;
		write!(&mut hex, "{byte:02x}")
			.map_err(|err| format!("failed to format testcase {} as hex: {err}", path.display()))?;
	}
	Ok(hex)
}

fn cargo_failure_summary(stdout: &[u8], stderr: &[u8]) -> String {
	let combined = [stdout, stderr].concat();
	let text = String::from_utf8_lossy(&combined);
	let mut diagnostic_summary = None;
	let mut extra_detail = None;
	let mut plain_error = None;

	for line in text.lines() {
		if let Ok(message) = serde_json::from_str::<CargoMessage>(line) {
			if message.reason != "compiler-message" {
				continue;
			}
			let Some(diagnostic) = message.message else {
				continue;
			};
			if diagnostic.level != "error" {
				continue;
			}
			diagnostic_summary = Some(diagnostic.message);
			if let Some(rendered) = diagnostic.rendered {
				if extra_detail.is_none() {
					for line in rendered.lines() {
						let trimmed = line.trim();
						if trimmed.starts_with("ld: archive member ")
							|| trimmed.starts_with("clang: error: ")
						{
							extra_detail = Some(trimmed.to_string());
							break;
						}
					}
				}
				if let Some(line) = rendered
					.lines()
					.find(|line| line.contains("Undefined symbols for architecture"))
				{
					extra_detail = Some(line.trim().to_string());
				}
			}
			continue;
		}

		if let Some(rest) = line.trim().strip_prefix("error: ") {
			plain_error = Some(rest.trim().to_string());
		}
		if extra_detail.is_none() {
			let trimmed = line.trim();
			if trimmed.starts_with("ld: archive member ") || trimmed.starts_with("clang: error: ") {
				extra_detail = Some(trimmed.to_string());
			}
		}
		if extra_detail.is_none() && line.contains("Undefined symbols for architecture") {
			extra_detail = Some(line.trim().to_string());
		}
	}

	let mut summary = diagnostic_summary
		.or(plain_error)
		.unwrap_or_else(|| format!("command failed with status {}", 1));
	if let Some(detail) = extra_detail {
		if !summary.contains(&detail) {
			summary.push_str("; ");
			summary.push_str(&detail);
		}
	}
	summary
}

fn remove_dir_all_if_exists(path: &Path) -> Result<(), String> {
	if !path.exists() {
		return Ok(());
	}
	fs::remove_dir_all(path).map_err(|err| format!("failed to remove {}: {err}", path.display()))
}

fn lines(output: &str) -> Vec<String> {
	output.lines().map(str::trim).filter(|line| !line.is_empty()).map(ToOwned::to_owned).collect()
}

fn run_capture(command: &mut Command, description: &str) -> Result<String, String> {
	let output = run_output(command, description)?;
	if !output.status.success() {
		return Err(format!(
			"{description} failed with status {}: {}",
			exit_code(output.status),
			String::from_utf8_lossy(&output.stderr).trim()
		));
	}
	String::from_utf8(output.stdout)
		.map_err(|err| format!("{description} returned non-utf8 output: {err}"))
}

fn run_status(command: &mut Command, description: &str) -> Result<(), String> {
	let status = command.status().map_err(|err| format!("failed to run {description}: {err}"))?;
	if !status.success() {
		return Err(format!("{description} failed with status {}", exit_code(status)));
	}
	Ok(())
}

fn run_output(command: &mut Command, description: &str) -> Result<std::process::Output, String> {
	command.output().map_err(|err| format!("failed to run {description}: {err}"))
}

fn exit_code(status: ExitStatus) -> i32 {
	status.code().unwrap_or(1)
}
