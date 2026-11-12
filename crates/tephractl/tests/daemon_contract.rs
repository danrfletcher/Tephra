//! The strings `tephractl` and `tephrad` must agree on, though they
//! share no code (tephrad is a dev-dependency here only).

#[test]
fn the_fix_line_and_error_kind_match_what_tephrad_writes() {
    assert_eq!(
        tephractl::daemon::FIX_LINE,
        tephrad::access::fix_line("~/Documents")
    );
    assert_eq!(
        tephractl::daemon::ERROR_KIND_PERMISSION_DENIED,
        tephrad::access::ERROR_KIND_PERMISSION_DENIED
    );
}

/// `access.json` as tephrad really writes it is what status reads.
#[test]
fn tephractl_reads_the_access_json_tephrad_writes() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap(); // no Documents inside
    let report = tephrad::access::probe_and_record(dir.path(), Some(home.path())).unwrap();
    let read = tephractl::daemon::status::read_access(dir.path()).expect("readable");
    assert_eq!(read.pid, report.pid);
    assert!(!read.documents_readable);
    assert_eq!(read.error_kind.as_deref(), Some("not_found"));
}
