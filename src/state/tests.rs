use super::*;
use crate::fixtures::{self, SHA};

/// fails if a key `deploy.toml` does not know is silently dropped.
///
/// An older shep-deploy reads a record a newer one wrote, and a key it
/// does not know is a field the newer one relies on. Without this a
/// `stagd` parses fine, leaves nothing staged, and the next promote
/// refuses with nothing anywhere indicating the field was ignored.
#[test]
fn an_unknown_key_in_the_record_is_refused_and_named() {
    // `checkout` is present, and load-bearing: it has no default, so
    // leaving it out made the document invalid twice over and the test
    // passed on whichever error serde reported first. It still caught the
    // guard being removed, but failed with "TOML parse error at line 1"
    // instead of naming the typo, which sends the next reader to the wrong
    // place.
    let toml = r#"
remote = "https://example.invalid/repo.git"
checkout = "/srv/web"
stagd = "x"
"#;
    let err = toml::from_str::<State>(toml).expect_err("a typo must not parse");
    assert!(
        format!("{err}").contains("stagd"),
        "the refusal must name the key: {err}"
    );
}

/// fails if removal cannot read a record written before the keys moved, or
/// if it reads them back into the record.
#[test]
fn a_restore_read_drops_the_moved_keys_instead_of_refusing_them() {
    let text = "remote = \"https://example.com/x\"\ncheckout = \"/srv/x\"\n\
                branch = \"release\"\nwatch = \"manual\"\nverify = \"alive\"\n";
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deploy.toml");
    fs::write(&path, text).expect("write record");

    let state = State::read_for_restore(&path).expect("reads");

    assert_eq!(state.branch, "");
    assert_eq!(state.watch, Watch::Auto);
    assert_eq!(state.verify, Verify::Probed);
}

/// fails if a record that still carries a key that moved to the sheep's
/// table is read, or is refused without saying where the key went. The
/// copy in the file would be ignored by every deploy, so reading it is
/// the silent version of the mistake.
#[test]
fn a_record_carrying_a_moved_key_is_refused_naming_the_key_and_its_new_home() {
    for (key, value) in [
        ("verify", "\"alive\""),
        ("watch", "\"manual\""),
        ("branch", "\"main\""),
    ] {
        let text =
            format!("remote = \"https://example.com/x\"\ncheckout = \"/srv/x\"\n{key} = {value}\n");

        let err = read_text(&text).expect_err(&format!("must refuse: {key}"));

        assert!(matches!(err, Error::Config(_)), "{key}: {err}");
        let shown = err.to_string();
        assert!(shown.contains(&format!("`{key}`")), "{shown}");
        assert!(shown.contains("[app.dogs.deploy]"), "{shown}");
    }
}

/// fails if state does not survive a write-then-read, or if the write
/// puts a setting in the file. This file is the only record of how a
/// sheep ran BEFORE the dog took over, so losing a field means removal
/// cannot restore the sheep and the operator is left with an app running
/// from a path under $SHEP_HOME they have no reason to know about.
///
/// The settings are non-defaults, so a key written or an adoption that
/// drops one shows.
#[test]
fn state_round_trips_through_toml_without_the_settings() {
    let settings = Settings {
        branch: Some("release".to_owned()),
        watch: Watch::Manual,
        verify: Verify::Alive,
        ..Settings::default()
    };
    let mut original = State {
        remote: "https://github.com/WatWowMap/ReactMap".into(),
        deployed: Some("a1b2c3a1b2c3a1b2c3a1b2c3a1b2c3a1b2c3a1b2".into()),
        origin_cwd: Some(PathBuf::from("/srv/reactmap")),
        origin_script: Some("bun .".into()),
        checkout: PathBuf::from("/srv/reactmap"),
        ..fixtures::state()
    };
    original.adopt("web", &settings).expect("adopts");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deploy.toml");

    original.write(&path).expect("writes");
    let text = fs::read_to_string(&path).expect("record");
    let mut back = State::read(&path).expect("reads");

    for key in ["branch", "watch", "verify", "release", "alive", "manual"] {
        assert!(!text.contains(key), "{key} is in the file:\n{text}");
    }
    assert_eq!(back.branch, "", "nothing is adopted by reading");
    back.adopt("web", &settings).expect("adopts");
    assert_eq!(back, original);
}

/// Whether any `*.tmp` sits in `dir`: the staging file a write leaves
/// behind when it does not finish.
fn any_tmp(dir: &Path) -> bool {
    fs::read_dir(dir)
        .expect("list")
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
}

/// Writes `text` as a record and reads it back through `State::read`,
/// so the validation runs the way it does in production.
fn read_text(text: &str) -> Result<State, Error> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deploy.toml");
    fs::write(&path, text).expect("write record");
    State::read(&path)
}

/// fails if a record that parses is acted on when its values cannot
/// work. Each of these used to reach git, or a path join, and fail
/// there in words about the mechanism: an empty `branch` as "needed a
/// single revision", `deployed = ""` as a rollback onto `releases/`
/// itself, a `remote` beginning with `-` as whatever git made of the
/// option. The refusal has to name the field an operator typed.
#[test]
fn a_record_whose_values_cannot_work_is_refused_by_field() {
    let base =
        |extra: &str| format!("remote = \"https://example.com/x\"\ncheckout = \"/srv/x\"\n{extra}");
    let cases = [
        ("remote", "remote = \"\"\ncheckout = \"/srv/x\"".to_owned()),
        (
            "remote",
            "remote = \"--upload-pack=x\"\ncheckout = \"/srv/x\"".to_owned(),
        ),
        (
            "checkout",
            "remote = \"https://example.com/x\"\ncheckout = \"\"".to_owned(),
        ),
        (
            "checkout",
            "remote = \"https://example.com/x\"\ncheckout = \"srv/x\"".to_owned(),
        ),
        ("deployed", base("deployed = \"\"")),
        ("deployed", base("deployed = \"main\"")),
        ("deployed", base("deployed = \"abc123\"")),
        ("failed", base("failed = \"..\"")),
        (
            "failed",
            base(&format!("deployed = \"{SHA}\"\nfailed = \"{SHA}\"")),
        ),
        ("origin_script", base("origin_cwd = \"/srv/x\"")),
        ("origin_cwd", base("origin_script = \"bun .\"")),
    ];
    for (field, text) in cases {
        let err = read_text(&text).expect_err(&format!("must refuse: {text}"));
        assert!(matches!(err, Error::Config(_)), "{text}: {err}");
        assert!(
            err.to_string().contains(&format!("`{field}`")),
            "must name `{field}`: {err}"
        );
    }
}

/// Adopts `branch` as the table's, into a fresh record.
fn adopt_branch(branch: Option<&str>) -> Result<State, Error> {
    let mut state = fixtures::state();
    state.adopt(
        "web",
        &Settings {
            branch: branch.map(str::to_owned),
            ..Settings::default()
        },
    )?;
    Ok(state)
}

/// fails if a branch that cannot work is adopted, or is refused in words
/// about git's mechanism. Each of these used to reach git: an empty one as
/// "needed a single revision", one beginning `-` as whatever git made of
/// the option. The refusal has to name the table the operator typed it in.
#[test]
fn a_branch_that_cannot_work_is_refused_naming_the_table() {
    for branch in ["", "  ", "-x", "ma\nin"] {
        let err = adopt_branch(Some(branch)).expect_err(&format!("must refuse: {branch:?}"));
        assert!(matches!(err, Error::Config(_)), "{branch:?}: {err}");
        let shown = err.to_string();
        assert!(shown.contains("`branch`"), "{branch:?}: {shown}");
        assert!(shown.contains("[app.dogs.deploy]"), "{branch:?}: {shown}");
    }
}

/// fails if a table with no `branch` is adopted as one that follows
/// nothing, or as `main`. A guess would deploy a branch nobody chose.
#[test]
fn a_table_with_no_branch_is_refused_naming_the_key() {
    let err = adopt_branch(None).expect_err("no branch to follow");

    let shown = err.to_string();
    assert!(matches!(err, Error::Config(_)), "{shown}");
    assert!(shown.contains("sets no `branch`"), "{shown}");
    assert!(shown.contains("[app.dogs.deploy]"), "{shown}");
}

/// fails if a `refs/` value that git would refuse as a ref name, or that
/// would change what a refspec means, reaches the fetch. `:` splits a
/// refspec into source and destination and `*` makes it a pattern, so
/// either would make the dog fetch something other than the ref named.
#[test]
fn a_ref_with_a_colon_or_star_is_refused() {
    for value in [
        "refs/pull/1/head:refs/heads/main",
        "refs/pull/*/head",
        "refs/pull/1 /head",
        "refs/pull/?/head",
        "refs/pull/[1]/head",
        "refs/pull\\1/head",
        "refs/pull/1^/head",
        "refs/pull/1~/head",
        "refs/pull/../head",
        "refs/pull/@{1}/head",
        "refs/pull/1/",
        "refs/pull/1.lock",
        "refs/pull/1.lock/head",
        "refs/pull/.hidden/head",
        "refs/pull/1/head.",
        "refs/pull//1/head",
    ] {
        let err = adopt_branch(Some(value)).expect_err(&format!("must refuse: {value}"));
        assert!(matches!(err, Error::Config(_)), "{value}: {err}");
        assert!(err.to_string().contains("`branch`"), "{value}: {err}");
    }
}

/// fails if a ref a forge really publishes is refused.
#[test]
fn a_well_formed_pull_ref_is_accepted() {
    for value in [
        "refs/pull/1/head",
        "refs/pull/1234/merge",
        "refs/tags/v1.2.3",
    ] {
        let state = adopt_branch(Some(value)).expect(value);
        assert_eq!(state.branch, value);
    }
}

/// fails if a record with every value in shape is refused, or if a
/// zero-length one - what a crash mid-write used to be able to leave -
/// is read as anything but a named failure.
#[test]
fn a_sound_record_reads_and_an_empty_one_is_refused() {
    let text = format!(
        "remote = \"https://example.com/x\"\ncheckout = \"/srv/x\"\n\
         deployed = \"{SHA}\"\norigin_cwd = \"/srv/x\"\norigin_script = \"bun .\"\n"
    );
    let state = read_text(&text).expect("a sound record");
    assert_eq!(state.deployed.as_deref(), Some(SHA));

    let err = read_text("").expect_err("nothing to read");
    assert!(matches!(err, Error::Config(_)), "{err}");
    assert!(err.to_string().contains("deploy.toml"), "{err}");
}

/// fails if the pre-adoption app definition does not survive a
/// write-then-read, `env` and probe included, or if a record written
/// before the field existed stops reading. The first is what removal
/// restores from; the second is every record on disk today.
#[test]
fn the_origin_round_trips_and_an_older_record_still_reads() {
    let origin: AppConfig = toml::from_str(
        "name = \"web\"\nscript = \"server.js\"\ninstances = 2\n[env]\nTOKEN = \"s3cret\"\n\
         [readiness_probe]\nkind = \"http\"\ntarget = \"http://127.0.0.1:3000/health\"\n",
    )
    .expect("an app");
    let mut with = sample(None);
    with.origin = Some(origin.clone());
    with.origin_cwd = Some(PathBuf::from("/srv/web"));
    with.origin_script = Some("server.js".to_owned());
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deploy.toml");
    with.write(&path).expect("writes");
    let back = State::read(&path).expect("reads");
    assert_eq!(back.origin.as_ref(), Some(&origin));

    let older = read_text("remote = \"https://example.com/x\"\ncheckout = \"/srv/x\"\n")
        .expect("a record from before the field");
    assert_eq!(older.origin, None);
}

/// fails if a `State` printed with `{:?}` shows an `env` value. The
/// record carries the app's `env` verbatim in `origin`, and the derive
/// is safe only because `AppConfig`'s own `Debug` prints the map as a
/// count; this pins that exact shape so a change to either side shows
/// up here rather than in a log.
#[test]
fn debug_output_redacts_the_origins_env() {
    let origin: AppConfig = toml::from_str(
        "name = \"web\"\nscript = \"server.js\"\n[env]\nTOKEN = \"hunter2-distinctive\"\n",
    )
    .expect("an app");
    let state = State {
        origin: Some(origin),
        ..fixtures::state()
    };

    let shown = format!("{state:?}");
    assert!(!shown.contains("hunter2"), "{shown}");
    assert!(!shown.contains("TOKEN"), "{shown}");
    assert!(shown.contains("env: <1 vars>"), "{shown}");
}

/// fails if a record carrying `origin` without the legacy pair reads.
/// Removal puts the sheep back through both, and a record with one and
/// not the other was accepted and then skipped as a dog-bootstrapped
/// sheep, leaving it under the tree with its origin unread.
#[test]
fn an_origin_without_the_legacy_pair_is_refused_by_name() {
    let origin: AppConfig =
        toml::from_str("name = \"web\"\nscript = \"server.js\"\ncwd = \"/srv/web\"\n")
            .expect("an app");
    let state = State {
        origin: Some(origin),
        origin_cwd: None,
        origin_script: None,
        ..fixtures::state()
    };

    let err = state
        .validate(Path::new("/x/deploy.toml"))
        .expect_err("origin without the pair");
    let shown = err.to_string();
    assert!(shown.contains("origin"), "{shown}");
    assert!(shown.contains("origin_cwd"), "{shown}");
}

/// fails if a write follows a link planted where the record goes. The
/// rename replaces the link with the real file; the file the link named
/// is never opened, let alone written with the record's `env`.
#[test]
fn a_link_at_the_record_path_is_replaced_not_written_through() {
    let dir = tempfile::tempdir().expect("tempdir");
    let victim = dir.path().join("victim");
    fs::write(&victim, "untouched").expect("victim");
    let path = dir.path().join("deploy.toml");
    std::os::unix::fs::symlink(&victim, &path).expect("a planted link");

    sample(None).write(&path).expect("writes");

    assert_eq!(fs::read_to_string(&victim).expect("victim"), "untouched");
    assert!(
        !fs::symlink_metadata(&path)
            .expect("record")
            .file_type()
            .is_symlink(),
        "the record is a real file now"
    );
    assert!(State::read(&path).is_ok());
}

/// fails if the record is readable by anyone but its owner. It carries
/// the app's `env` verbatim once an origin is recorded, and shep's own
/// roll is owner-only for the same reason.
#[test]
fn the_record_is_written_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    use shep_client::shep_core::atomic_file::OWNER_ONLY_FILE_MODE;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deploy.toml");
    sample(None).write(&path).expect("writes");
    let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode, OWNER_ONLY_FILE_MODE);
}

/// fails if a rename that cannot land leaves its temporary file behind.
/// Nothing reads `deploy.toml.tmp`, so a stale one would sit beside the
/// record forever, a full copy of it that no listing ever mentions.
#[test]
fn a_failed_write_leaves_no_tmp_file_behind() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A directory where the record should go, so the rename fails.
    let path = dir.path().join("deploy.toml");
    fs::create_dir_all(&path).expect("something in the way");

    sample(None)
        .write(&path)
        .expect_err("cannot rename onto a directory");

    assert!(
        !any_tmp(dir.path()),
        "the temporary file must be removed with the failure"
    );
}

/// fails if reading a `deploy.toml` that is not there produces
/// anything other than a named I/O failure. An operator asking to
/// deploy a sheep that was never set up as a target meets this path,
/// and the path it names is the only clue about what is missing.
#[test]
fn reading_a_missing_state_file_names_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deploy.toml");
    let err = State::read(&path).expect_err("nothing to read");
    assert!(err.to_string().contains("deploy.toml"));
}

/// fails if a table that sets neither `watch` nor `verify` adopts as
/// anything but auto and probed. A table written before these keys
/// moved there must keep being watched rather than silently stop
/// deploying, and an absent `verify` must mean "wait for the readiness
/// probe", never "deploy without checking".
#[test]
fn a_table_that_sets_neither_adopts_auto_and_probed() {
    let mut state = State {
        watch: Watch::Manual,
        verify: Verify::Alive,
        ..fixtures::state()
    };

    state
        .adopt(
            "web",
            &Settings {
                branch: Some("main".to_owned()),
                ..Settings::default()
            },
        )
        .expect("adopts");

    assert_eq!(state.watch, Watch::Auto);
    assert_eq!(state.verify, Verify::Probed);
}

/// fails if a staged sha is lost between a write and a read, or if a
/// record with nothing staged grows a `staged` line. The second is what
/// keeps an older shep-deploy, which refuses unknown fields, able to
/// read every record that has never been staged.
#[test]
fn a_staged_sha_round_trips_and_an_empty_one_is_not_written() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deploy.toml");

    let staged = State {
        staged: Some(fixtures::OTHER_SHA.to_owned()),
        ..fixtures::state()
    };
    staged.write(&path).expect("writes");
    assert_eq!(State::read(&path).expect("reads").staged, staged.staged);

    fixtures::state().write(&path).expect("writes");
    let text = fs::read_to_string(&path).expect("record");
    assert!(!text.contains("staged"), "{text}");
}

/// fails if a hand-edited `staged` that names no commit is accepted. It
/// is joined onto `releases/` and handed to the next deploy as the
/// release to land, so the refusal has to name the field.
#[test]
fn a_staged_value_that_is_not_a_sha_is_refused_by_field() {
    let base = "remote = \"https://example.com/x\"\ncheckout = \"/srv/x\"\n";
    for staged in ["\"\"", "\"main\"", "\"abc123\"", "\"..\""] {
        let text = format!("{base}staged = {staged}");
        let err = read_text(&text).expect_err(&format!("must refuse: {text}"));
        assert!(matches!(err, Error::Config(_)), "{text}: {err}");
        assert!(err.to_string().contains("`staged`"), "{text}: {err}");
    }
}

/// A minimal, valid `State` for the write tests below, where the exact
/// values don't matter - only that they change between the two writes.
fn sample(deployed: Option<&str>) -> State {
    State {
        deployed: deployed.map(str::to_owned),
        ..fixtures::state()
    }
}

/// Pins that `write` works and that a completed write leaves no `.tmp`
/// file behind - it does NOT, and cannot, prove atomicity. A
/// single-threaded test has no way to observe a process crashing
/// mid-write, so the atomic-rename claim rests on the implementation
/// shape (write to a sibling temp file, then one `rename(2)` within the
/// same directory) rather than on anything this test checks. Two writes
/// are run specifically to prove the *second* write's tmp file is also
/// cleaned up, not just a first write starting from nothing.
#[test]
fn write_survives_a_second_write_and_leaves_no_tmp_file_behind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deploy.toml");
    let tmp = dir.path();

    let first = sample(None);
    first.write(&path).expect("first write");
    assert!(path.exists(), "the state file must exist after a write");
    assert!(
        !any_tmp(tmp),
        "a completed write must not leave a .tmp file"
    );

    let second = sample(Some(fixtures::OTHER_SHA));
    second.write(&path).expect("second write");
    assert!(
        !any_tmp(tmp),
        "a second, overwriting write must also leave no .tmp file"
    );

    assert_eq!(State::read(&path).expect("reads").deployed, second.deployed);
}
