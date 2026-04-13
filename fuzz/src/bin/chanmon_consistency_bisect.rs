use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

const TARGET_NAME: &str = "chanmon_consistency_target";
const TEST_NAME: &str = "run_test_cases";
const CASE_DIR: &str = "test_cases/chanmon_consistency";
const DEFAULT_SINCE: &str = "3 months ago";
const DEFAULT_CACHE_DIR: &str = ".chanmon-bisect-cache";
const DEFAULT_RUSTFLAGS: &str = "--cfg=fuzzing --cfg=secp256k1_fuzz --cfg=hashes_fuzz";
const DEFAULT_TOOLCHAIN: &str = "1.75.0";
const DEFAULT_BRANCH: &str = "HEAD";
const TIP_REPLAY_RUNS: usize = 10;
const STABLE_REPLAY_RUNS: usize = 1;
const FLAKY_REPLAY_RUNS: usize = 100;

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
			let revs: Vec<OsString> = args.collect();
			if revs.is_empty() {
				return Err("cache-commit needs at least one revision".to_string());
			}
			let total = revs.len();
			for (idx, rev) in revs.iter().enumerate() {
				cache_commit(rev.as_os_str(), Some((idx + 1, total)))?;
			}
			Ok(())
		},
		"replay" => {
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
			let outcome = replay_commit(&rev, &case_path, true)?;
			std::process::exit(outcome.exit_code);
		},
		"bisect" => {
			let (branch, since) = parse_branch_and_optional_since(args.collect(), "bisect", true)?;
			let since = since.unwrap_or_else(|| OsString::from(DEFAULT_SINCE));
			bisect_cached(&branch, &since)
		},
		"list-merges" => {
			let (branch, since) =
				parse_branch_and_optional_since(args.collect(), "list-merges", true)?;
			let since = since.unwrap_or_else(|| OsString::from(DEFAULT_SINCE));
			for commit in merge_commits_since(&since, &branch)? {
				println!("{commit}");
			}
			Ok(())
		},
		"list-cache" => {
			for entry in cached_commits_sorted(OsStr::new(DEFAULT_BRANCH))? {
				println!("{}", entry.commit);
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
  cargo run --bin chanmon_consistency_bisect -- cache-commit <rev> [<rev>...]
  cargo run --bin chanmon_consistency_bisect -- replay <rev> <case-name>
  cargo run --bin chanmon_consistency_bisect -- bisect [--branch <rev>] [<since>]
  cargo run --bin chanmon_consistency_bisect -- list-merges [--branch <rev>] [<since>]
  cargo run --bin chanmon_consistency_bisect -- list-cache

Run this from the fuzz directory.
`bisect` automatically caches any missing first-parent merge revisions in the selected window, plus the selected tip.

Environment:
  CHANMON_CACHE_DIR   Override the cache root, default {DEFAULT_CACHE_DIR}
  CHANMON_RUSTFLAGS   Override build RUSTFLAGS, default {DEFAULT_RUSTFLAGS}
  CHANMON_TOOLCHAIN   Override rustup toolchain, default {DEFAULT_TOOLCHAIN}"
	);
}

fn require_fuzz_dir() -> Result<(), String> {
	let cwd = env::current_dir().map_err(|err| format!("failed to get cwd: {err}"))?;
	if !cwd.join("Cargo.toml").is_file()
		|| !cwd.join("src/bin").join(format!("{TARGET_NAME}.rs")).is_file()
	{
		return Err("run this tool from the fuzz directory".to_string());
	}
	Ok(())
}

fn parse_branch_and_optional_since(
	args: Vec<OsString>, command_name: &str, allow_since: bool,
) -> Result<(OsString, Option<OsString>), String> {
	let mut branch = OsString::from(DEFAULT_BRANCH);
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
		} else if allow_since {
			if since.is_some() {
				return Err(format!(
					"{command_name} accepts at most one <since> argument and one --branch <rev>"
				));
			}
			since = Some(arg.clone());
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

fn builder_repo_dir() -> Result<PathBuf, String> {
	Ok(cache_root()?.join("builder-repo"))
}

fn rustflags() -> OsString {
	env::var_os("CHANMON_RUSTFLAGS").unwrap_or_else(|| OsString::from(DEFAULT_RUSTFLAGS))
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
	Ok(builder_repo)
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

fn commit_cache_dir(commit: &str) -> Result<PathBuf, String> {
	Ok(commits_root()?.join(commit))
}

fn commit_binary_path(commit: &str) -> Result<PathBuf, String> {
	Ok(commit_cache_dir(commit)?.join(TARGET_NAME))
}

fn commit_log_path(commit: &str) -> Result<PathBuf, String> {
	Ok(commit_cache_dir(commit)?.join("build.log"))
}

fn commit_build_summary_path(commit: &str) -> Result<PathBuf, String> {
	Ok(commit_cache_dir(commit)?.join("build-summary.txt"))
}

fn commit_info_path(commit: &str) -> Result<PathBuf, String> {
	Ok(commit_cache_dir(commit)?.join("build-info.txt"))
}

fn commit_results_dir(commit: &str, runs: usize) -> Result<PathBuf, String> {
	Ok(commit_cache_dir(commit)?.join("results").join(runs.to_string()))
}

fn replay_result_path(commit: &str, runs: usize, case_hex: &str) -> Result<PathBuf, String> {
	Ok(commit_results_dir(commit, runs)?.join(case_hex))
}

fn write_build_info(commit: &str) -> Result<(), String> {
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
	let contents = format!(
			"commit={commit}\nshort={}\nsubject={}\nbuilt_at={}\nrustc={}\ncargo={}\nrustflags={}\ntoolchain={}\n",
			short.trim(),
			subject.trim(),
			built_at.trim(),
			rustc.trim(),
			cargo.trim(),
			rustflags().to_string_lossy(),
			toolchain().to_string_lossy(),
		);
	fs::write(commit_info_path(commit)?, contents)
		.map_err(|err| format!("failed to write build info for {commit}: {err}"))
}

fn cache_commit(rev: &OsStr, progress: Option<(usize, usize)>) -> Result<String, String> {
	let commit = resolve_commit(rev)?;
	let binary_path = commit_binary_path(&commit)?;
	let progress_prefix =
		progress.map(|(current, total)| format!("[{current}/{total}] ")).unwrap_or_default();
	if binary_path.is_file() {
		println!("{progress_prefix}cached {commit}");
		return Ok(commit);
	}

	ensure_cache_dirs()?;
	let builder_repo = ensure_builder_repo()?;
	let commit_dir = commit_cache_dir(&commit)?;
	fs::create_dir_all(&commit_dir)
		.map_err(|err| format!("failed to create cache entry for {commit}: {err}"))?;

	println!("{progress_prefix}building {commit}");
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
			.current_dir(builder_repo.join("fuzz"))
			.env("CARGO_TARGET_DIR", shared_target_dir()?)
			.env("RUSTFLAGS", rustflags())
			.arg("test")
			.arg("--bin")
			.arg(TARGET_NAME)
			.arg("--no-run")
			.arg("--message-format=json"),
		"rustup run cargo test --no-run",
	)?;
	fs::write(commit_log_path(&commit)?, [&output.stdout[..], &output.stderr[..]].concat())
		.map_err(|err| format!("failed to write build log for {commit}: {err}"))?;
	if !output.status.success() {
		let summary = cargo_failure_summary(&output.stdout, &output.stderr);
		fs::write(commit_build_summary_path(&commit)?, format!("{summary}\n"))
			.map_err(|err| format!("failed to write build summary for {commit}: {err}"))?;
		return Err(format!(
			"build failed for {commit}: {summary}\nsee {}",
			commit_log_path(&commit)?.display(),
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
	write_build_info(&commit)?;
	println!("{progress_prefix}stored {}", binary_path.display());
	Ok(commit)
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

struct ReplayOutcome {
	verdict: ReplayVerdict,
	exit_code: i32,
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
	rev: &OsStr, input_file: &Path, inherit_stdio: bool,
) -> Result<ReplayOutcome, String> {
	let commit = cache_commit(rev, None)?;
	replay_cached_commit(&commit, input_file, inherit_stdio, STABLE_REPLAY_RUNS)
}

fn replay_cached_commit(
	commit: &str, input_file: &Path, inherit_stdio: bool, runs: usize,
) -> Result<ReplayOutcome, String> {
	let binary_path = commit_binary_path(&commit)?;
	let input_abs = absolute_path(input_file)?;
	if !input_abs.is_file() {
		return Err(format!("input file not found: {}", input_abs.display()));
	}
	let input_hex = case_hex(&input_abs)?;

	if !inherit_stdio {
		if let Some(outcome) = read_replay_result(commit, runs, &input_hex)? {
			return Ok(outcome);
		}
	}

	if inherit_stdio {
		let status = run_replay_once(&binary_path, &input_abs, true)?;
		let verdict = if status.success() { ReplayVerdict::Pass } else { ReplayVerdict::Fail };
		return Ok(ReplayOutcome { verdict, exit_code: exit_code(status) });
	}

	let mut saw_pass = false;
	let mut saw_fail = false;
	for _ in 0..runs {
		let status = run_replay_once(&binary_path, &input_abs, false)?;
		if status.success() {
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
	write_replay_result(commit, runs, &input_hex, verdict)?;
	Ok(ReplayOutcome { verdict, exit_code: verdict.exit_code() })
}

fn run_replay_once(
	binary_path: &Path, input_abs: &Path, inherit_stdio: bool,
) -> Result<ExitStatus, String> {
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

	let mut command = Command::new(&binary_path);
	command.current_dir(&replay_dir);
	command.arg(TEST_NAME).arg("--exact");
	if inherit_stdio {
		command.arg("--nocapture");
		command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
	} else {
		command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
	}

	let status = command
		.status()
		.map_err(|err| format!("failed to run cached binary {}: {err}", binary_path.display()))?;
	remove_dir_all_if_exists(&replay_dir)?;
	Ok(status)
}

fn read_replay_result(
	commit: &str, runs: usize, case_hex: &str,
) -> Result<Option<ReplayOutcome>, String> {
	let path = replay_result_path(commit, runs, case_hex)?;
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
	Ok(Some(ReplayOutcome { verdict, exit_code: verdict.exit_code() }))
}

fn write_replay_result(
	commit: &str, runs: usize, case_hex: &str, verdict: ReplayVerdict,
) -> Result<(), String> {
	let dir = commit_results_dir(commit, runs)?;
	fs::create_dir_all(&dir)
		.map_err(|err| format!("failed to create replay result dir {}: {err}", dir.display()))?;
	let path = replay_result_path(commit, runs, case_hex)?;
	let contents = format!("{}\n", verdict.as_str());
	fs::write(&path, contents)
		.map_err(|err| format!("failed to write replay result {}: {err}", path.display()))
}

struct CachedCommit {
	commit: String,
}

struct CaseResult {
	first_nonpass_commit: Option<String>,
	first_nonpass_verdict: Option<ReplayVerdict>,
	nonpassing_from_start: bool,
}

struct CaseSummary {
	case_name: String,
	case_hex: String,
	result: CaseResult,
}

fn bisect_cached(branch: &OsStr, since: &OsStr) -> Result<(), String> {
	let expected_commits = merge_commits_since(since, branch)?;
	cache_missing_binaries(&expected_commits)?;

	let cached = cached_commits_for_expected(&expected_commits)?;
	if cached.is_empty() {
		return Err("no cached commits found for the selected bisect window".to_string());
	}

	let cases = current_cases()?;
	if cases.is_empty() {
		return Err(format!("no test cases found in {CASE_DIR}"));
	}

	let mut summaries = Vec::new();
	let total_cases = cases.len();
	for (idx, case_path) in cases.iter().enumerate() {
		let case_name = case_path
			.file_name()
			.and_then(|name| name.to_str())
			.ok_or_else(|| format!("invalid testcase path {}", case_path.display()))?;
		println!("[{}/{}] bisecting {}", idx + 1, total_cases, case_name);
		let result = first_breaking_commit_for_case(case_name, case_path, &cached)?;
		summaries.push(CaseSummary {
			case_name: case_name.to_string(),
			case_hex: case_hex(&case_path)?,
			result,
		});
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
		for summary in matching_cases {
			let status_suffix = summary
				.result
				.first_nonpass_verdict
				.map(|verdict| format!(" [{}]", verdict.as_str()))
				.unwrap_or_default();
			println!("{}: {}{}", summary.case_name, summary.case_hex, status_suffix);
		}
	}

	if !printed_failure_section {
		println!("\nNo failing or flaky testcases across cached merge commits.");
	}
	Ok(())
}

fn first_breaking_commit_for_case(
	case_name: &str, case_path: &Path, cached: &[CachedCommit],
) -> Result<CaseResult, String> {
	let mut outcomes = HashMap::new();
	let mut replay_at = |idx: usize, runs: usize| -> Result<ReplayVerdict, String> {
		if let Some(result) = outcomes.get(&(idx, runs)) {
			return Ok(*result);
		}
		let outcome = replay_cached_commit(&cached[idx].commit, case_path, false, runs)?;
		let result = outcome.verdict;
		outcomes.insert((idx, runs), result);
		Ok(result)
	};

	let latest_idx = cached.len() - 1;
	let latest = replay_at(latest_idx, TIP_REPLAY_RUNS)?;
	println!(
		"  latest {} ({} runs): {}",
		short_commit(&cached[latest_idx].commit)?,
		TIP_REPLAY_RUNS,
		describe_case_outcome(latest)
	);
	if latest.is_pass() {
		println!("  {case_name}: not failing or flaky on latest cached merge commit");
		return Ok(CaseResult {
			first_nonpass_commit: None,
			first_nonpass_verdict: None,
			nonpassing_from_start: false,
		});
	}

	let bisect_runs =
		if latest == ReplayVerdict::Flake { FLAKY_REPLAY_RUNS } else { STABLE_REPLAY_RUNS };
	println!("  using {bisect_runs} run(s) for older commits");

	let oldest = replay_at(0, bisect_runs)?;
	println!(
		"  oldest {} ({} runs): {}",
		short_commit(&cached[0].commit)?,
		bisect_runs,
		describe_case_outcome(oldest)
	);
	if !oldest.is_pass() {
		println!("  {case_name}: already non-passing at oldest cached merge commit");
		return Ok(CaseResult {
			first_nonpass_commit: Some(cached[0].commit.clone()),
			first_nonpass_verdict: Some(oldest),
			nonpassing_from_start: true,
		});
	}

	let mut transition_idx = latest_idx;
	let mut probes = 0usize;
	for idx in (0..latest_idx).rev() {
		let outcome = replay_at(idx, bisect_runs)?;
		probes += 1;
		if probes == 1 || probes % 10 == 0 || outcome.is_pass() {
			println!(
				"  probe {probes} {} ({} runs): {}",
				short_commit(&cached[idx].commit)?,
				bisect_runs,
				describe_case_outcome(outcome)
			);
		}
		if outcome.is_pass() {
			break;
		}
		transition_idx = idx;
	}

	let nonpass = replay_at(transition_idx, bisect_runs)?;
	println!(
		"  {case_name}: newest pass -> non-pass transition at {} [{}]",
		short_commit(&cached[transition_idx].commit)?,
		nonpass.as_str()
	);

	Ok(CaseResult {
		first_nonpass_commit: Some(cached[transition_idx].commit.clone()),
		first_nonpass_verdict: Some(nonpass),
		nonpassing_from_start: false,
	})
}

fn describe_case_outcome(verdict: ReplayVerdict) -> &'static str {
	verdict.as_str()
}

fn cached_commits_sorted(branch: &OsStr) -> Result<Vec<CachedCommit>, String> {
	let root = commits_root()?;
	if !root.is_dir() {
		return Ok(Vec::new());
	}

	let mut cached_by_commit = HashMap::new();
	for entry in fs::read_dir(&root)
		.map_err(|err| format!("failed to read cache dir {}: {err}", root.display()))?
	{
		let entry = entry
			.map_err(|err| format!("failed to read cache entry in {}: {err}", root.display()))?;
		let path = entry.path();
		if !path.is_dir() {
			continue;
		}
		let Some(name) = path.file_name() else {
			continue;
		};
		let commit = name.to_string_lossy().to_string();
		if !path.join(TARGET_NAME).is_file() {
			continue;
		}
		cached_by_commit.insert(commit.clone(), CachedCommit { commit });
	}

	let mut commits = Vec::with_capacity(cached_by_commit.len());
	for commit in all_first_parent_commits_with_tip(branch)? {
		if let Some(entry) = cached_by_commit.remove(&commit) {
			commits.push(entry);
		}
	}
	Ok(commits)
}

fn cached_commits_for_expected(expected_commits: &[String]) -> Result<Vec<CachedCommit>, String> {
	let root = commits_root()?;
	if !root.is_dir() {
		return Ok(Vec::new());
	}

	let mut cached_by_commit = HashMap::new();
	for entry in fs::read_dir(&root)
		.map_err(|err| format!("failed to read cache dir {}: {err}", root.display()))?
	{
		let entry = entry
			.map_err(|err| format!("failed to read cache entry in {}: {err}", root.display()))?;
		let path = entry.path();
		if !path.is_dir() {
			continue;
		}
		let Some(name) = path.file_name() else {
			continue;
		};
		let commit = name.to_string_lossy().to_string();
		if !path.join(TARGET_NAME).is_file() {
			continue;
		}
		cached_by_commit.insert(commit.clone(), CachedCommit { commit });
	}

	let mut commits = Vec::with_capacity(expected_commits.len());
	for commit in expected_commits {
		if let Some(entry) = cached_by_commit.remove(commit) {
			commits.push(entry);
		}
	}
	Ok(commits)
}

fn missing_cached_binaries(expected_commits: &[String]) -> Result<Vec<String>, String> {
	let mut missing = Vec::new();
	for commit in expected_commits {
		if !commit_binary_path(commit)?.is_file() {
			missing.push(commit.clone());
		}
	}
	Ok(missing)
}

fn cache_missing_binaries(expected_commits: &[String]) -> Result<(), String> {
	let missing_commits = missing_cached_binaries(expected_commits)?;
	if missing_commits.is_empty() {
		return Ok(());
	}

	println!(
		"caching {} missing commit(s) required for bisect",
		missing_commits.len()
	);
	let total = missing_commits.len();
	for (idx, commit) in missing_commits.iter().enumerate() {
		cache_commit(OsStr::new(commit), Some((idx + 1, total)))?;
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
