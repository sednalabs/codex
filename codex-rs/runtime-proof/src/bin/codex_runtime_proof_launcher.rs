fn main() {
    #[cfg(target_os = "linux")]
    let result = {
        let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
        match arguments.as_slice() {
            [] => codex_runtime_proof::launcher::run(),
            [argument] if argument.as_os_str() == std::ffi::OsStr::new("provision-api-bearer") => {
                codex_runtime_proof::launcher::provision_api_bearer()
            }
            [argument] if argument.as_os_str() == std::ffi::OsStr::new("logout-api-profile") => {
                codex_runtime_proof::launcher::logout_api_profile()
            }
            [argument, mode]
                if argument.as_os_str() == std::ffi::OsStr::new("provision-api-bearer")
                    && mode.as_os_str() == std::ffi::OsStr::new("--no-browser") =>
            {
                codex_runtime_proof::launcher::provision_api_bearer_with_manual_callback_input()
            }
            [argument, mode]
                if argument.as_os_str()
                    == std::ffi::OsStr::new("__runtime-proof-api-profile-child")
                    && mode.as_os_str() == std::ffi::OsStr::new("browser") =>
            {
                codex_runtime_proof::launcher::run_api_profile_child(
                    /*use_manual_callback_input*/ false,
                )
            }
            [argument, mode]
                if argument.as_os_str()
                    == std::ffi::OsStr::new("__runtime-proof-api-profile-child")
                    && mode.as_os_str() == std::ffi::OsStr::new("callback-input") =>
            {
                codex_runtime_proof::launcher::run_api_profile_child(
                    /*use_manual_callback_input*/ true,
                )
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
                if let Some(detail) =
                    codex_runtime_proof::launcher::safe_provisioning_failure_detail(&error)
                {
                    eprintln!(
                        "runtime-proof launcher failed closed (stage: {stage}; detail: {detail})"
                    );
                } else {
                    eprintln!("runtime-proof launcher failed closed (stage: {stage})");
                }
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
