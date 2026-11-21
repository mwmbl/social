//! Audit F (secrets and deployment): failing tests that demonstrate findings. Each is ignored
//! until fixed; run them with `cargo test -p mwmbl_social --test audit_f -- --ignored`.

use mwmbl_social::config::Config;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const SECRET: &str = "s3cret-admin-token-DO-NOT-LOG";

fn no_env(_: &str) -> Option<String> {
    None
}

/// F1: an `admin_tokens` written as a string (the shape the env override takes) is a parse
/// error, and the error, printed to stderr by `main`, quotes the token.
#[test]
#[ignore = "AUDIT: F1 - fails until fixed"]
fn a_mistyped_token_list_is_not_echoed_in_the_config_error() -> TestResult {
    let text = format!("base_url = \"https://social.example\"\nadmin_tokens = \"{SECRET}\"\n");
    let err = Config::from_sources(Some(("config.toml", &text)), no_env)
        .err()
        .ok_or("expected a parse error")?;
    let shown = format!("{err}");
    assert!(
        !shown.contains(SECRET),
        "config error leaks the admin token:\n{shown}"
    );
    Ok(())
}

/// F1: a misspelt key (`admin_token`) is refused by `deny_unknown_fields`, and the error quotes
/// the whole source line, token included.
#[test]
#[ignore = "AUDIT: F1 - fails until fixed"]
fn a_misspelt_token_key_is_not_echoed_in_the_config_error() -> TestResult {
    let text = format!("admin_token = [\"{SECRET}\"]\n");
    let err = Config::from_sources(Some(("config.toml", &text)), no_env)
        .err()
        .ok_or("expected a parse error")?;
    let shown = format!("{err}");
    assert!(
        !shown.contains(SECRET),
        "config error leaks the admin token:\n{shown}"
    );
    Ok(())
}

/// F2: the SQLite database holds every registration's Ed25519 private key
/// (`registrations.our_private_key`), yet it is created with the process umask (0644 under the
/// usual 022), unlike `keys/actor.pem` (0600 in a 0700 directory).
#[cfg(unix)]
#[test]
#[ignore = "AUDIT: F2 - fails until fixed"]
fn the_store_holding_registration_keys_is_private() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    let config = Config {
        data_dir: dir.path().join("data"),
        ..Config::default()
    };
    std::fs::create_dir_all(&config.data_dir)?;
    let path = config.store_path();
    drop(store::Store::open(&path)?);
    let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
    assert_eq!(
        mode & 0o077,
        0,
        "{} is mode {mode:o}: other local users can read the FASP private keys",
        path.display()
    );
    Ok(())
}
