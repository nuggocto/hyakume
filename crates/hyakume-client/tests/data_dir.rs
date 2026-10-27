use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use hyakume_client::DataDir;

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_profile_directory_ends_up_owner_only_whether_new_or_left_open_by_an_older_build() {
    let dir = tempfile::tempdir().expect("temp dir");
    let fresh = dir.path().join("fresh/hyakume");
    let old = dir.path().join("old");
    std::fs::create_dir(&old).unwrap();
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();

    DataDir::at(&fresh).create().unwrap();
    DataDir::at(&old).create().unwrap();

    assert_eq!(mode(&fresh), 0o700);
    assert_eq!(mode(&old), 0o700);
}
