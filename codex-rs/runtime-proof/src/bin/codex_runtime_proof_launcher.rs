fn main() {
    #[cfg(target_os = "linux")]
    let exit_code = match codex_runtime_proof::launcher::run() {
        Ok(code) => code,
        Err(_) => {
            eprintln!("runtime proof launcher failed closed");
            1
        }
    };

    #[cfg(not(target_os = "linux"))]
    let exit_code = {
        eprintln!("runtime proof launcher requires Linux");
        1
    };

    std::process::exit(exit_code);
}
