use std::env;

fn main() {
    let exit_code = zoos_runner_fake::run_cli(env::args().skip(1));
    std::process::exit(exit_code);
}
