//! A manual probe of the real macOS Keychain, for the human check at
//! review. It uses the service `com.danrfletcher.tephra.probe` and a dummy
//! value, never a real token.
//!
//!     cargo run -p tephrad --example credential_probe            # set, read back, show state
//!     cargo run -p tephrad --example credential_probe -- get     # read only
//!     cargo run -p tephrad --example credential_probe -- delete  # remove the item
//!
//! After the default run, the item "com.danrfletcher.tephra.probe"
//! (account `github`) shows in Keychain Access. If the access prompt is
//! answered with Deny, `get` reports the denied state. The dummy value is
//! never printed: the probe says whether the stored value matches it.
//!
//! On a build without a Keychain this reports that no store is available
//! and exits non-zero.

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    use tephrad::credentials::{
        credential_state, CredentialState, CredentialStore, KeychainStore, Provider, Secret,
    };

    const SERVICE: &str = "com.danrfletcher.tephra.probe";
    const DUMMY: &str = "tephra-probe-dummy-value-not-a-secret";

    let store = KeychainStore::with_service(SERVICE);
    let provider = Provider::Github;
    let action = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "roundtrip".into());

    let report = |label: &str, state: CredentialState| println!("{label}: {state:?}");

    match action.as_str() {
        "roundtrip" => {
            if let Err(e) = store.set(provider, &Secret::new(DUMMY)) {
                println!("set failed: {e}");
                report("state", credential_state(&store, provider));
                return ExitCode::FAILURE;
            }
            println!("set: ok (service {SERVICE}, account {provider})");
            match store.get(provider) {
                Ok(Some(found)) => println!(
                    "get: ok, the stored value {} the dummy value",
                    if found.expose() == DUMMY {
                        "matches"
                    } else {
                        "DOES NOT match"
                    }
                ),
                Ok(None) => {
                    println!("get: no item found after set");
                    return ExitCode::FAILURE;
                }
                Err(e) => {
                    println!("get failed: {e}");
                    report("state", credential_state(&store, provider));
                    return ExitCode::FAILURE;
                }
            }
            report("state", credential_state(&store, provider));
            println!("the item stays in the keychain; remove it with: -- delete");
            ExitCode::SUCCESS
        }
        "get" => {
            let state = credential_state(&store, provider);
            report("state", state.clone());
            if matches!(state, CredentialState::SignedIn) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        "delete" => match store.delete(provider) {
            Ok(()) => {
                println!("delete: ok");
                ExitCode::SUCCESS
            }
            Err(e) => {
                println!("delete failed: {e}");
                ExitCode::FAILURE
            }
        },
        other => {
            eprintln!("unknown action {other:?}; use roundtrip, get or delete");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    use tephrad::credentials::{credential_state, Provider, UnavailableStore};
    println!(
        "state: {:?}",
        credential_state(&UnavailableStore, Provider::Github)
    );
    println!("this build has no Keychain; run the probe on macOS");
    std::process::ExitCode::FAILURE
}
