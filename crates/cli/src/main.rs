//! The `chaldea` binary: one call into [`chaldea_cli::dispatch`] (hook-ingest §1).

fn main() {
    chaldea_cli::dispatch(
        &std::env::args_os().collect::<Vec<_>>(),
        chaldea_cli::Stall::Off,
    )
}
