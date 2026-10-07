//! Integration tests for the service layer against real temporary git repositories.

mod common;

use std::path::Path;

use common::{TestHome, TestRepo, numbered};
use lani::base::BaseMode;
use lani::error::Error;
use lani::model::{Actor, IssueStatus, ReviewStatus, Side};
use lani::service::{
    CommentTarget, CreateIssue, IssueView, Location, Service, StartReview, StartedReview,
};

fn service(home: &TestHome) -> Service {
    Service::open(&home.db_path()).expect("open service")
}

fn start(service: &mut Service, repo: &TestRepo, author: Actor, mode: BaseMode) -> StartedReview {
    service
        .start_review(&StartReview {
            repo_path: &repo.path,
            title: "Test review",
            author,
            base_mode: mode,
            summary: None,
            head_ref: None,
        })
        .expect("start review")
}

/// A create-issue request for `src/lib.rs` lines `start..=end` read from the worktree at HEAD.
fn issue_request<'a>(
    review_id: i64,
    repo: &'a TestRepo,
    commit: &'a str,
    start: u32,
    end: u32,
    start_text: &'a str,
) -> CreateIssue<'a> {
    CreateIssue {
        review_id,
        author: Actor::Reviewer,
        body: "Something is wrong here.\nMore detail.",
        path: "src/lib.rs",
        side: Side::Head,
        start_line: start,
        end_line: end,
        start_line_text: Some(start_text),
        commit,
        worktree_path: Some(&repo.path),
    }
}

/// main has src/lib.rs with 20 numbered lines; feature has one more commit touching another file.
fn feature_repo() -> TestRepo {
    let repo = TestRepo::new(&[("src/lib.rs", &numbered(20)), ("README.md", "readme\n")]);
    repo.switch_new("feature");
    repo.write("README.md", "readme\nmore\n");
    repo.commit_all("feature work");
    repo
}

fn expect_invalid<T: std::fmt::Debug>(result: lani::error::Result<T>, needle: &str) -> String {
    match result {
        Err(Error::Invalid(message)) => {
            assert!(
                message.contains(needle),
                "expected {needle:?} in: {message}"
            );
            message
        }
        other => panic!("expected Invalid containing {needle:?}, got {other:?}"),
    }
}

fn current(view: &IssueView) -> (String, u32, u32) {
    match &view.location {
        Location::Current {
            path,
            start_line,
            end_line,
            ..
        } => (path.clone(), *start_line, *end_line),
        Location::Outdated { reason } => {
            panic!("expected a current location, got outdated: {reason}")
        }
    }
}

fn is_outdated(view: &IssueView) -> bool {
    matches!(view.location, Location::Outdated { .. })
}

// ── base selection ───────────────────────────────────────────────────────────────────────────

#[test]
fn auto_base_picks_the_nearest_parent_of_a_stacked_branch() {
    let home = TestHome::new();
    let repo = TestRepo::new(&[("a.txt", "a\n")]);
    repo.switch_new("feature/a");
    repo.write("a.txt", "a\nb\n");
    repo.commit_all("a1");
    repo.switch_new("feature/b");
    repo.write("b.txt", "1\n");
    repo.commit_all("b1");
    repo.write("b.txt", "1\n2\n");
    repo.commit_all("b2");

    let mut service = service(&home);
    let started = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto);
    let choice = started.base_choice.expect("new review has a base choice");
    assert_eq!(choice.base_ref.as_deref(), Some("refs/heads/feature/a"));
    assert_eq!(choice.reason, "auto: nearest parent branch, 2 commits");
    assert_eq!(
        choice.other_candidates,
        vec![("refs/heads/main".to_string(), 3)]
    );
    assert_eq!(started.view.git.unwrap().commits_ahead, Some(2));
}

#[test]
fn auto_base_on_the_default_branch_is_a_whole_tree_review() {
    let home = TestHome::new();
    let repo = TestRepo::new(&[("a.txt", "a\n")]);
    let mut service = service(&home);
    let started = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto);
    assert_eq!(started.view.review.base_ref, None);
    assert!(
        started
            .base_choice
            .unwrap()
            .reason
            .contains("default branch")
    );
}

#[test]
fn auto_base_tie_prefers_the_default_branch() {
    let home = TestHome::new();
    let repo = TestRepo::new(&[("a.txt", "a\n")]);
    repo.git(&["branch", "other"]);
    repo.switch_new("feature");
    repo.write("a.txt", "changed\n");
    repo.commit_all("change");
    let mut service = service(&home);
    let started = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto);
    assert_eq!(
        started.view.review.base_ref.as_deref(),
        Some("refs/heads/main")
    );
}

#[test]
fn explicit_base_and_explicit_whole_tree() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let started = start(
        &mut service,
        &repo,
        Actor::Reviewer,
        BaseMode::Ref("main".into()),
    );
    assert_eq!(
        started.view.review.base_ref.as_deref(),
        Some("refs/heads/main")
    );
    service
        .close_review(Actor::Human, started.view.review.id)
        .unwrap();

    let started = start(&mut service, &repo, Actor::Reviewer, BaseMode::None);
    assert_eq!(started.view.review.base_ref, None);

    let review_of_main = service.start_review(&StartReview {
        repo_path: &repo.path,
        title: "t",
        author: Actor::Reviewer,
        base_mode: BaseMode::Ref("feature".into()),
        summary: None,
        head_ref: Some("main"),
    });
    assert!(
        review_of_main.is_ok(),
        "any base other than the head is fine: {review_of_main:?}"
    );
    let same = service.start_review(&StartReview {
        repo_path: &repo.path,
        title: "t",
        author: Actor::Reviewer,
        base_mode: BaseMode::Ref("feature".into()),
        summary: None,
        head_ref: Some("feature"),
    });
    assert!(
        matches!(same, Ok(StartedReview { created: false, .. })),
        "existing review returned"
    );
}

#[test]
fn one_open_review_per_branch_and_closed_reviews_allow_a_new_one() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let first = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto);
    assert!(first.created);
    let again = start(&mut service, &repo, Actor::Implementer, BaseMode::None);
    assert!(!again.created);
    assert_eq!(again.view.review.id, first.view.review.id);

    service
        .close_review(Actor::Reviewer, first.view.review.id)
        .unwrap();
    let second = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto);
    assert!(second.created);
    assert_ne!(second.view.review.id, first.view.review.id);
}

#[test]
fn detached_head_needs_an_explicit_branch() {
    let home = TestHome::new();
    let repo = feature_repo();
    repo.git(&["switch", "-q", "--detach", "HEAD"]);
    let mut service = service(&home);
    let result = service.start_review(&StartReview {
        repo_path: &repo.path,
        title: "t",
        author: Actor::Reviewer,
        base_mode: BaseMode::Auto,
        summary: None,
        head_ref: None,
    });
    expect_invalid(result, "HEAD is detached");
}

// ── anchor validation ────────────────────────────────────────────────────────────────────────

#[test]
fn create_issue_stores_anchor_snippet_and_root_comment() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let head = repo.head();

    let view = service
        .create_issue(&issue_request(review_id, &repo, &head, 5, 6, "  line 5 "))
        .unwrap();
    assert_eq!(current(&view), ("src/lib.rs".to_string(), 5, 6));
    assert_eq!(view.issue.anchor.commit, head);
    assert_eq!(view.issue.anchor.snippet_start, 2);
    assert_eq!(
        view.issue.anchor.snippet,
        "line 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9"
    );
    assert_eq!(view.comments.len(), 1);
    assert_eq!(view.comments[0].author, Actor::Reviewer);
    assert_eq!(view.comments[0].head_sha, head);
}

#[test]
fn create_issue_accepts_an_abbreviated_commit() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let short = repo.git(&["rev-parse", "--short", "HEAD"]);
    let view = service
        .create_issue(&issue_request(review_id, &repo, &short, 1, 1, "line 1"))
        .unwrap();
    assert_eq!(view.issue.anchor.commit, repo.head());
}

#[test]
fn create_issue_rejects_a_commit_not_on_the_branch() {
    let home = TestHome::new();
    let repo = feature_repo();
    repo.switch("main");
    repo.write("other.txt", "x\n");
    let main_only = repo.commit_all("main only");
    repo.switch("feature");
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let result = service.create_issue(&issue_request(review_id, &repo, &main_only, 1, 1, "line 1"));
    expect_invalid(result, "is not on branch feature");
}

#[test]
fn create_issue_rejects_wrong_start_line_text_and_lists_matches() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let head = repo.head();
    let message = expect_invalid(
        service.create_issue(&issue_request(review_id, &repo, &head, 5, 5, "line 7")),
        "start_line_text does not match line 5",
    );
    assert!(message.contains("Lines with that text: 7."), "{message}");
}

#[test]
fn create_issue_rejects_bad_ranges_and_paths() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let head = repo.head();
    expect_invalid(
        service.create_issue(&issue_request(review_id, &repo, &head, 19, 21, "line 19")),
        "has 20 lines",
    );
    expect_invalid(
        service.create_issue(&issue_request(review_id, &repo, &head, 6, 5, "line 6")),
        "invalid line range",
    );
    let mut request = issue_request(review_id, &repo, &head, 1, 1, "x");
    request.path = "../outside";
    expect_invalid(service.create_issue(&request), "invalid path");
    request.path = "src";
    expect_invalid(service.create_issue(&request), "is not a file");
    request.path = "missing.rs";
    assert!(matches!(
        service.create_issue(&request),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn uncommitted_change_inside_the_range_is_rejected() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let head = repo.head();
    repo.write(
        "src/lib.rs",
        &numbered(20).replace("line 6\n", "line six\n"),
    );
    expect_invalid(
        service.create_issue(&issue_request(review_id, &repo, &head, 5, 7, "line 5")),
        "lines 5-7 of src/lib.rs on disk",
    );
}

#[test]
fn staged_change_inside_the_range_is_rejected() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let head = repo.head();
    repo.write(
        "src/lib.rs",
        &numbered(20).replace("line 6\n", "line six\n"),
    );
    repo.git(&["add", "src/lib.rs"]);
    expect_invalid(
        service.create_issue(&issue_request(review_id, &repo, &head, 5, 7, "line 5")),
        "differ from commit",
    );
}

#[test]
fn uncommitted_lines_above_the_range_make_numbers_ambiguous() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let head = repo.head();
    // Three new lines after line 2 on disk: disk lines 10-11 are commit lines 7-8.
    repo.write(
        "src/lib.rs",
        &numbered(20).replace("line 2\n", "line 2\nnew a\nnew b\nnew c\n"),
    );
    let message = expect_invalid(
        service.create_issue(&issue_request(review_id, &repo, &head, 10, 11, "line 7")),
        "The differences are above lines 10-11",
    );
    assert!(message.contains("the same lines are 7-8 in"), "{message}");
    assert!(message.contains("worktree_path: null"), "{message}");
}

#[test]
fn uncommitted_changes_that_keep_numbering_are_accepted() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let head = repo.head();
    // A same-size edit above and an insertion below leave lines 10-11 where they were.
    let edited = numbered(20)
        .replace("line 2\n", "line two\n")
        .replace("line 15\n", "line 15\nadded\n");
    repo.write("src/lib.rs", &edited);
    let view = service
        .create_issue(&issue_request(review_id, &repo, &head, 10, 11, "line 10"))
        .unwrap();
    assert_eq!(current(&view), ("src/lib.rs".to_string(), 10, 11));
}

#[test]
fn null_worktree_path_skips_the_disk_check() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let head = repo.head();
    repo.write("src/lib.rs", "completely different\n");
    let mut request = issue_request(review_id, &repo, &head, 5, 5, "line 5");
    request.worktree_path = None;
    assert!(service.create_issue(&request).is_ok());
}

#[test]
fn file_missing_on_disk_is_rejected() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let head = repo.head();
    std::fs::remove_file(repo.path.join("src/lib.rs")).unwrap();
    expect_invalid(
        service.create_issue(&issue_request(review_id, &repo, &head, 5, 5, "line 5")),
        "does not exist on disk",
    );
}

#[test]
fn head_that_moved_since_the_read_is_detected_through_the_disk_check() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let read_at = repo.head();
    // The implementer commits lines at the top after the reviewer read the file.
    repo.write("src/lib.rs", &format!("header\n{}", numbered(20)));
    repo.commit_all("add header");
    expect_invalid(
        service.create_issue(&issue_request(review_id, &repo, &read_at, 5, 5, "line 5")),
        "The differences are above lines 5-5",
    );
}

#[test]
fn base_side_needs_a_diff_review_and_a_null_worktree() {
    let home = TestHome::new();
    let repo = TestRepo::new(&[("src/lib.rs", &numbered(20))]);
    repo.switch_new("feature");
    repo.write("src/lib.rs", &numbered(20).replace("line 4\n", ""));
    repo.commit_all("delete line 4");
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let head = repo.head();

    let mut request = issue_request(review_id, &repo, &head, 4, 4, "line 4");
    request.side = Side::Base;
    expect_invalid(service.create_issue(&request), "worktree_path must be null");
    request.worktree_path = None;
    let view = service.create_issue(&request).unwrap();
    assert_eq!(view.issue.anchor.commit, repo.git(&["rev-parse", "main"]));
    assert_eq!(current(&view), ("src/lib.rs".to_string(), 4, 4));

    service.close_review(Actor::Human, review_id).unwrap();
    let whole_tree = start(&mut service, &repo, Actor::Reviewer, BaseMode::None)
        .view
        .review
        .id;
    let mut request = issue_request(whole_tree, &repo, &head, 4, 4, "line 5");
    request.side = Side::Base;
    request.worktree_path = None;
    expect_invalid(service.create_issue(&request), "whole-tree review");
}

#[test]
fn binary_files_cannot_be_anchored() {
    let home = TestHome::new();
    let repo = TestRepo::new(&[("a.txt", "a\n")]);
    std::fs::write(repo.path.join("image.bin"), b"\x89PNG\0\0\x01").unwrap();
    repo.commit_all("binary");
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::None)
        .view
        .review
        .id;
    let head = repo.head();
    let mut request = issue_request(review_id, &repo, &head, 1, 1, "x");
    request.path = "image.bin";
    expect_invalid(service.create_issue(&request), "binary file");
}

// ── relocation ───────────────────────────────────────────────────────────────────────────────

fn repo_with_issue(start_line: u32, end_line: u32) -> (TestHome, TestRepo, Service, i64) {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let head = repo.head();
    let text = format!("line {start_line}");
    let issue_id = service
        .create_issue(&issue_request(
            review_id, &repo, &head, start_line, end_line, &text,
        ))
        .unwrap()
        .issue
        .id;
    (home, repo, service, issue_id)
}

#[test]
fn issue_follows_lines_when_code_is_inserted_above() {
    let (_home, repo, service, issue_id) = repo_with_issue(10, 11);
    repo.write("src/lib.rs", &format!("a\nb\nc\n{}", numbered(20)));
    let new_head = repo.commit_all("insert above");
    let view = service.get_issue(issue_id).unwrap();
    assert_eq!(current(&view), ("src/lib.rs".to_string(), 13, 14));
    let code = view.code.unwrap();
    assert_eq!(code.commit, new_head);
    assert_eq!(code.first_line, 10);
}

#[test]
fn issue_is_outdated_when_its_lines_change() {
    let (_home, repo, service, issue_id) = repo_with_issue(10, 11);
    repo.write("src/lib.rs", &numbered(20).replace("line 11\n", "fixed\n"));
    repo.commit_all("fix");
    let view = service.get_issue(issue_id).unwrap();
    assert!(is_outdated(&view));
    // The stored snippet is shown instead of current code.
    assert_eq!(view.code.unwrap().first_line, 7);
}

#[test]
fn issue_follows_a_rename_and_is_outdated_when_the_file_is_deleted() {
    let (_home, repo, service, issue_id) = repo_with_issue(10, 11);
    repo.git(&["mv", "src/lib.rs", "src/core.rs"]);
    repo.commit_all("rename");
    let view = service.get_issue(issue_id).unwrap();
    assert_eq!(current(&view), ("src/core.rs".to_string(), 10, 11));

    repo.git(&["rm", "-q", "src/core.rs"]);
    repo.commit_all("delete");
    let view = service.get_issue(issue_id).unwrap();
    assert!(is_outdated(&view));
}

// ── permissions, lifecycle and concurrency ───────────────────────────────────────────────────

#[test]
fn only_the_reviewer_and_human_change_issue_status() {
    let (_home, _repo, mut service, issue_id) = repo_with_issue(3, 3);
    assert!(matches!(
        service.set_issue_status(Actor::Implementer, issue_id, IssueStatus::Resolved, None),
        Err(Error::Forbidden(_))
    ));
    let view = service
        .set_issue_status(
            Actor::Reviewer,
            issue_id,
            IssueStatus::Resolved,
            Some("Fixed, thanks."),
        )
        .unwrap();
    assert_eq!(view.issue.status, IssueStatus::Resolved);
    assert_eq!(view.issue.resolved_by, Some(Actor::Reviewer));
    assert_eq!(view.comments.last().unwrap().body, "Fixed, thanks.");

    expect_invalid(
        service.set_issue_status(Actor::Reviewer, issue_id, IssueStatus::Resolved, None),
        "already resolved",
    );
    expect_invalid(
        service.set_issue_status(Actor::Reviewer, issue_id, IssueStatus::Open, None),
        "needs a comment",
    );
    let view = service
        .set_issue_status(Actor::Human, issue_id, IssueStatus::Open, None)
        .unwrap();
    assert_eq!(view.issue.status, IssueStatus::Open);
    assert_eq!(view.issue.resolved_by, None);
}

#[test]
fn only_the_reviewer_and_human_close_reviews_and_closed_reviews_are_read_only() {
    let (_home, _repo, mut service, issue_id) = repo_with_issue(3, 3);
    let review_id = service.get_issue(issue_id).unwrap().issue.review_id;
    assert!(matches!(
        service.close_review(Actor::Implementer, review_id),
        Err(Error::Forbidden(_))
    ));
    let view = service.close_review(Actor::Reviewer, review_id).unwrap();
    assert_eq!(view.review.status, ReviewStatus::Closed);
    expect_invalid(
        service.add_comment(Actor::Implementer, CommentTarget::Issue(issue_id), "late"),
        "is closed",
    );
}

#[test]
fn comment_edits_respect_roles_and_versions() {
    let (_home, _repo, mut service, issue_id) = repo_with_issue(3, 3);
    let comment = service
        .add_comment(
            Actor::Implementer,
            CommentTarget::Issue(issue_id),
            "Fixed in abc.",
        )
        .unwrap();
    assert_eq!(comment.version, 1);

    assert!(matches!(
        service.edit_comment(Actor::Reviewer, comment.id, "hijack", 1),
        Err(Error::Forbidden(_))
    ));
    let edited = service
        .edit_comment(Actor::Human, comment.id, "Fixed in abc (edited).", 1)
        .unwrap();
    assert_eq!(edited.version, 2);
    assert_eq!(edited.edited_by, Some(Actor::Human));
    assert_eq!(edited.author, Actor::Implementer);

    match service.edit_comment(Actor::Implementer, comment.id, "stale edit", 1) {
        Err(Error::Conflict(message)) => {
            assert!(message.contains("Fixed in abc (edited)."), "{message}");
            assert!(message.contains("expected_version 2"), "{message}");
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    let edited = service
        .edit_comment(Actor::Implementer, comment.id, "Fixed in abc and def.", 2)
        .unwrap();
    assert_eq!(edited.version, 3);
    assert_eq!(edited.edited_by, Some(Actor::Implementer));
}

#[test]
fn summary_updates_use_versions_and_summary_comments_are_separate() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Implementer, BaseMode::Auto)
        .view
        .review
        .id;

    let detail = service.get_review(review_id, Some(&repo.path)).unwrap();
    assert_eq!(detail.view.review.summary, "");
    assert_eq!(detail.view.review.summary_version, 1);
    assert!(detail.worktree.unwrap().matches_review_head);

    let detail = service
        .update_summary(review_id, Actor::Implementer, "Adds X so that Y.", 1)
        .unwrap();
    assert_eq!(detail.view.review.summary_version, 2);
    assert_eq!(
        detail.view.review.summary_updated_by,
        Some(Actor::Implementer)
    );
    assert!(matches!(
        service.update_summary(review_id, Actor::Reviewer, "Overwrite", 1),
        Err(Error::Conflict(_))
    ));

    service
        .add_comment(
            Actor::Reviewer,
            CommentTarget::Summary(review_id),
            "Looks reasonable.",
        )
        .unwrap();
    let detail = service.get_review(review_id, None).unwrap();
    assert_eq!(detail.summary_comments.len(), 1);
    assert_eq!(detail.view.counts.open, 0);
}

#[test]
fn list_issues_filters_by_status_and_path() {
    let (_home, repo, mut service, first) = repo_with_issue(3, 3);
    let review_id = service.get_issue(first).unwrap().issue.review_id;
    let head = repo.head();
    let mut request = issue_request(review_id, &repo, &head, 1, 1, "readme");
    request.path = "README.md";
    let second = service.create_issue(&request).unwrap().issue.id;
    service
        .set_issue_status(Actor::Reviewer, first, IssueStatus::Resolved, None)
        .unwrap();

    let open = service
        .list_issues(review_id, Some(IssueStatus::Open), None, false)
        .unwrap();
    assert_eq!(
        open.issues.iter().map(|i| i.issue.id).collect::<Vec<_>>(),
        vec![second]
    );
    assert!(open.issues[0].code.is_none());
    let all = service.list_issues(review_id, None, None, true).unwrap();
    assert_eq!(all.issues.len(), 2);
    assert!(all.issues.iter().all(|i| i.code.is_some()));
    let in_lib = service
        .list_issues(review_id, None, Some("src/lib.rs"), false)
        .unwrap();
    assert_eq!(
        in_lib.issues.iter().map(|i| i.issue.id).collect::<Vec<_>>(),
        vec![first]
    );
}

#[test]
fn list_reviews_filters_by_repo_and_marks_the_checked_out_branch() {
    let home = TestHome::new();
    let repo = feature_repo();
    let other = TestRepo::new(&[("x.txt", "x\n")]);
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    start(&mut service, &other, Actor::Reviewer, BaseMode::Auto);

    let all = service
        .list_reviews(Some(ReviewStatus::Open), None)
        .unwrap();
    assert_eq!(all.items.len(), 2);
    let mine = service
        .list_reviews(Some(ReviewStatus::Open), Some(&repo.path))
        .unwrap();
    assert_eq!(mine.items.len(), 1);
    assert_eq!(mine.items[0].view.review.id, review_id);
    assert!(mine.items[0].checked_out);
    let closed = service
        .list_reviews(Some(ReviewStatus::Closed), Some(&repo.path))
        .unwrap();
    assert!(closed.items.is_empty());
}

#[test]
fn worktrees_of_one_repo_share_reviews() {
    let home = TestHome::new();
    let repo = feature_repo();
    repo.switch("main");
    let linked_parent = tempfile::tempdir().unwrap();
    let linked = std::fs::canonicalize(linked_parent.path())
        .unwrap()
        .join("wt");
    repo.git(&["worktree", "add", "-q", linked.to_str().unwrap(), "feature"]);
    let mut service = service(&home);
    let started = service
        .start_review(&StartReview {
            repo_path: &linked,
            title: "From the linked worktree",
            author: Actor::Reviewer,
            base_mode: BaseMode::Auto,
            summary: Some("Intent."),
            head_ref: None,
        })
        .unwrap();
    assert_eq!(started.view.review.head_ref, "refs/heads/feature");
    let from_main = service.list_reviews(None, Some(&repo.path)).unwrap();
    assert_eq!(from_main.items.len(), 1);
    assert!(
        !from_main.items[0].checked_out,
        "main worktree has main checked out"
    );
}

// ── storage guarantees ───────────────────────────────────────────────────────────────────────

#[test]
fn git_calls_never_rewrite_the_index() {
    let home = TestHome::new();
    let repo = feature_repo();
    // Rewriting a file with identical content leaves the index's stat data stale, which makes a
    // plain `git status` refresh and rewrite the index.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    repo.write("src/lib.rs", &numbered(20));
    let before = repo.index_bytes();

    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let head = repo.head();
    service
        .create_issue(&issue_request(review_id, &repo, &head, 5, 5, "line 5"))
        .unwrap();
    service.get_review(review_id, Some(&repo.path)).unwrap();
    service.list_issues(review_id, None, None, true).unwrap();
    assert_eq!(
        repo.index_bytes(),
        before,
        "LANI must not modify the repository"
    );

    // Control: a plain `git status` does rewrite the index here, so the check above is meaningful.
    repo.git(&["status", "--porcelain"]);
    assert_ne!(
        repo.index_bytes(),
        before,
        "test setup should leave the index stale"
    );
}

#[test]
fn comments_cannot_point_at_another_reviews_issue() {
    let (home, repo, service, issue_id) = repo_with_issue(3, 3);
    drop(service);
    repo.switch("main");
    repo.switch_new("second");
    repo.write("z.txt", "z\n");
    repo.commit_all("second");
    let mut service = Service::open(&home.db_path()).unwrap();
    let other_review = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    drop(service);

    let conn = lani::store::open(&home.db_path()).unwrap();
    let result = conn.execute(
        "INSERT INTO comment (review_id, issue_id, author, body, head_sha, created_at, edited_by,
                              edited_at, version)
         VALUES (?1, ?2, 'human', 'x', 'abc', '2026-01-01T00:00:00.000Z', NULL, NULL, 1)",
        rusqlite::params![other_review, issue_id],
    );
    assert!(
        result.is_err(),
        "composite foreign key must reject the mismatched review"
    );
}

#[test]
fn the_database_is_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let home = TestHome::new();
    let _service = service(&home);
    let mode = std::fs::metadata(home.db_path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn reopening_the_database_keeps_the_schema() {
    let home = TestHome::new();
    drop(service(&home));
    drop(service(&home));
    let conn = lani::store::open(Path::new(&home.db_path())).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 1);
    let timeout: String = conn
        .query_row(
            "SELECT value FROM setting WHERE key = 'idle_timeout'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(timeout, "3h");
}

// ── human-only operations (M2) ───────────────────────────────────────────────────────────────

#[test]
fn only_the_human_renames_rebases_reopens_and_changes_settings() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    for claude in [Actor::Reviewer, Actor::Implementer] {
        assert!(matches!(
            service.retitle_review(claude, review_id, "x"),
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(
            service.change_base(claude, review_id, &BaseMode::None),
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(
            service.reopen_review(claude, review_id),
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(
            service.set_idle_timeout(claude, lani::settings::IdleTimeout::Never),
            Err(Error::Forbidden(_))
        ));
    }

    let view = service
        .retitle_review(Actor::Human, review_id, "  Better title ")
        .unwrap();
    assert_eq!(view.review.title, "Better title");
    let view = service
        .change_base(Actor::Human, review_id, &BaseMode::None)
        .unwrap();
    assert_eq!(view.review.base_ref, None);
    assert_eq!(view.review.base_source, lani::model::BaseSource::WebUi);
    let view = service
        .change_base(Actor::Human, review_id, &BaseMode::Ref("main".into()))
        .unwrap();
    assert_eq!(view.review.base_ref.as_deref(), Some("refs/heads/main"));
    expect_invalid(
        service.change_base(Actor::Human, review_id, &BaseMode::Ref("feature".into())),
        "base and head are both feature",
    );
    let candidates = service.base_candidates(review_id).unwrap();
    assert_eq!(
        candidates,
        vec!["refs/heads/main".to_string()],
        "the head is not a candidate"
    );

    let kinds: Vec<_> = service
        .list_events(review_id)
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            lani::model::EventKind::ReviewBaseChanged,
            lani::model::EventKind::ReviewBaseChanged,
            lani::model::EventKind::ReviewRetitled,
            lani::model::EventKind::ReviewCreated,
        ],
        "newest first"
    );
}

#[test]
fn reopening_is_refused_while_the_branch_has_another_open_review() {
    let home = TestHome::new();
    let repo = feature_repo();
    let mut service = service(&home);
    let first = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    service.close_review(Actor::Human, first).unwrap();
    let second = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    let message = expect_invalid(
        service.reopen_review(Actor::Human, first),
        "already has an open review",
    );
    assert!(message.contains(&format!("(review {second})")), "{message}");
    service.close_review(Actor::Human, second).unwrap();
    let view = service.reopen_review(Actor::Human, first).unwrap();
    assert_eq!(view.review.status, ReviewStatus::Open);
}

#[test]
fn line_endings_are_normalized_and_unchanged_saves_are_not_edits() {
    let (_home, _repo, mut service, issue_id) = repo_with_issue(3, 3);
    let review_id = service.get_issue(issue_id).unwrap().issue.review_id;

    let comment = service
        .add_comment(
            Actor::Reviewer,
            CommentTarget::Issue(issue_id),
            "one\r\ntwo\rthree",
        )
        .unwrap();
    assert_eq!(comment.body, "one\ntwo\nthree");

    // A browser re-submits unchanged text with CRLF line endings: nothing changes.
    let saved = service
        .edit_comment(Actor::Human, comment.id, "one\r\ntwo\r\nthree", 1)
        .unwrap();
    assert_eq!(saved.version, 1);
    assert_eq!(saved.edited_by, None, "not marked as edited");

    let detail = service
        .update_summary(review_id, Actor::Human, "Intent:\r\n- a\r\n", 1)
        .unwrap();
    assert_eq!(detail.view.review.summary, "Intent:\n- a\n");
    assert_eq!(detail.view.review.summary_version, 2);
    let detail = service
        .update_summary(review_id, Actor::Human, "Intent:\r\n- a\r\n", 2)
        .unwrap();
    assert_eq!(
        detail.view.review.summary_version, 2,
        "unchanged summary is not a new version"
    );
    let kinds: Vec<_> = service
        .list_events(review_id)
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == lani::model::EventKind::SummaryUpdated)
            .count(),
        1
    );
    assert!(!kinds.contains(&lani::model::EventKind::CommentEdited));

    expect_invalid(
        service.retitle_review(Actor::Human, review_id, "two\rlines"),
        "single line",
    );
}

#[test]
fn a_moved_repository_degrades_to_stored_data() {
    let (_home, repo, mut service, issue_id) = repo_with_issue(10, 11);
    let review_id = service.get_issue(issue_id).unwrap().issue.review_id;
    let elsewhere = tempfile::tempdir().unwrap();
    let moved = elsewhere.path().join("moved");
    std::fs::rename(&repo.path, &moved).unwrap();

    let detail = service.get_review(review_id, None).unwrap();
    assert!(detail.view.git.is_none());
    let reason = detail.view.repo_unavailable.clone().unwrap();
    assert!(reason.contains("no longer at"), "{reason}");

    let list = service.list_issues(review_id, None, None, true).unwrap();
    match &list.issues[0].location {
        Location::Outdated { reason } => assert!(reason.contains("no longer at"), "{reason}"),
        Location::Current { .. } => panic!("cannot be current without the repository"),
    }
    assert_eq!(
        list.issues[0].code.as_ref().unwrap().first_line,
        7,
        "stored snippet"
    );
    assert!(service.get_issue(issue_id).is_ok());

    // Operations that need the repository fail clearly; the rest still work.
    expect_invalid(
        service.add_comment(Actor::Human, CommentTarget::Issue(issue_id), "x"),
        "no longer at",
    );
    expect_invalid(
        service.set_issue_status(Actor::Human, issue_id, IssueStatus::Resolved, Some("done")),
        "no longer at",
    );
    expect_invalid(service.base_candidates(review_id), "no longer at");
    let view = service
        .set_issue_status(Actor::Reviewer, issue_id, IssueStatus::Resolved, None)
        .unwrap();
    assert_eq!(view.issue.status, IssueStatus::Resolved);
    service.close_review(Actor::Human, review_id).unwrap();
    let view = service.reopen_review(Actor::Human, review_id).unwrap();
    assert!(view.repo_unavailable.is_some());

    std::fs::rename(&moved, &repo.path).unwrap();
    assert!(
        service
            .get_review(review_id, None)
            .unwrap()
            .view
            .git
            .is_some()
    );
}

#[test]
fn issues_sharing_an_anchor_commit_are_placed_from_one_tree_diff() {
    let home = TestHome::new();
    let repo = TestRepo::new(&[
        ("a.rs", &numbered(10)),
        ("b.rs", &numbered(10)),
        ("c.rs", &numbered(10)),
    ]);
    repo.switch_new("feature");
    repo.write("a.rs", &numbered(11));
    let anchor = repo.commit_all("touch a");
    let mut service = service(&home);
    let review_id = start(&mut service, &repo, Actor::Reviewer, BaseMode::Auto)
        .view
        .review
        .id;
    for path in ["a.rs", "b.rs", "c.rs"] {
        let mut request = issue_request(review_id, &repo, &anchor, 5, 5, "line 5");
        request.path = path;
        service.create_issue(&request).unwrap();
    }
    // One later commit renames b.rs, inserts a line at the top of a.rs and leaves c.rs alone.
    repo.git(&["mv", "b.rs", "renamed.rs"]);
    repo.write("a.rs", &format!("top\n{}", numbered(11)));
    repo.commit_all("rename b, insert into a");

    let list = service.list_issues(review_id, None, None, false).unwrap();
    let mut placed: Vec<(String, u32, u32)> = list.issues.iter().map(current).collect();
    placed.sort();
    assert_eq!(
        placed,
        vec![
            ("a.rs".to_string(), 6, 6),
            ("c.rs".to_string(), 5, 5),
            ("renamed.rs".to_string(), 5, 5),
        ]
    );
}
