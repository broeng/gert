//! `load_diff`'s order (store, checkout, Gerrit) and the local source's
//! rules, against a real temporary repository.

use gerrit_api::fake::RouteHttpClient;
use gpui_kit::TestAppContext;
use local_git::testutil::TestRepo;
use model::{
    ChangeDetail, ChangeNumber, DiffKey, FileEntry, FileStatus, FilesKey, PatchSetNumber,
    PatchSetRef, Whitespace,
};
use store::Store;

use super::LocalSource;
use crate::{
    diff_load::{DiffSource, LoadError, load_diff},
    session::Session,
    testing::{detail, file},
    tests::{URL, env_with, setup},
};

/// A repository with two commits: `src/a.rs` changes in the second.
pub(crate) struct Fixture {
    pub repo: TestRepo,
    pub base: String,
    pub target: String,
}

pub(crate) const A_OLD: &str = "l1\nl2\nl3\n";
pub(crate) const A_NEW: &str = "l1\nCHANGED\nl3\n";

pub(crate) fn fixture() -> Fixture {
    let repo = TestRepo::new();
    repo.write("src/a.rs", A_OLD.as_bytes());
    repo.write("src/b.rs", b"same\n");
    let base = repo.commit("first");
    repo.write("src/a.rs", A_NEW.as_bytes());
    let target = repo.commit("second");
    Fixture { repo, base, target }
}

/// Change 7 with one patch set, whose commit is `sha`.
pub(crate) fn detail_at(sha: &str) -> ChangeDetail {
    let mut d = detail(7, 1, 1);
    d.revisions[0].sha = sha.into();
    d.current_revision = Some(sha.into());
    d
}

pub(crate) fn files_key(sha: &str) -> FilesKey {
    FilesKey {
        change: ChangeNumber(7),
        base: None,
        target: PatchSetRef {
            number: PatchSetNumber(1),
            sha: sha.into(),
        },
    }
}

pub(crate) fn paths() -> Vec<FileEntry> {
    ["/COMMIT_MSG", "src/a.rs", "src/b.rs"]
        .iter()
        .map(|p| file(p, FileStatus::Modified))
        .collect()
}

fn diff_key(sha: &str, path: &str) -> DiffKey {
    DiffKey {
        change: ChangeNumber(7),
        base: None,
        target: files_key(sha).target,
        path: path.into(),
        whitespace: Whitespace::IgnoreNone,
    }
}

fn source(f: &Fixture) -> LocalSource {
    let repo = local_git::Repo::open(f.repo.path()).expect("open");
    LocalSource::resolve(repo, &files_key(&f.target))
        .expect("resolves")
        .with_files(&paths())
}

fn fresh_store() -> Store {
    let mut store = Store::open_in_memory().expect("store");
    store.server_id(URL).expect("server");
    store
}

fn session(store: Store, http: &RouteHttpClient, signed_in: bool) -> Session {
    let env = env_with(store, http);
    let auth = signed_in.then_some(gerrit_api::Auth::Anonymous);
    Session::new(&env, &setup(), auth).expect("session")
}

fn gerrit_diff() -> RouteHttpClient {
    let http = RouteHttpClient::new();
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/handwritten/diff_rename.response.json");
    http.route("/diff", 200, std::fs::read(path).expect("fixture"));
    http
}

fn run<T>(f: impl std::future::Future<Output = T>) -> T {
    futures::executor::block_on(f)
}

#[gpui_kit::test]
fn a_local_diff_needs_no_request_and_is_not_stored(_cx: &mut TestAppContext) {
    let f = fixture();
    let http = RouteHttpClient::new();
    let s = session(fresh_store(), &http, true);
    let key = diff_key(&f.target, "src/a.rs");
    let (diff, from) = run(load_diff(
        s.syncer().clone(),
        key.clone(),
        true,
        Some(source(&f)),
    ))
    .expect("diff");
    assert_eq!(from, DiffSource::Local);
    assert!(format!("{:?}", diff.chunks).contains("CHANGED"));
    assert!(http.requested().is_empty(), "{:?}", http.requested());
    assert!(s.syncer().cached_diff(&key).expect("read").is_none());
}

#[gpui_kit::test]
fn a_local_diff_shows_offline(_cx: &mut TestAppContext) {
    let f = fixture();
    let http = RouteHttpClient::new();
    let s = session(fresh_store(), &http, false);
    let key = diff_key(&f.target, "src/a.rs");
    let (_, from) = run(load_diff(s.syncer().clone(), key, false, Some(source(&f)))).expect("diff");
    assert_eq!(from, DiffSource::Local);
}

#[gpui_kit::test]
fn a_cached_diff_wins_over_the_checkout(_cx: &mut TestAppContext) {
    let f = fixture();
    let http = RouteHttpClient::new();
    let mut store = fresh_store();
    let server = store.server_id(URL).expect("server");
    let key = diff_key(&f.target, "src/a.rs");
    let cached = crate::diff::tests::fixture("handwritten/diff_rename.response.json");
    store
        .save_diff(server, &key, &cached, jiff::Timestamp::now())
        .expect("save");
    let s = session(store, &http, true);
    let (diff, from) =
        run(load_diff(s.syncer().clone(), key, true, Some(source(&f)))).expect("diff");
    assert_eq!(from, DiffSource::Cache);
    assert_eq!(diff, cached);
}

#[gpui_kit::test]
fn what_the_checkout_cannot_serve_comes_from_gerrit(_cx: &mut TestAppContext) {
    let f = fixture();
    let local = source(&f);
    // A file the list does not have (maybe a rename), a magic file, and
    // another patch set's commit are all Gerrit's.
    let http = gerrit_diff();
    let s = session(fresh_store(), &http, true);
    for key in [
        diff_key(&f.target, "src/unlisted.rs"),
        diff_key(&f.target, "/COMMIT_MSG"),
        diff_key(&f.base, "src/a.rs"),
    ] {
        let (_, from) = run(load_diff(
            s.syncer().clone(),
            key.clone(),
            true,
            Some(local.clone()),
        ))
        .expect("diff");
        assert_eq!(from, DiffSource::Gerrit, "{}", key.path);
    }
    assert_eq!(http.requested().len(), 3);
    // Without a file list nothing is guessed at either.
    let bare = LocalSource {
        renames: None,
        ..local.clone()
    };
    assert!(!bare.serves(&diff_key(&f.target, "src/a.rs")));
}

#[gpui_kit::test]
fn a_missing_commit_means_no_local_source_and_gerrit_answers(_cx: &mut TestAppContext) {
    let f = fixture();
    let repo = local_git::Repo::open(f.repo.path()).expect("open");
    let missing = "0123456789012345678901234567890123456789";
    assert!(LocalSource::resolve(repo, &files_key(missing)).is_none());
    let http = gerrit_diff();
    let s = session(fresh_store(), &http, true);
    let (_, from) = run(load_diff(
        s.syncer().clone(),
        diff_key(missing, "src/a.rs"),
        true,
        None,
    ))
    .expect("diff");
    assert_eq!(from, DiffSource::Gerrit);
    // And offline without a source it is an error, as before.
    let offline = session(fresh_store(), &http, false);
    let err = run(load_diff(
        offline.syncer().clone(),
        diff_key(missing, "src/a.rs"),
        false,
        None,
    ))
    .expect_err("fails");
    assert!(matches!(err, LoadError::Failed(_)));
}

#[gpui_kit::test]
fn a_rename_is_diffed_against_the_old_path(_cx: &mut TestAppContext) {
    let r = TestRepo::new();
    r.write("old.rs", b"keep\nold line\nkeep2\n");
    r.commit("first");
    r.git(&["mv", "old.rs", "new.rs"]);
    r.write("new.rs", b"keep\nnew line\nkeep2\n");
    let target = r.commit("rename");
    let repo = local_git::Repo::open(r.path()).expect("open");
    let mut renamed = file("new.rs", FileStatus::Renamed);
    renamed.old_path = Some("old.rs".into());
    let local = LocalSource::resolve(repo, &files_key(&target))
        .expect("resolves")
        .with_files(&[renamed]);
    let diff = local.diff(&diff_key(&target, "new.rs")).expect("diff");
    // Common lines on both sides prove the old side was read from old.rs.
    assert!(
        diff.chunks
            .iter()
            .any(|c| matches!(c, model::DiffChunk::Common { .. })),
        "{diff:?}"
    );
}
