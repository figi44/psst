//! `psst-server` for end-to-end tests: the same `main` as the real binary, under a name that
//! cannot collide with it (cargo#6313).

fn main() -> std::process::ExitCode {
    psst_server::main()
}
