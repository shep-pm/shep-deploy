use super::*;
use crate::fixtures;
use shep_client::dogs::SECRET_KEY;

/// `[deploy]`'s text, as the layer every sheep's own table is put over.
///
/// # Panics
/// If `text` is not a TOML table.
#[track_caller]
fn section(text: &str) -> DogTable {
    shared(toml::from_str(text).expect("the section is TOML"))
}

/// A sheep's own table, as the shepherd holds it.
///
/// # Panics
/// If `text` is not a TOML table.
#[track_caller]
fn own(text: &str) -> DogTable {
    toml::from_str(text).expect("the table is TOML")
}

/// fails if a table does not reach every key this dog reads, or if `env`,
/// which only a hook carries, is lost on the way.
#[test]
fn a_table_parses_into_settings() {
    let app = fixtures::app_with_settings(
        "web",
        "command = 'make build'\nartifacts = ['target/release/koji']\n\
         reload = ['cmd/voting']\n\
         [release]\ncommand = 'migrate'\nenv = { PASSWORD = 'hunter2' }\n\
         [after]\ncommand = 'purge-cache'\n",
    );

    let settings = from_app("web", &fixtures::no_section(), &app).expect("parses");

    assert_eq!(settings.command.as_deref(), Some("make build"));
    assert_eq!(settings.artifacts, [PathBuf::from("target/release/koji")]);
    assert_eq!(settings.reload, ["cmd/voting"]);
    assert_eq!(settings.release.command.as_deref(), Some("migrate"));
    assert_eq!(
        settings.release.env.get("PASSWORD").map(String::as_str),
        Some("hunter2")
    );
    assert_eq!(settings.after.command.as_deref(), Some("purge-cache"));
}

/// fails if a sheep with no table, or an empty one, stops meaning "no build, no
/// hooks, every sha deploys".
#[test]
fn no_table_and_an_empty_table_are_both_the_default() {
    let bare = fixtures::app_named("web");
    assert_eq!(
        from_app("web", &fixtures::no_section(), &bare).expect("parses"),
        Settings::default()
    );

    let empty = fixtures::app_with_settings("web", "");
    assert_eq!(
        from_app("web", &fixtures::no_section(), &empty).expect("parses"),
        Settings::default()
    );
}

/// fails if a typo is ignored, which is a build or a filter that is never
/// applied with nothing saying so. The sheep is named because the table is not
/// in any file the operator has open at that moment.
#[test]
fn an_unknown_key_is_refused_naming_the_key_and_the_sheep() {
    let app = fixtures::app_with_settings("web", "commnd = 'make'\n");

    let err = from_app("web", &fixtures::no_section(), &app).expect_err("refused");

    assert!(matches!(err, Error::Config(_)), "{err}");
    let shown = err.to_string();
    assert!(shown.contains("commnd"), "{shown}");
    assert!(shown.contains("web"), "{shown}");
}

/// fails if a hook's key is checked less strictly than the table's.
#[test]
fn an_unknown_hook_key_is_refused() {
    let app = fixtures::app_with_settings("web", "[after]\ncommnd = 'true'\n");

    let err = from_app("web", &fixtures::no_section(), &app).expect_err("refused");

    assert!(err.to_string().contains("commnd"), "{err}");
}

/// fails if an artifact that can leave the release is accepted because it came
/// from the operator's table: the copy-back runs at the dog's own uid whoever
/// wrote the path.
#[test]
fn an_artifact_outside_the_release_is_refused() {
    let app = fixtures::app_with_settings("web", "artifacts = ['../../etc/passwd']\n");

    let err = from_app("web", &fixtures::no_section(), &app).expect_err("refused");

    assert!(err.to_string().contains(".."), "{err}");
}

/// fails if a value of the wrong type is echoed into the error: a hook's `env`
/// holds secrets, and serde quotes what it could not use.
#[test]
fn a_wrong_typed_value_is_not_echoed() {
    for table in [
        "[release]\nenv = { PASSWORD = 1234567 }\n",
        "[release]\nenv = 'hunter2'\n",
        "release = 'hunter2'\n",
        "reload = 'hunter2'\n",
    ] {
        let app = fixtures::app_with_settings("web", table);

        let shown = from_app("web", &fixtures::no_section(), &app)
            .expect_err("refused")
            .to_string();

        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(!shown.contains("1234567"), "{shown}");
        assert!(shown.contains("web"), "{shown}");
    }
}

/// fails if `Debug` prints a hook's `env` values through `Settings`.
#[test]
fn debug_redacts_hook_env_values() {
    let settings = Settings {
        command: Some("make".to_owned()),
        release: HookSpec {
            command: Some("migrate".to_owned()),
            env: [("PASSWORD".to_owned(), "hunter2".to_owned())].into(),
        },
        ..Settings::default()
    };

    assert_eq!(
        format!("{settings:?}"),
        "Settings { interval: UpDuration(30s), retention: 5, git_timeout: UpDuration(300s), \
         build_timeout: UpDuration(3600s), passthrough: [], branch: None, watch: Auto, \
         verify: Probed, command: Some(\"make\"), build_env: <0 vars>, artifacts: [], reload: [], \
         release: HookSpec { command: Some(\"migrate\"), env: <1 vars> }, \
         after: HookSpec { command: None, env: <0 vars> } }"
    );
}

/// fails if `Debug` prints a `build_env` value through `Settings`: it is where
/// an operator keeps a database URL or an API key, and a resolved secret is the
/// value itself.
///
/// Exact, so that a `derive(Debug)` put back on `Settings` fails here.
#[test]
fn debug_redacts_build_env_values() {
    let settings = Settings {
        build_env: [(
            "PROD_DB_URL".to_owned(),
            "postgres://u:hunter2@db".to_owned(),
        )]
        .into(),
        ..Settings::default()
    };

    let shown = format!("{settings:?}");

    assert!(!shown.contains("hunter2"), "{shown}");
    assert!(!shown.contains("postgres://"), "{shown}");
    assert!(shown.contains("build_env: <1 vars>"), "{shown}");
}

/// fails if a message carries a control character from a key the operator
/// typed: serde names an unknown field in its message, and a TOML key can be
/// any quoted string.
#[test]
fn a_parser_message_never_carries_a_control_character() {
    let app = fixtures::app_with_settings("web", "\"a\\u001bb\" = 1\n");

    let shown = from_app("web", &fixtures::no_section(), &app)
        .expect_err("refused")
        .to_string();

    assert!(!shown.chars().any(char::is_control), "{shown:?}");
    assert!(shown.contains("a?b"), "{shown}");
}

/// fails if a sheep's table sets a key the dog's section alone sets. It is
/// refused for that sheep rather than ignored, because a value that is read
/// nowhere and looks like it is read somewhere is the worst kind of setting.
#[test]
fn a_dog_wide_key_in_a_sheeps_table_is_refused_naming_the_key_and_the_sheep() {
    for key in [
        "interval",
        "retention",
        "git_timeout",
        "build_timeout",
        "passthrough",
    ] {
        let err = from_tables(
            "web",
            &fixtures::no_section(),
            Some(&own(&format!("{key} = 1"))),
        )
        .expect_err("refused");

        let shown = err.to_string();
        assert!(shown.contains(key), "{shown}");
        assert!(shown.contains("web"), "{shown}");
        assert!(shown.contains("dog-wide"), "{shown}");
    }
}

/// fails if the dog-wide keys stop being taken out of the shared layer. Left
/// in, a sheep's settings would carry a copy of them that was never validated,
/// and `change` would have a value to pin into the sheep's own table.
#[test]
fn the_shared_layer_keeps_everything_but_the_dog_wide_keys() {
    let shared = section(
        "interval = '5m'\nretention = 3\nbranch = 'main'\ncommand = 'make'\n\
         passthrough = ['CARGO_HOME']\n",
    );

    let kept: Vec<&str> = shared.as_map().keys().map(String::as_str).collect();
    assert_eq!(kept, ["branch", "command"]);
}

/// fails if a sheep's table stops overriding the dog's section, or if the
/// section stops reaching the sheep it shares the value with.
#[test]
fn a_sheeps_own_key_wins_and_a_shared_one_reaches_the_sheep_that_lacks_it() {
    let shared = section("branch = 'main'\ncommand = 'make'\nreload = ['src/**']\n");

    let follows = from_tables("web", &shared, Some(&own("branch = 'release'\n")))
        .expect("the sheep sets its own branch");
    assert_eq!(follows.branch.as_deref(), Some("release"));
    assert_eq!(follows.command.as_deref(), Some("make"), "shared");
    assert_eq!(follows.reload, ["src/**"], "shared");

    let shared_only = from_tables("api", &shared, None).expect("no table of its own");
    assert_eq!(shared_only.branch.as_deref(), Some("main"));
    assert_eq!(shared_only.command.as_deref(), Some("make"));
}

/// fails if a sheep's hook table stops adding to the dog's instead of replacing
/// it whole. `DogTable::layered` merges nested tables key by key and replaces
/// every other value, so a sheep can add its own `env` to a migration command
/// every sheep shares.
#[test]
fn a_sheeps_hook_adds_to_the_shared_hook_key_by_key() {
    let shared = section("[release]\ncommand = 'migrate'\nenv = { REGION = 'eu' }\n");

    let settings = from_tables(
        "web",
        &shared,
        Some(&own(
            "[release]\ncommand = 'migrate --strict'\nenv = { DB = 'x' }\n",
        )),
    )
    .expect("parses");

    assert_eq!(
        settings.release.command.as_deref(),
        Some("migrate --strict")
    );
    let env: Vec<&str> = settings.release.env.keys().map(String::as_str).collect();
    assert_eq!(
        env,
        ["DB", "REGION"],
        "the sheep adds its own var to the shared one"
    );
}

/// fails if the schema stops describing the keys the settings type takes, or
/// stops marking what it marks. `shep lookout` draws its form from this schema
/// and writes what the operator types into a table this dog refuses on read, so
/// a key the schema knows and the type does not is a settings pane that breaks
/// the sheep it writes to.
#[test]
fn the_schema_publishes_every_key_and_marks_the_ones_that_are_not_per_sheep() {
    let schema = shep_client::dogs::config_schema::<Settings>();
    let schema = schema.as_value();
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .expect("a derived struct schema has properties");

    assert_eq!(
        properties.len(),
        14,
        "a key was added or removed without this test being read"
    );
    for name in properties.keys() {
        let layered = DogTable::from(
            [(name.clone(), serde_json::json!([]))]
                .into_iter()
                .collect::<Map<String, Value>>(),
        );
        if let Err(err) = into_settings("web", layered) {
            assert!(
                !err.to_string().contains("unknown field"),
                "the schema publishes `{name}`, which the settings type does not take: {err}"
            );
        }
    }

    let wide: Vec<&str> = properties
        .iter()
        .filter(|(_, property)| {
            property.get(shep_client::dogs::DOG_WIDE_KEY) == Some(&serde_json::Value::Bool(true))
        })
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        wide,
        [
            "build_timeout",
            "git_timeout",
            "interval",
            "passthrough",
            "retention"
        ],
        "the keys the pane shows as dog-wide are the keys a sheep's table is refused for"
    );

    // A hook is one type used twice, so its schema is shared rather than
    // inlined: the pane follows each `release` and `after` to `$defs` and
    // finds the mark there.
    for hook in ["release", "after"] {
        let referred = properties[hook]
            .get("$ref")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("`{hook}` shares one schema with the other hook"))
            .trim_start_matches("#")
            .to_owned();
        let env = schema
            .pointer(&referred)
            .and_then(|hook| hook.pointer("/properties/env"))
            .expect("the pane draws a field for each hook's env");
        assert_eq!(
            env.get(SECRET_KEY),
            Some(&serde_json::Value::Bool(true)),
            "a hook's env is where the operator keeps a password, so the pane shows <set>"
        );
    }
}

/// fails if `change` writes the layered settings back into the sheep's table.
/// A `[deploy]` value copied there is pinned to that sheep: it stops following
/// the dog-wide value the moment the dog's section changes, and it looks like a
/// choice somebody made for that sheep.
#[tokio::test(start_paused = true)]
async fn change_writes_only_the_key_it_changed_into_the_sheeps_own_table() {
    let sheep = fixtures::Sheep::of("web", "command = 'make build'\n");
    let shared = section("branch = 'main'\ninterval = '5m'\ncommand = 'make'\n");

    change(&sheep, "web", [Change::Watch(Watch::Manual)])
        .await
        .expect("written");

    let table = sheep
        .app()
        .dogs
        .get("deploy")
        .expect("the table is there")
        .clone();
    let held: Vec<&str> = table.as_map().keys().map(String::as_str).collect();
    assert_eq!(
        held,
        ["command", "watch"],
        "nothing inherited from [deploy]"
    );
    assert_eq!(
        table.as_map()["command"].as_str(),
        Some("make build"),
        "the sheep's own command is left alone, not replaced by the shared one"
    );

    let after = read(&sheep, "web", &shared).await.expect("read");
    assert_eq!(after.branch.as_deref(), Some("main"), "still inherited");
    assert_eq!(after.watch, Watch::Manual, "and the change landed");
}

/// fails if a sheep with no table of its own stops being readable at all, which
/// is what makes `[deploy]` mean "every sheep" rather than "every sheep that
/// opted in twice".
#[tokio::test(start_paused = true)]
async fn a_sheep_with_no_table_of_its_own_takes_the_dogs_section() {
    let sheep = fixtures::Sheep::of("web", "");
    let shared = section("branch = 'main'\nwatch = 'manual'\n");

    let settings = read(&sheep, "web", &shared).await.expect("read");

    assert_eq!(settings.branch.as_deref(), Some("main"));
    assert_eq!(settings.watch, Watch::Manual);
}

/// fails if a sheep's own table stops being read through the request that
/// resolves its secret references: a hook would run with `{{secret:DB}}` as
/// its password.
///
/// The two requests are answered differently here on purpose. A double that
/// hands one copy to both cannot tell a dog that reads the resolved table from
/// one that reads the copy inside `sheep_config`, and the copy inside the
/// config is unresolved: resolution is a property of the table request, not of
/// the sheep's config.
#[tokio::test(start_paused = true)]
async fn a_sheeps_table_is_read_resolved_and_its_config_is_read_whole() {
    let sheep = fixtures::Sheep::of(
        "web",
        "branch = 'main'\n[release]\nenv = { DB = '{{secret:DB}}' }\n",
    )
    .resolving(&[("DB", "hunter2")]);
    let shared = section("branch = 'main'\n");

    let (app, settings) = held(&sheep, "web", &shared).await.expect("held");

    assert_eq!(app.name, "web", "the build and verify steps need the app");
    assert_eq!(
        settings.release.env.get("DB").map(String::as_str),
        Some("hunter2"),
        "the hook runs with the password, not with the reference to it"
    );
    assert_eq!(
        app.dogs
            .get("deploy")
            .and_then(|table| table.as_map().get("release"))
            .and_then(|release| release.get("env"))
            .and_then(|env| env.get("DB"))
            .and_then(Value::as_str),
        Some("{{secret:DB}}"),
        "the config is handed over whole, as the shepherd stores it"
    );
}
