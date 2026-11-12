//! argv builders for the `launchctl` calls `tephractl daemon` makes.
//! Pure: they only build argument lists, so the orderings in
//! `install.rs` can be tested against a recording runner.

use std::path::Path;

pub const PROGRAM: &str = "/bin/launchctl";

/// The per-user GUI domain, e.g. `gui/501`.
pub fn domain(uid: u32) -> String {
    format!("gui/{uid}")
}

/// A service in that domain, e.g. `gui/501/com.danrfletcher.tephrad`.
pub fn service_target(uid: u32, label: &str) -> String {
    format!("{}/{label}", domain(uid))
}

pub fn print_args(uid: u32, label: &str) -> Vec<String> {
    vec!["print".to_string(), service_target(uid, label)]
}

pub fn bootstrap_args(uid: u32, plist: &Path) -> Vec<String> {
    vec![
        "bootstrap".to_string(),
        domain(uid),
        plist.display().to_string(),
    ]
}

pub fn bootout_args(uid: u32, label: &str) -> Vec<String> {
    vec!["bootout".to_string(), service_target(uid, label)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_and_args_have_the_documented_shape() {
        assert_eq!(domain(501), "gui/501");
        assert_eq!(
            service_target(501, "com.danrfletcher.tephrad"),
            "gui/501/com.danrfletcher.tephrad"
        );
        assert_eq!(
            print_args(501, "x"),
            ["print", "gui/501/x"].map(String::from)
        );
        assert_eq!(
            bootout_args(501, "x"),
            ["bootout", "gui/501/x"].map(String::from)
        );
        assert_eq!(
            bootstrap_args(501, Path::new("/a b/x.plist")),
            ["bootstrap", "gui/501", "/a b/x.plist"].map(String::from)
        );
    }
}
