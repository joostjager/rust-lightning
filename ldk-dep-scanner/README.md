# ldk-dep-scanner

Scan rust-lightning dependents to find projects using `no_std` mode.

## Purpose

This tool scans GitHub repositories that depend on rust-lightning to determine whether they use it in `std` or `no_std` mode. A project uses `no_std` when it specifies `default-features = false` for lightning crates without re-enabling the `std` feature.

## Getting dependents.json

First, install [gh-dependents](https://github.com/otiai10/gh-dependents):

```bash
go install github.com/otiai10/gh-dependents@latest
```

Then fetch the dependents list:

```bash
gh-dependents lightningdevkit/rust-lightning > dependents.json
```

This scrapes GitHub's dependency graph and outputs a JSON file with all repositories that depend on rust-lightning.

## Building

```bash
cd ldk-dep-scanner
cargo build --release
```

## Usage

```bash
# Basic usage (requires GITHUB_TOKEN for API rate limits)
GITHUB_TOKEN=$(gh auth token) cargo run -- -m 100

# Or with the release binary
GITHUB_TOKEN=$(gh auth token) ./target/release/ldk-dep-scanner -m 100
```

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `-i, --input` | Path to dependents.json | `dependents.json` |
| `-o, --output` | Output markdown file | `dependents_report.md` |
| `-m, --min-stars` | Minimum stars filter | `0` |
| `-t, --token` | GitHub token (or use `GITHUB_TOKEN` env) | - |
| `-c, --concurrency` | Number of concurrent requests | `5` |
| `-v, --verbose` | Show detailed scanning output | `false` |

### Example

```bash
# Scan repos with 50+ stars, verbose output
GITHUB_TOKEN=$(gh auth token) cargo run -- -m 50 -v

# Scan all repos (may take a while)
GITHUB_TOKEN=$(gh auth token) cargo run --
```

## Output

The tool generates a markdown file (`dependents_report.md`) containing a table of repositories using rust-lightning in `no_std` mode, sorted by stars:

| Repository | Stars | Last Updated | Cargo.toml |
|------------|-------|--------------|------------|
| user/repo | 123 | 2024-01-15 | [Cargo.toml](link) |

## How it works

1. Reads `dependents.json` containing repos that depend on rust-lightning
2. Filters out rust-lightning itself and obvious forks by name
3. For each repo, fetches metadata to check if it's a fork of rust-lightning (skips if so)
4. Fetches the repo's file tree to find all `Cargo.toml` files
5. Checks each `Cargo.toml` for lightning dependencies and their feature configuration
6. A repo is marked as `NO_STD` if any lightning dependency has `default-features = false` without `features = ["std"]`
