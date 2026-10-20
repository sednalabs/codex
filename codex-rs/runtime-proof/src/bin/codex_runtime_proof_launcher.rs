fn main() {
    #[cfg(target_os = "linux")]
    let result = {
        let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
        match arguments.as_slice() {
            [] => codex_runtime_proof::launcher::run(),
            [argument] if argument.as_os_str() == std::ffi::OsStr::new("provision-api-bearer") => {
                codex_runtime_proof::launcher::provision_api_bearer()
            }
            [argument, mode]
                if argument.as_os_str() == std::ffi::OsStr::new("provision-api-bearer")
                    && mode.as_os_str() == std::ffi::OsStr::new("--no-browser") =>
            {
                codex_runtime_proof::launcher::provision_api_bearer_with_manual_callback_input()
            }
            _ => Err(anyhow::anyhow!("unsupported fixed launcher mode")),
        }
    };
    #[cfg(target_os = "linux")]
    let exit_code = match result {
        Ok(code) => code,
        Err(error) => {
            if let Some(stage) =
                codex_runtime_proof::launcher::safe_provisioning_failure_stage(&error)
            {
                eprintln!("runtime-proof launcher failed closed (stage: {stage})");
            } else {
                eprintln!("runtime-proof launcher failed closed");
            }
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
