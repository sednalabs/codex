fn main() {
    #[cfg(target_os = "linux")]
    let result = {
        let mut arguments = std::env::args_os().skip(1);
        let first = arguments.next();
        let second = arguments.next();
        match (first, second) {
            (None, None) => codex_runtime_proof::launcher::run(),
            (Some(argument), None)
                if argument.as_os_str() == std::ffi::OsStr::new("provision-api-bearer") =>
            {
                codex_runtime_proof::launcher::provision_api_bearer()
            }
            _ => Err(anyhow::anyhow!("unsupported fixed launcher mode")),
        }
    };
    #[cfg(target_os = "linux")]
    let exit_code = match result {
        Ok(code) => code,
        Err(_) => {
            eprintln!("runtime-proof launcher failed closed");
            1
        }
    };

    #[cfg(not(target_os = "linux"))]
    let exit_code = {
        eprintln!("runtime-proof launcher requires Linux");
        1
    };

    std::process::exit(exit_code);
}
