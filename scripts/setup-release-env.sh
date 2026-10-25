#!/usr/bin/env bash
# Configure the `release` GitHub environment that gates the publish job in release.yml:
# one required reviewer, deployable only from the main branch. Safe to re-run. Reviewer defaults to
# n-seiji (or $REVIEWER); the repository defaults to n-seiji/novakey-r (or $REPO).
# Usage: scripts/setup-release-env.sh [reviewer-login]
set -euo pipefail
reviewer="${1:-${REVIEWER:-n-seiji}}"
repo="${REPO:-n-seiji/novakey-r}"
env_path="repos/$repo/environments/release"

id="$(gh api "users/$reviewer" -q .id)"
[ -n "$id" ] || { echo "setup-release-env: no GitHub user $reviewer" >&2; exit 1; }

gh api -X PUT "$env_path" --input - >/dev/null <<JSON
{
  "reviewers": [{"type": "User", "id": $id}],
  "prevent_self_review": false,
  "can_admins_bypass": false,
  "deployment_branch_policy": {"protected_branches": false, "custom_branch_policies": true}
}
JSON

policies="$(gh api "$env_path/deployment-branch-policies" -q '.branch_policies[] | "\(.id)\t\(.name)\t\(.type)"')"
have=0
while IFS=$'\t' read -r pid name type; do
  [ -n "$pid" ] || continue
  if [ "$name" = "main" ] && [ "$type" = "branch" ]; then
    have=1
  else
    gh api -X DELETE "$env_path/deployment-branch-policies/$pid" >/dev/null
  fi
done <<< "$policies"
if [ "$have" = 0 ]; then
  gh api -X POST "$env_path/deployment-branch-policies" --input - >/dev/null <<'JSON'
{"name": "main", "type": "branch"}
JSON
fi

echo "reviewers:"
gh api "$env_path" -q '.protection_rules[] | select(.type == "required_reviewers") | .reviewers[] | "  \(.reviewer.login)"'
echo "deployment policies:"
gh api "$env_path/deployment-branch-policies" -q '.branch_policies[] | "  \(.type) \(.name)"'
