//! Choosing a review's base: the repo's default branch and nearest-parent detection
//! (docs/design.md §9).

use crate::error::{Error, Result};
use crate::git::{self, BranchDistance, Git};
use crate::model::{BaseSource, short_ref};
use crate::text::quoted;

/// How the caller wants the base chosen (`base_mode` in `start_review`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaseMode {
    /// Whole-tree review on the default branch, otherwise the nearest parent branch.
    Auto,
    /// Whole-tree review.
    None,
    /// Diff review against this ref, as the user wrote it (e.g. `main` or `origin/main`).
    Ref(String),
}

/// The chosen base and how it was chosen, for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseChoice {
    /// Full ref name, or `None` for a whole-tree review.
    pub base_ref: Option<String>,
    pub source: BaseSource,
    /// A short explanation, e.g. "auto: nearest parent branch, 2 commits".
    pub reason: String,
    /// For auto-detected parents: the other candidates and their distances, nearest first.
    pub other_candidates: Vec<(String, u64)>,
}

/// Choose the base for a review of `head_ref` (a full ref name) whose tip is `head_sha`.
pub fn choose_base(
    git: &Git,
    head_ref: &str,
    head_sha: &str,
    mode: &BaseMode,
) -> Result<BaseChoice> {
    match mode {
        BaseMode::None => Ok(BaseChoice {
            base_ref: None,
            source: BaseSource::Explicit,
            reason: "explicit: whole-tree review".to_string(),
            other_candidates: Vec::new(),
        }),
        BaseMode::Ref(name) => {
            if git::is_worktree_relative(name) {
                return Err(Error::Invalid(format!(
                    "base {} is relative to a worktree's HEAD or reflog, which LANI would read from \
                     the repository's main worktree, not yours. Pass a branch name, such as \"main\".",
                    quoted(name)
                )));
            }
            let full = git.full_ref_name(name)?.ok_or_else(|| {
                Error::NotFound(format!(
                    "base {name:?} is not a branch or other ref in this repository"
                ))
            })?;
            if git::is_per_worktree_ref(&full) {
                return Err(Error::Invalid(format!(
                    "base {} is {}, a ref each worktree keeps its own copy of, so LANI would read \
                     the main worktree's. Pass a branch, tag or remote-tracking branch name.",
                    quoted(name),
                    quoted(&full)
                )));
            }
            if git.resolve_commit(&full)?.is_none() {
                return Err(Error::Invalid(format!(
                    "base {name:?} does not point to a commit"
                )));
            }
            if full == head_ref {
                return Err(Error::Invalid(format!(
                    "base and head are both {}; choose a different base, or base_mode \"none\" for a whole-tree review",
                    quoted(short_ref(head_ref))
                )));
            }
            Ok(BaseChoice {
                base_ref: Some(full),
                source: BaseSource::Explicit,
                reason: "explicit".to_string(),
                other_candidates: Vec::new(),
            })
        }
        BaseMode::Auto => {
            let default = default_branch(git)?;
            if default.as_deref() == Some(head_ref) {
                return Ok(BaseChoice {
                    base_ref: None,
                    source: BaseSource::Auto,
                    reason: format!(
                        "auto: {} is the default branch, so this is a whole-tree review",
                        quoted(short_ref(head_ref))
                    ),
                    other_candidates: Vec::new(),
                });
            }
            let distances = git.branch_distances(head_sha)?;
            nearest_parent(&distances, head_ref, default.as_deref())
        }
    }
}

/// The repo's default branch as a full ref name: the local branch named by
/// `refs/remotes/origin/HEAD`, else by `init.defaultBranch`, else `main`, else `master`.
/// Each candidate counts only if the local branch exists.
pub fn default_branch(git: &Git) -> Result<Option<String>> {
    let mut candidates = Vec::new();
    if let Some(target) = git.symbolic_ref("refs/remotes/origin/HEAD")?
        && let Some(name) = target.strip_prefix("refs/remotes/origin/")
    {
        candidates.push(name.to_string());
    }
    if let Some(name) = git.config_get("init.defaultBranch")? {
        candidates.push(name);
    }
    candidates.push("main".to_string());
    candidates.push("master".to_string());

    for name in candidates {
        let full = format!("refs/heads/{name}");
        if git.resolve_commit(&full)?.is_some() {
            return Ok(Some(full));
        }
    }
    Ok(None)
}

/// Pick the local branch whose fork point is closest to the head: the fewest head commits it
/// lacks ("behind"). Branches that already contain the whole head are not parents.
fn nearest_parent(
    distances: &[BranchDistance],
    head_ref: &str,
    default_branch: Option<&str>,
) -> Result<BaseChoice> {
    let mut candidates: Vec<&BranchDistance> = distances
        .iter()
        .filter(|distance| distance.refname != head_ref && distance.behind > 0)
        .collect();
    candidates.sort_by(|a, b| {
        a.behind
            .cmp(&b.behind)
            .then_with(|| a.refname.cmp(&b.refname))
    });

    let Some(nearest) = candidates.first() else {
        return Err(Error::Invalid(format!(
            "no parent branch found for {}: every other local branch already contains all of its commits; \
             use base_mode \"ref\" with an explicit base_ref, or base_mode \"none\" for a whole-tree review",
            quoted(short_ref(head_ref))
        )));
    };
    let tied: Vec<&BranchDistance> = candidates
        .iter()
        .copied()
        .filter(|candidate| candidate.behind == nearest.behind)
        .collect();

    let chosen = if tied.len() == 1 {
        nearest
    } else if let Some(default) = tied
        .iter()
        .find(|c| Some(c.refname.as_str()) == default_branch)
    {
        default
    } else {
        let names: Vec<String> = tied.iter().map(|c| quoted(short_ref(&c.refname))).collect();
        return Err(Error::Invalid(format!(
            "cannot choose a parent branch for {}: {} are equally close ({} commits); \
             use base_mode \"ref\" with one of them as base_ref",
            quoted(short_ref(head_ref)),
            names.join(", "),
            nearest.behind
        )));
    };

    let other_candidates = candidates
        .iter()
        .filter(|candidate| candidate.refname != chosen.refname)
        .take(3)
        .map(|candidate| (candidate.refname.clone(), candidate.behind))
        .collect();
    Ok(BaseChoice {
        base_ref: Some(chosen.refname.clone()),
        source: BaseSource::Auto,
        reason: format!(
            "auto: nearest parent branch, {} commit{}",
            chosen.behind,
            if chosen.behind == 1 { "" } else { "s" }
        ),
        other_candidates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn distance(name: &str, behind: u64) -> BranchDistance {
        BranchDistance {
            refname: format!("refs/heads/{name}"),
            ahead: 0,
            behind,
        }
    }

    const HEAD: &str = "refs/heads/feature/b";
    const MAIN: Option<&str> = Some("refs/heads/main");

    #[test]
    fn picks_the_closest_branch() {
        let distances = [
            distance("main", 6),
            distance("feature/a", 2),
            distance("feature/b", 0),
        ];
        let choice = nearest_parent(&distances, HEAD, MAIN).unwrap();
        assert_eq!(choice.base_ref.as_deref(), Some("refs/heads/feature/a"));
        assert_eq!(choice.reason, "auto: nearest parent branch, 2 commits");
        assert_eq!(
            choice.other_candidates,
            vec![("refs/heads/main".to_string(), 6)]
        );
    }

    #[test]
    fn skips_branches_containing_the_head() {
        let distances = [distance("main", 3), distance("feature/b-followup", 0)];
        let choice = nearest_parent(&distances, HEAD, MAIN).unwrap();
        assert_eq!(choice.base_ref.as_deref(), Some("refs/heads/main"));
    }

    #[test]
    fn tie_prefers_the_default_branch() {
        let distances = [distance("feature/a", 2), distance("main", 2)];
        let choice = nearest_parent(&distances, HEAD, MAIN).unwrap();
        assert_eq!(choice.base_ref.as_deref(), Some("refs/heads/main"));
    }

    #[test]
    fn tie_without_default_branch_is_rejected() {
        let distances = [
            distance("develop", 2),
            distance("release", 2),
            distance("main", 5),
        ];
        let error = nearest_parent(&distances, HEAD, MAIN)
            .unwrap_err()
            .to_string();
        assert!(error.contains(r#""develop", "release""#), "{error}");
    }

    #[test]
    fn no_candidates_is_rejected() {
        let error = nearest_parent(&[distance("feature/b", 0)], HEAD, MAIN).unwrap_err();
        assert!(error.to_string().contains("no parent branch"), "{error}");
    }
}
