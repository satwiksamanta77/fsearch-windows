#!/usr/bin/env bash
# Publish FSearch for Windows. Run this on YOUR machine — it never sees a token
# in plaintext and never needs one: releases are published by the workflow
# using the per-run GITHUB_TOKEN that GitHub Actions issues itself.
#
#   ./publish.sh <owner>/<repo>
#
# Prereqs: git, and either `gh` (easiest) or an existing empty GitHub repo.
set -euo pipefail
REPO="${1:?usage: ./publish.sh <owner>/<repo>   e.g. ./publish.sh you/fsearch-windows}"
cd "$(dirname "$0")"

# 1. Re-author the commits as you. The repo carries no identity of its own, so
#    this uses your global git config; refuse rather than guess if it is unset.
NAME=$(git config user.name  || true)
MAIL=$(git config user.email || true)
if [ -z "$NAME" ] || [ -z "$MAIL" ]; then
  echo "!! git identity not set. Do this once, then re-run:"
  echo "     git config --global user.name  \"Your Name\""
  echo "     git config --global user.email \"you@example.com\""
  exit 1
fi
git rebase --root --exec 'git commit --amend --reset-author --no-edit -q' >/dev/null
echo "-> commits authored by $(git log -1 --format='%an <%ae>')"

# 2. Create the repo and push, or just push if it already exists.
if command -v gh >/dev/null 2>&1; then
  gh auth status >/dev/null 2>&1 || gh auth login --hostname github.com --git-protocol https --web
  gh repo create "$REPO" --public --source=. --remote=origin --push 2>/dev/null \
    || { git remote add origin "https://github.com/$REPO.git" 2>/dev/null || git remote set-url origin "https://github.com/$REPO.git"; git push -u origin main; }
else
  echo "!! 'gh' not found. Create an EMPTY repo at https://github.com/$REPO (no README),"
  echo "   then:  git remote add origin https://github.com/$REPO.git && git push -u origin main"
  exit 1
fi

# 3. Tag, which is what actually triggers the release build.
VER=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml)
git tag -f "v$VER"
git push origin "v$VER"

cat <<MSG

Done. Watch the build:
  https://github.com/$REPO/actions/workflows/release.yml

When it goes green the release is at:
  https://github.com/$REPO/releases/tag/v$VER
with fsearch-$VER-windows-x64.zip, both bare exes, and SHA256SUMS.txt.

Want artifacts without publishing a release? Run the 'release' workflow
manually from the Actions tab (workflow_dispatch) and download them from the run.
MSG
