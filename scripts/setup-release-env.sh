#!/usr/bin/env bash
# Configure the two GitHub environments release.yml uses, both deployable only from the main branch:
#   signing  holds the signing and notarization secrets for the build job (no reviewers)
#   release  gates the publish job (one required reviewer)
# Safe to re-run. Reviewer defaults to n-seiji (or $REVIEWER); the repository defaults to
# n-seiji/novakey-r (or $REPO).
# Usage: scripts/setup-release-env.sh [reviewer-login]
set -euo pipefail
reviewer="${1:-${REVIEWER:-n-seiji}}"
repo="${REPO:-n-seiji/novakey-r}"

id="$(gh api "users/$reviewer" -q .id)"
[ -n "$id" ] || { echo "setup-release-env: no GitHub user $reviewer" >&2; exit 1; }

# configure_env NAME REVIEWERS_JSON: create or update the environment, then leave exactly one
# deployment branch policy on it: main.
configure_env() {
  local name="$1" reviewers="$2"
  local env_path="repos/$repo/environments/$name"

  gh api -X PUT "$env_path" --input - >/dev/null <<JSON
{
  "reviewers": $reviewers,
  "prevent_self_review": false,
  "can_admins_bypass": false,
  "deployment_branch_policy": {"protected_branches": false, "custom_branch_policies": true}
}
JSON

  local policies have=0 pid pname ptype
  policies="$(gh api "$env_path/deployment-branch-policies" -q '.branch_policies[] | "\(.id)\t\(.name)\t\(.type)"')"
  while IFS=$'\t' read -r pid pname ptype; do
    [ -n "$pid" ] || continue
    if [ "$pname" = "main" ] && [ "$ptype" = "branch" ]; then
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
}

# show_env NAME: print the environment's reviewers and branch policies.
show_env() {
  local env_path="repos/$repo/environments/$1"
  echo "$1:"
  echo "  reviewers:"
  gh api "$env_path" -q '.protection_rules[] | select(.type == "required_reviewers") | .reviewers[] | "    \(.reviewer.login)"'
  echo "  deployment policies:"
  gh api "$env_path/deployment-branch-policies" -q '.branch_policies[] | "    \(.type) \(.name)"'
}

configure_env signing '[]'
configure_env release "[{\"type\": \"User\", \"id\": $id}]"
show_env signing
show_env release
