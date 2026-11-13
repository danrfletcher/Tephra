//! Renders the launchd property list for the `tephrad` agent (G14).
//!
//! Pure: no filesystem, no process, so it is unit-tested on every
//! platform. The output is a plain XML plist, written by hand rather
//! than through a plist crate: the shape is fixed and tiny, and a
//! hand-rolled writer keeps `tephractl` free of another dependency.
//!
//! The agent runs the installed, signed copy of `tephrad` (never a
//! path into `target/`), starts at load, is restarted only after a
//! failed exit (`KeepAlive` with `SuccessfulExit=false`, so a clean
//! SIGTERM stop stays stopped), waits 10 s between restarts so a
//! missing or crashing binary cannot spin, and logs to
//! `~/Library/Logs/tephra/`.

use std::fmt;
use std::path::{Path, PathBuf};

/// Seconds launchd waits between starts (`ThrottleInterval`).
pub const THROTTLE_INTERVAL_SECS: u32 = 10;

/// Everything the plist is made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlistSpec {
    pub label: String,
    /// The installed copy of `tephrad` that launchd runs.
    pub program: PathBuf,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    /// `HOME` of the installing shell.
    pub home: String,
    /// Copied only when set (and non-empty) in the installing shell.
    pub xdg_config_home: Option<String>,
    pub xdg_data_home: Option<String>,
}

/// A value that cannot be written into an XML 1.0 plist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlistError {
    pub field: &'static str,
    pub reason: String,
}

impl fmt::Display for PlistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "can't write {} into the launchd plist: {}",
            self.field, self.reason
        )
    }
}

impl std::error::Error for PlistError {}

/// XML-escapes `s` for element text. `&`, `<` and `>` are escaped (the
/// spec's required set), quotes too so the output is safe anywhere.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// XML 1.0 forbids most control characters, even escaped. Reject them
/// rather than write a plist launchd would refuse.
fn check_xml_chars(field: &'static str, s: &str) -> Result<(), PlistError> {
    match s
        .chars()
        .find(|c| (*c as u32) < 0x20 && !matches!(*c, '\t' | '\n' | '\r'))
    {
        None => Ok(()),
        Some(c) => Err(PlistError {
            field,
            reason: format!("control character U+{:04X} is not valid in XML", c as u32),
        }),
    }
}

fn path_str<'a>(field: &'static str, p: &'a Path) -> Result<&'a str, PlistError> {
    p.to_str().ok_or_else(|| PlistError {
        field,
        reason: "the path is not valid UTF-8".to_string(),
    })
}

fn string_el(field: &'static str, value: &str) -> Result<String, PlistError> {
    check_xml_chars(field, value)?;
    Ok(format!("<string>{}</string>", escape(value)))
}

fn key_el(field: &'static str, key: &str) -> Result<String, PlistError> {
    check_xml_chars(field, key)?;
    Ok(format!("<key>{}</key>", escape(key)))
}

/// Renders the plist. Key order is fixed, so the output is stable.
pub fn render_plist(spec: &PlistSpec) -> Result<String, PlistError> {
    let program = path_str("ProgramArguments", &spec.program)?;
    let stdout_path = path_str("StandardOutPath", &spec.stdout_path)?;
    let stderr_path = path_str("StandardErrorPath", &spec.stderr_path)?;

    let mut env: Vec<(&'static str, &str)> = vec![("HOME", spec.home.as_str())];
    if let Some(v) = spec.xdg_config_home.as_deref().filter(|v| !v.is_empty()) {
        env.push(("XDG_CONFIG_HOME", v));
    }
    if let Some(v) = spec.xdg_data_home.as_deref().filter(|v| !v.is_empty()) {
        env.push(("XDG_DATA_HOME", v));
    }

    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    out.push_str("<plist version=\"1.0\">\n<dict>\n");

    out.push_str(&format!("\t{}\n", key_el("Label", "Label")?));
    out.push_str(&format!("\t{}\n", string_el("Label", &spec.label)?));

    out.push_str(&format!(
        "\t{}\n",
        key_el("ProgramArguments", "ProgramArguments")?
    ));
    out.push_str("\t<array>\n");
    out.push_str(&format!(
        "\t\t{}\n",
        string_el("ProgramArguments", program)?
    ));
    out.push_str("\t</array>\n");

    out.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n");

    out.push_str("\t<key>KeepAlive</key>\n\t<dict>\n");
    out.push_str("\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n");
    out.push_str("\t</dict>\n");

    out.push_str(&format!(
        "\t<key>ThrottleInterval</key>\n\t<integer>{THROTTLE_INTERVAL_SECS}</integer>\n"
    ));

    out.push_str("\t<key>StandardOutPath</key>\n");
    out.push_str(&format!(
        "\t{}\n",
        string_el("StandardOutPath", stdout_path)?
    ));
    out.push_str("\t<key>StandardErrorPath</key>\n");
    out.push_str(&format!(
        "\t{}\n",
        string_el("StandardErrorPath", stderr_path)?
    ));

    out.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
    for (name, value) in env {
        out.push_str(&format!("\t\t{}\n", key_el("EnvironmentVariables", name)?));
        out.push_str(&format!(
            "\t\t{}\n",
            string_el("EnvironmentVariables", value)?
        ));
    }
    out.push_str("\t</dict>\n");

    out.push_str("</dict>\n</plist>\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn spec(home: &str) -> PlistSpec {
        PlistSpec {
            label: "com.danrfletcher.tephrad".to_string(),
            program: PathBuf::from(format!(
                "{home}/Library/Application Support/tephra/bin/tephrad"
            )),
            stdout_path: PathBuf::from(format!("{home}/Library/Logs/tephra/tephrad.out.log")),
            stderr_path: PathBuf::from(format!("{home}/Library/Logs/tephra/tephrad.err.log")),
            home: home.to_string(),
            xdg_config_home: None,
            xdg_data_home: None,
        }
    }

    /// Parses `xml` back with `plutil` (macOS only): `-lint` first,
    /// then `-convert json` so every key can be checked. `None` on
    /// other platforms, where these checks are skipped.
    fn parse_with_plutil(xml: &str) -> Option<Value> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.plist");
        std::fs::write(&path, xml).unwrap();
        let lint = std::process::Command::new("plutil")
            .arg("-lint")
            .arg(&path)
            .output()
            .expect("run plutil");
        assert!(
            lint.status.success(),
            "plutil -lint failed: {}{}",
            String::from_utf8_lossy(&lint.stdout),
            String::from_utf8_lossy(&lint.stderr)
        );
        let json = std::process::Command::new("plutil")
            .args(["-convert", "json", "-o", "-"])
            .arg(&path)
            .output()
            .expect("run plutil");
        assert!(json.status.success());
        Some(serde_json::from_slice(&json.stdout).expect("plutil json"))
    }

    /// Checks every G14 key against `spec`, on the parsed plist.
    fn assert_g14_keys(value: &Value, spec: &PlistSpec) {
        assert_eq!(value["Label"], spec.label.as_str());
        assert_eq!(
            value["ProgramArguments"],
            serde_json::json!([spec.program.to_str().unwrap()])
        );
        assert_eq!(value["RunAtLoad"], true);
        assert_eq!(
            value["KeepAlive"],
            serde_json::json!({"SuccessfulExit": false})
        );
        assert_eq!(value["ThrottleInterval"], 10);
        assert_eq!(value["StandardOutPath"], spec.stdout_path.to_str().unwrap());
        assert_eq!(
            value["StandardErrorPath"],
            spec.stderr_path.to_str().unwrap()
        );
        let env = value["EnvironmentVariables"].as_object().unwrap();
        assert_eq!(env["HOME"], spec.home.as_str());
        match &spec.xdg_config_home {
            Some(v) if !v.is_empty() => assert_eq!(env["XDG_CONFIG_HOME"], v.as_str()),
            _ => assert!(!env.contains_key("XDG_CONFIG_HOME")),
        }
        match &spec.xdg_data_home {
            Some(v) if !v.is_empty() => assert_eq!(env["XDG_DATA_HOME"], v.as_str()),
            _ => assert!(!env.contains_key("XDG_DATA_HOME")),
        }
        assert_eq!(
            env.len(),
            1 + usize::from(
                spec.xdg_config_home
                    .as_deref()
                    .is_some_and(|v| !v.is_empty())
            ) + usize::from(spec.xdg_data_home.as_deref().is_some_and(|v| !v.is_empty())),
            "no other environment variable is copied"
        );
        let top: Vec<&String> = value.as_object().unwrap().keys().collect();
        assert_eq!(top.len(), 8, "exactly the G14 keys, got {top:?}");
    }

    #[test]
    fn default_case_matches_the_fixture_byte_for_byte() {
        let rendered = render_plist(&spec("/Users/dan")).unwrap();
        let expected = include_str!("../../tests/fixtures/expected/plist_default.xml");
        assert_eq!(rendered, expected);
    }

    /// Table-driven: each case is rendered, parsed back with plutil
    /// (macOS) and checked key by key.
    #[test]
    fn every_case_renders_a_valid_plist_with_every_g14_key() {
        let mut with_xdg = spec("/Users/dan");
        with_xdg.xdg_config_home = Some("/Users/dan/.cfg".to_string());
        with_xdg.xdg_data_home = Some("/Users/dan/.data".to_string());

        let mut only_data = spec("/Users/dan");
        only_data.xdg_data_home = Some("/Volumes/Data/xdg".to_string());

        let mut empty_xdg = spec("/Users/dan");
        empty_xdg.xdg_config_home = Some(String::new());
        empty_xdg.xdg_data_home = Some(String::new());

        let mut label_test = spec("/tmp/t");
        label_test.label = "com.danrfletcher.tephrad.test".to_string();

        let cases: Vec<(&str, PlistSpec)> = vec![
            ("xdg unset", spec("/Users/dan")),
            ("xdg config and data set", with_xdg),
            ("only xdg data set", only_data),
            ("xdg set but empty counts as unset", empty_xdg),
            ("space in path", spec("/Users/dan fletcher")),
            ("unicode home", spec("/Users/Zoë Müller/ホーム")),
            ("ampersand in path", spec("/Users/a&b")),
            ("angle brackets in path", spec("/Users/<x>/y>z")),
            ("all three specials", spec("/Users/R&D <team> & co")),
            ("quotes in path", spec("/Users/it's \"mine\"")),
            ("test label", label_test),
        ];

        for (name, case) in &cases {
            let xml = render_plist(case).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"),
                "{name}"
            );
            match parse_with_plutil(&xml) {
                Some(value) => assert_g14_keys(&value, case),
                None => {
                    // Not macOS: no plutil. Still check the text.
                    assert!(xml.contains("<key>RunAtLoad</key>\n\t<true/>"), "{name}");
                    assert!(
                        xml.contains("<key>SuccessfulExit</key>\n\t\t<false/>"),
                        "{name}"
                    );
                    assert!(xml.contains("<integer>10</integer>"), "{name}");
                }
            }
        }
    }

    #[test]
    fn markup_characters_are_escaped_and_never_raw() {
        let xml = render_plist(&spec("/Users/R&D <team> & co")).unwrap();
        assert!(xml.contains("R&amp;D &lt;team&gt; &amp; co"));
        assert!(!xml.contains("R&D"));
        assert!(!xml.contains("<team>"));
    }

    #[test]
    fn xdg_variables_are_written_only_when_set() {
        let unset = render_plist(&spec("/Users/dan")).unwrap();
        assert!(!unset.contains("XDG_CONFIG_HOME"));
        assert!(!unset.contains("XDG_DATA_HOME"));

        let mut s = spec("/Users/dan");
        s.xdg_config_home = Some("/c".to_string());
        s.xdg_data_home = Some("/d".to_string());
        let set = render_plist(&s).unwrap();
        assert!(set.contains("<key>XDG_CONFIG_HOME</key>\n\t\t<string>/c</string>"));
        assert!(set.contains("<key>XDG_DATA_HOME</key>\n\t\t<string>/d</string>"));
    }

    #[test]
    fn a_control_character_is_refused_not_written() {
        let mut s = spec("/Users/dan");
        s.home = "/Users/da\u{1}n".to_string();
        let err = render_plist(&s).unwrap_err();
        assert_eq!(err.field, "EnvironmentVariables");
        assert!(err.to_string().contains("U+0001"));
    }

    #[test]
    fn a_non_utf8_path_is_refused_not_lossily_written() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let mut s = spec("/Users/dan");
            s.program = PathBuf::from(std::ffi::OsStr::from_bytes(b"/Users/\xff/tephrad"));
            let err = render_plist(&s).unwrap_err();
            assert_eq!(err.field, "ProgramArguments");
        }
    }
}
