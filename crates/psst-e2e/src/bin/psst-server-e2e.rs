//! `psst-server` under a name that can't collide with the real binary (cargo#6313).

fn main() -> std::process::ExitCode {
    psst_server::main()
}
