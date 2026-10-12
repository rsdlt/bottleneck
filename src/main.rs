use std::process::ExitCode;

const USAGE: &str = "usage: bottleneck <record|view|open>";

fn main() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}
