use std::path::PathBuf;

use codexctl::config::Paths;

#[test]
fn paths_from_custom_home() {
    let home = PathBuf::from("/tmp/fake-home");
    let paths = Paths::from_home(home);
    assert_eq!(paths.codexctl_dir(), PathBuf::from("/tmp/fake-home/.codexctl"));
    assert_eq!(paths.profiles_dir(), PathBuf::from("/tmp/fake-home/.codexctl/profiles"));
    assert_eq!(paths.active_file(), PathBuf::from("/tmp/fake-home/.codexctl/active"));
    assert_eq!(paths.codex_auth_json(), PathBuf::from("/tmp/fake-home/.codex/auth.json"));
}

#[test]
fn ensure_dirs_creates_profiles_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_home(tmp.path().to_path_buf());
    paths.ensure_dirs().unwrap();
    assert!(paths.profiles_dir().exists());
}
