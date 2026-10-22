#!/usr/bin/env bash
# Merge elliothux/open-compute main into centos8-compat.
# The workflow file documents the schedule, the token, and the conflict rules.
set -euo pipefail

export GIT_TERMINAL_PROMPT=0
export GIT_MERGE_AUTOEDIT=no

upstream_url="${SYNC_UPSTREAM_URL:-https://github.com/elliothux/open-compute.git}"
upstream_branch="${SYNC_UPSTREAM_BRANCH:-main}"
target_branch="${SYNC_TARGET_BRANCH:-centos8-compat}"
repo="${GITHUB_REPOSITORY:-}"
dry_run=false
if [ "${DRY_RUN:-false}" = "true" ]; then
  dry_run=true
fi

# SYNC_ALLOW_UPSTREAM_OVERRIDE=local-test is only for a local rehearsal.
# The workflow does not set it. Production fetches elliothux/open-compute main.
if [ "${SYNC_ALLOW_UPSTREAM_OVERRIDE:-}" != "local-test" ]; then
  case "$upstream_url" in
    https://github.com/elliothux/open-compute | https://github.com/elliothux/open-compute.git) ;;
    *)
      echo "Refusing to fetch ${upstream_url}."
      exit 1
      ;;
  esac
  if [ "$upstream_branch" != "main" ]; then
    echo "Refusing to merge ${upstream_branch}. The upstream branch must be main."
    exit 1
  fi
fi

if [ "$repo" = "elliothux/open-compute" ]; then
  echo "Refusing to sync. This workflow must run on the fork, not on elliothux/open-compute."
  exit 1
fi

issue_title="Upstream merge into centos8-compat has conflicts"

write_output() {
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    printf '%s=%s\n' "$1" "$2" >>"$GITHUB_OUTPUT"
  fi
}

write_summary() {
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    printf '%s\n' "$1" >>"$GITHUB_STEP_SUMMARY"
  fi
}

# Package inputs are the files that change the glibc 2.28 ocd binary or the
# CentOS 8 ocd build. A docs-only merge does not start that build.
is_package_input() {
  case "$1" in
    crates/* | packages/* | apps/* | scripts/* | share/* | third_party/* | test/glibc-compat/* | .github/actions/*)
      return 0
      ;;
    Cargo.toml | Cargo.lock | rust-toolchain.toml | package.json | bun.lock | docs/references/single-binary.md | .github/workflows/centos8-ocd.yml)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

restore_base() {
  git merge --abort >/dev/null 2>&1 || true
  git reset --hard "$base_sha" >/dev/null
}

require_clean_base() {
  if [ "$(git rev-parse HEAD)" != "$base_sha" ]; then
    echo "Failed to restore ${target_branch} to ${base_sha}."
    exit 1
  fi
  if [ -n "$(git status --porcelain --untracked-files=all)" ]; then
    echo "The worktree is still dirty after reset. ${target_branch} was not pushed."
    git status --porcelain --untracked-files=all
    exit 1
  fi
}

issue_number() {
  gh issue list --state open --limit 100 --json number,title \
    --jq "[.[] | select(.title == \"${issue_title}\") | .number][0] // empty"
}

close_conflict_issue() {
  if [ "$dry_run" = "true" ]; then
    return 0
  fi
  local number
  if ! number="$(issue_number)"; then
    echo "The merge succeeded. The conflict issue was not closed."
    return 0
  fi
  if [ -z "$number" ]; then
    return 0
  fi
  local body_file
  body_file="$(mktemp)"
  cat >"$body_file" <<EOF
A later sync contained upstream \`${upstream_branch}\` (\`${upstream_sha}\`) in \`${target_branch}\` (\`$(git rev-parse HEAD)\`).

This conflict is closed.
EOF
  if ! gh issue close "$number" --comment "$(cat "$body_file")"; then
    echo "The merge succeeded. Issue #${number} is still open."
  fi
  rm -f "$body_file"
}

report_conflict() {
  local conflict_files="$1"
  echo "Merge conflict. ${target_branch} was not pushed. The CentOS 8 ocd build was not started."
  if [ -n "$conflict_files" ]; then
    echo "Conflicting files:"
    printf '%s\n' "$conflict_files"
  else
    echo "Git reported a conflict and did not list files."
  fi
  write_output result conflict
  write_output build false
  write_summary "Merge conflict. \`${target_branch}\` was not pushed. The ocd build was not started."
  if [ "$dry_run" = "true" ]; then
    exit 1
  fi
  if [ -z "$repo" ]; then
    echo "GITHUB_REPOSITORY is empty. No issue was opened."
    exit 1
  fi
  local body_file number shown
  body_file="$(mktemp)"
  shown="$conflict_files"
  if [ "$(printf '%s\n' "$conflict_files" | wc -l)" -gt 100 ]; then
    shown="$(printf '%s\n' "$conflict_files" | head -n 100)"
    shown="${shown}"$'\n''(The list shows the first 100 files.)'
  fi
  cat >"$body_file" <<EOF
The sync workflow could not merge \`elliothux/open-compute\` \`${upstream_branch}\` (\`${upstream_sha}\`) into \`${target_branch}\` (\`${base_sha}\`).

\`${target_branch}\` was not pushed. The CentOS 8 ocd build was not started.

The workflow does not choose upstream or the fork.

Conflicting files:

\`\`\`
${shown}
\`\`\`

Resolve the conflicts on \`${target_branch}\` and push that merge. Then run **CentOS 8 ocd compatibility** when the merge changes a package input.

Workflow: \`.github/workflows/sync-upstream-centos8.yml\`
EOF
  number="$(issue_number || true)"
  if [ -n "$number" ]; then
    gh issue comment "$number" --body-file "$body_file" || echo "Could not comment on issue #${number}."
  else
    gh issue create --title "$issue_title" --body-file "$body_file" || echo "Could not open a conflict issue."
  fi
  rm -f "$body_file"
  exit 1
}

git config --local user.name "github-actions[bot]"
git config --local user.email "41898282+github-actions[bot]@users.noreply.github.com"

if git remote get-url upstream >/dev/null 2>&1; then
  git remote remove upstream
fi
git remote add upstream "$upstream_url"
git fetch --no-tags upstream "$upstream_branch"
upstream_sha="$(git rev-parse "FETCH_HEAD")"
git fetch --no-tags origin "$target_branch"
git checkout -B "$target_branch" "origin/${target_branch}"
base_sha="$(git rev-parse HEAD)"

echo "upstream ${upstream_branch}: ${upstream_sha}"
echo "${target_branch}: ${base_sha}"

if git merge-base --is-ancestor "$upstream_sha" HEAD; then
  echo "${target_branch} already contains upstream ${upstream_branch} (${upstream_sha}). No push. No ocd build."
  write_output result up-to-date
  write_output build false
  write_summary "\`${target_branch}\` already contains upstream \`${upstream_branch}\` \`${upstream_sha}\`. No push. No ocd build."
  close_conflict_issue
  exit 0
fi

merge_message="$(
  cat <<EOF
Merge upstream ${upstream_branch} into ${target_branch}

Upstream: https://github.com/elliothux/open-compute/commit/${upstream_sha}

[skip ci]
EOF
)"

# Do not pass -X ours or -X theirs. A conflict must stop the merge.
set +e
git merge --no-ff -m "$merge_message" "$upstream_sha"
merge_status=$?
set -e

if [ "$merge_status" -ne 0 ]; then
  if [ -f .git/MERGE_HEAD ]; then
    conflict_files="$(git diff --name-only --diff-filter=U || true)"
    restore_base
    require_clean_base
    report_conflict "$conflict_files"
  fi
  echo "git merge failed before it created a merge. ${target_branch} was not pushed. The ocd build was not started."
  restore_base
  require_clean_base
  write_output result merge-failed
  write_output build false
  exit 1
fi

if [ -n "$(git diff --name-only --diff-filter=U)" ] || [ -f .git/MERGE_HEAD ]; then
  echo "The merge still has unresolved files. ${target_branch} was not pushed."
  restore_base
  require_clean_base
  exit 1
fi

parent_count="$(git rev-list --parents -n 1 HEAD | awk '{print NF - 1}')"
if [ "$parent_count" -lt 2 ]; then
  echo "The merge did not create a merge commit. ${target_branch} was not pushed."
  restore_base
  require_clean_base
  exit 1
fi

if [ -n "$(git status --porcelain --untracked-files=all)" ]; then
  echo "The merge left the worktree dirty. ${target_branch} was not pushed."
  restore_base
  require_clean_base
  exit 1
fi

merge_sha="$(git rev-parse HEAD)"
build=false
package_changes=""
while IFS= read -r -d '' path; do
  if is_package_input "$path"; then
    build=true
    package_changes="${package_changes}${path}"$'\n'
  fi
done < <(git diff --name-only -z "$base_sha" "$merge_sha")

echo "Merge commit: ${merge_sha}"
if [ "$build" = "true" ]; then
  echo "Package inputs changed:"
  printf '%s' "$package_changes"
else
  echo "No package input changed. A pushed merge will not start the ocd build."
fi

if [ "$dry_run" = "true" ]; then
  echo "Dry run. ${merge_sha} was not pushed. The ocd build was not started."
  restore_base
  require_clean_base
  write_output result dry-run
  write_output build false
  write_summary "Dry run merged upstream \`${upstream_sha}\` and did not push."
  exit 0
fi

if [ "$target_branch" != "centos8-compat" ]; then
  echo "Refusing to push to ${target_branch}."
  restore_base
  exit 1
fi
if [ -z "$repo" ]; then
  echo "GITHUB_REPOSITORY is empty. The merge was not pushed."
  restore_base
  exit 1
fi
if [ "${GITHUB_ACTIONS:-}" != "true" ]; then
  echo "Refusing to push outside GitHub Actions. ${target_branch} was not pushed."
  restore_base
  exit 1
fi
if [ -z "${PUSH_TOKEN:-}" ]; then
  echo "PUSH_TOKEN is empty. The merge was not pushed."
  echo "Set the Actions secret UPSTREAM_SYNC_TOKEN when GITHUB_TOKEN cannot push."
  restore_base
  exit 1
fi

# actions/checkout stored PUSH_TOKEN for origin. Do not print the token.
# The commit message contains [skip ci], so this push does not start workflows.
# A GITHUB_TOKEN push also does not start push workflows. Start the ocd build
# below with workflow_dispatch when package inputs changed.
git push origin "HEAD:refs/heads/${target_branch}"

remote_sha=""
attempt=1
while [ "$attempt" -le 3 ]; do
  remote_sha="$(git ls-remote "https://github.com/${repo}.git" "refs/heads/${target_branch}" | awk 'NR==1 {print $1}')"
  if [ "$remote_sha" = "$merge_sha" ]; then
    break
  fi
  sleep 2
  attempt=$((attempt + 1))
done
if [ "$remote_sha" != "$merge_sha" ]; then
  echo "The push did not leave ${target_branch} at ${merge_sha}. The ocd build was not started."
  exit 1
fi

close_conflict_issue
write_output result merged
write_output merge_sha "$merge_sha"

if [ "$build" != "true" ]; then
  echo "Pushed ${merge_sha}. No package input changed. The ocd build was not started."
  write_output build false
  write_summary "Pushed \`${merge_sha}\`. No package input changed. The ocd build was not started."
  exit 0
fi

if ! gh workflow run centos8-ocd.yml --ref "$target_branch"; then
  echo "Pushed ${merge_sha}, but workflow_dispatch of centos8-ocd.yml failed. The ocd build was not started."
  write_output build false
  exit 1
fi

echo "Pushed ${merge_sha}. Started CentOS 8 ocd compatibility on ${target_branch}."
write_output build true
write_summary "Pushed \`${merge_sha}\`. Started CentOS 8 ocd compatibility on \`${target_branch}\`."
