fn main() {
    std::process::exit(zoos_runner_ort::run_cli(std::env::args().skip(1)));
}
