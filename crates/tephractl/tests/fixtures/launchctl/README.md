Fixtures for `daemon::status` parsing `launchctl print gui/<uid>/<label>`.

`print_not_loaded.txt` is verbatim output (exit 113) from this Mac. The
others are based on real `launchctl print` output from other agents on
this Mac with the label, paths and environment rewritten for tephrad
and long blocks dropped. `print_not_running_signal.txt` and
`print_never_run.txt` follow the documented field names but were NOT
captured from a real crashed agent: re-capture them on a Mac where
tephrad has actually been `kill -9`'d.

`print_not_running_exit.txt` uses the real non-zero exit form
`last exit code = 78: EX_CONFIG` (launchd appends the sysexits name);
`parse_exit_code` reads the number before the `:`.
