//! The "Tephra Local" code-signing identity (G20): argv builders for
//! `security`, `openssl` and `codesign`, the OpenSSL config for the
//! certificate, and the `codesign -dvv` parser.
//!
//! Pure: nothing here runs a command or touches a file. The private
//! key is only ever passed to `security import` as a file path in a
//! private temp directory (never in argv, never printed); see
//! `install.rs`.

use std::path::Path;

/// The certificate's common name and the `--sign` identity.
pub const IDENTITY_NAME: &str = "Tephra Local";
/// The code-signing identifier of the installed copy.
pub const IDENTIFIER: &str = "com.danrfletcher.tephrad";

pub const SECURITY: &str = "/usr/bin/security";
pub const CODESIGN: &str = "/usr/bin/codesign";
/// macOS's own LibreSSL; a Homebrew `openssl` earlier on `PATH` must
/// not change what gets generated.
pub const OPENSSL: &str = "/usr/bin/openssl";

/// Ten years: the identity is reused across installs, so a short
/// lifetime would silently invalidate the Full Disk Access grant.
pub const CERT_DAYS: &str = "3650";

/// OpenSSL config for a self-signed code-signing certificate: not a
/// CA, `digitalSignature` only, EKU `codeSigning`.
pub fn openssl_config() -> String {
    format!(
        "[req]\n\
         distinguished_name = dn\n\
         x509_extensions = v3_code\n\
         prompt = no\n\
         \n\
         [dn]\n\
         CN = {IDENTITY_NAME}\n\
         \n\
         [v3_code]\n\
         basicConstraints = critical,CA:false\n\
         keyUsage = critical,digitalSignature\n\
         extendedKeyUsage = critical,codeSigning\n"
    )
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

/// `security find-certificate -c "Tephra Local" [<keychain>]`: exit 0
/// when the identity's certificate is already there.
pub fn find_certificate_args(keychain: Option<&Path>) -> Vec<String> {
    let mut a = vec![
        "find-certificate".to_string(),
        "-c".to_string(),
        IDENTITY_NAME.to_string(),
    ];
    if let Some(k) = keychain {
        a.push(s(k));
    }
    a
}

pub fn openssl_req_args(config: &Path, key_out: &Path, cert_out: &Path) -> Vec<String> {
    vec![
        "req".to_string(),
        "-x509".to_string(),
        "-newkey".to_string(),
        "rsa:2048".to_string(),
        "-nodes".to_string(),
        "-keyout".to_string(),
        s(key_out),
        "-out".to_string(),
        s(cert_out),
        "-days".to_string(),
        CERT_DAYS.to_string(),
        "-config".to_string(),
        s(config),
        "-extensions".to_string(),
        "v3_code".to_string(),
    ]
}

/// `security import <file> [-k <keychain>] -T /usr/bin/codesign`.
pub fn import_args(file: &Path, keychain: Option<&Path>) -> Vec<String> {
    let mut a = vec!["import".to_string(), s(file)];
    if let Some(k) = keychain {
        a.push("-k".to_string());
        a.push(s(k));
    }
    a.push("-T".to_string());
    a.push(CODESIGN.to_string());
    a
}

/// Marks the self-signed certificate trusted for code signing, which
/// `codesign` needs before it will use the identity. This is the step
/// macOS may ask for the user's password or approval on.
pub fn trust_args(cert: &Path, keychain: Option<&Path>) -> Vec<String> {
    let mut a = vec![
        "add-trusted-cert".to_string(),
        "-r".to_string(),
        "trustRoot".to_string(),
        "-p".to_string(),
        "codeSign".to_string(),
    ];
    if let Some(k) = keychain {
        a.push("-k".to_string());
        a.push(s(k));
    }
    a.push(s(cert));
    a
}

pub fn list_keychains_args() -> Vec<String> {
    ["list-keychains", "-d", "user"].map(String::from).to_vec()
}

pub fn set_keychain_list_args(keychains: &[String]) -> Vec<String> {
    let mut a: Vec<String> = ["list-keychains", "-d", "user", "-s"]
        .map(String::from)
        .to_vec();
    a.extend(keychains.iter().cloned());
    a
}

/// Parses `security list-keychains` output: one quoted path per line.
pub fn parse_keychain_list(output: &str) -> Vec<String> {
    output
        .lines()
        .map(|l| l.trim().trim_matches('"').to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// `codesign --force --sign "Tephra Local" --identifier
/// com.danrfletcher.tephrad [--keychain <k>] <path>`.
pub fn sign_args(path: &Path, keychain: Option<&Path>) -> Vec<String> {
    let mut a = vec![
        "--force".to_string(),
        "--sign".to_string(),
        IDENTITY_NAME.to_string(),
        "--identifier".to_string(),
        IDENTIFIER.to_string(),
    ];
    if let Some(k) = keychain {
        a.push("--keychain".to_string());
        a.push(s(k));
    }
    a.push(s(path));
    a
}

/// `codesign -dvv <path>`: prints the signature details on stderr.
pub fn display_args(path: &Path) -> Vec<String> {
    vec!["-dvv".to_string(), s(path)]
}

/// The first `Authority=` line of `codesign -dvv` output: the leaf
/// certificate that signed the binary.
pub fn parse_authority(report: &str) -> Option<String> {
    report
        .lines()
        .find_map(|l| l.strip_prefix("Authority="))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_a_non_ca_code_signing_certificate() {
        let c = openssl_config();
        assert!(c.contains("CN = Tephra Local"));
        assert!(c.contains("basicConstraints = critical,CA:false"));
        assert!(c.contains("keyUsage = critical,digitalSignature"));
        assert!(c.contains("extendedKeyUsage = critical,codeSigning"));
    }

    #[test]
    fn sign_args_match_the_documented_command() {
        assert_eq!(
            sign_args(Path::new("/x/tephrad"), None),
            [
                "--force",
                "--sign",
                "Tephra Local",
                "--identifier",
                "com.danrfletcher.tephrad",
                "/x/tephrad"
            ]
            .map(String::from)
        );
        let with = sign_args(Path::new("/x/tephrad"), Some(Path::new("/k.keychain-db")));
        assert_eq!(
            &with[5..7],
            ["--keychain", "/k.keychain-db"].map(String::from)
        );
    }

    #[test]
    fn import_grants_codesign_and_keychain_is_optional() {
        assert_eq!(
            import_args(Path::new("/t/key.pem"), None),
            ["import", "/t/key.pem", "-T", "/usr/bin/codesign"].map(String::from)
        );
        assert_eq!(
            import_args(Path::new("/t/key.pem"), Some(Path::new("/k"))),
            [
                "import",
                "/t/key.pem",
                "-k",
                "/k",
                "-T",
                "/usr/bin/codesign"
            ]
            .map(String::from)
        );
    }

    #[test]
    fn the_private_key_path_never_appears_in_the_trust_or_find_args() {
        let find = find_certificate_args(None);
        assert_eq!(
            find,
            ["find-certificate", "-c", "Tephra Local"].map(String::from)
        );
        let trust = trust_args(Path::new("/t/cert.pem"), None);
        assert!(trust.iter().all(|a| !a.contains("key.pem")));
    }

    #[test]
    fn authority_is_the_first_authority_line() {
        let report = "Executable=/x\nIdentifier=com.danrfletcher.tephrad\nAuthority=Tephra Local\nTeamIdentifier=not set\n";
        assert_eq!(parse_authority(report), Some("Tephra Local".to_string()));
        assert_eq!(parse_authority("code object is not signed at all"), None);
    }

    #[test]
    fn keychain_list_output_is_unquoted() {
        let out = "    \"/Users/dan/Library/Keychains/login.keychain-db\"\n    \"/Library/Keychains/System.keychain\"\n";
        assert_eq!(
            parse_keychain_list(out),
            vec![
                "/Users/dan/Library/Keychains/login.keychain-db".to_string(),
                "/Library/Keychains/System.keychain".to_string()
            ]
        );
    }
}
