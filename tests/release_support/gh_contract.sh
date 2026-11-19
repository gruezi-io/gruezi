#!/bin/bash
set -eu
ctl="$(cd "$(dirname "$0")" && pwd -P)"
printf '%s\n' "$*" >> "$ctl/gh.log"

# Captured from gh 2.103.2's read-only `--json` field help.
run_fields="attempt conclusion createdAt databaseId displayTitle event headBranch headSha name number startedAt status updatedAt url workflowDatabaseId workflowName"
case "${1:-} ${2:-}" in
    "run list") available="$run_fields" ;;
    "run view") available="${run_fields/ name/ jobs name}" ;;
    "repo view")
        available="archivedAt assignableUsers codeOfConduct contactLinks createdAt defaultBranchRef deleteBranchOnMerge description diskUsage forkCount fundingLinks hasDiscussionsEnabled hasIssuesEnabled hasProjectsEnabled hasWikiEnabled homepageUrl id isArchived isBlankIssuesEnabled isEmpty isFork isInOrganization isMirror isPrivate isSecurityPolicyEnabled isTemplate isUserConfigurationRepository issueTemplates issues labels languages latestRelease licenseInfo mentionableUsers mergeCommitAllowed milestones mirrorUrl name nameWithOwner openGraphImageUrl owner parent primaryLanguage projects projectsV2 pullRequestTemplates pullRequests pushedAt rebaseMergeAllowed repositoryTopics securityPolicyUrl squashMergeAllowed sshUrl stargazerCount templateRepository updatedAt url usesCustomOpenGraphImage viewerCanAdminister viewerDefaultCommitEmail viewerDefaultMergeMethod viewerHasStarred viewerPermission viewerPossibleCommitEmails viewerSubscription visibility watchers"
        ;;
    *) available="" ;;
esac
json="" repo="" option=""
for arg in "$@"; do
    case "$option" in
        --json) json="$arg"; option=""; continue ;;
        -R|--repo) repo="$arg"; option=""; continue ;;
    esac
    case "$arg" in
        --json|-R|--repo) option="$arg" ;;
        --json=*) json="${arg#--json=}" ;;
        --repo=*) repo="${arg#--repo=}" ;;
        -R?*) repo="${arg#-R}" ;;
    esac
done
if [[ -n "$json" ]]; then
    IFS=, read -ra fields <<< "$json"
    for field in "${fields[@]}"; do
        if [[ " $available " != *" $field "* ]]; then
            printf 'Unknown JSON field: "%s"\nAvailable fields:\n' "$field" >&2
            for field in $available; do printf '  %s\n' "$field" >&2; done
            exit 1
        fi
    done
fi

# Tests can change the ambient repository independently of `gh repo view`.
ambient_repo=fixture/repo
if [[ -f "$ctl/ambient-repo" ]]; then ambient_repo="$(cat "$ctl/ambient-repo")"; fi
case "${1:-} ${2:-}" in
    "workflow run"|"run list"|"run view")
        [[ "${repo:-$ambient_repo}" == fixture/repo ]] || {
            echo "gh: repository ${repo:-$ambient_repo} has no fixture runs" >&2
            exit 1
        }
        ;;
esac
