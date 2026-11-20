//! Hook emitter, GUI-subsystem variant for C-5's mitigation re-run. The
//! attribute only takes effect on Windows and is ignored elsewhere.
#![windows_subsystem = "windows"]

fn main() {
    chs::emitter::main()
}
