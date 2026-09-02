use super::OwnedHandle;
use super::ProvisioningRequest;
use super::pin_existing_ancestors;
use super::validate_request;
use codex_windows_sandbox::FramedProvisioningMessage;
use codex_windows_sandbox::PROVISIONING_PROTOCOL_VERSION;
use codex_windows_sandbox::ProvisioningMessage;
use codex_windows_sandbox::SandboxProvisioningRequest;
use codex_windows_sandbox::SandboxProvisioningResponse;
use codex_windows_sandbox::WindowsSandboxProvisioningSettings;
use codex_windows_sandbox::WindowsSandboxProxyListeners;
use codex_windows_sandbox::to_wide;
use codex_windows_sandbox::write_provisioning_frame;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use windows_sys::Win32::Foundation as foundation;
use windows_sys::Win32::Storage::FileSystem as filesystem;
use windows_sys::Win32::Storage::Packaging::Appx;
use windows_sys::Win32::System::Pipes as pipes;

fn framed_request(request: SandboxProvisioningRequest) -> Vec<u8> {
    let mut frame = Vec::new();
    write_provisioning_frame(
        &mut frame,
        &FramedProvisioningMessage {
            version: PROVISIONING_PROTOCOL_VERSION,
            message: ProvisioningMessage::ProvisionSandboxRequest { payload: request },
        },
    )
    .unwrap();
    frame
}

#[test]
fn provisioning_request_preserves_home_spaces_and_unicode() {
    let request = framed_request(SandboxProvisioningRequest {
        codex_home: "D:\\Codex Homes\\Jos\u{00e9}\\.codex".to_string(),
        settings: WindowsSandboxProvisioningSettings::default(),
        listeners: WindowsSandboxProxyListeners::default(),
    });
    assert_eq!(
        validate_request(&request).unwrap(),
        ProvisioningRequest {
            codex_home: PathBuf::from("D:\\Codex Homes\\Jos\u{00e9}\\.codex"),
            listeners: WindowsSandboxProxyListeners::default(),
            settings: WindowsSandboxProvisioningSettings::default(),
        }
    );
}

#[test]
fn structured_provisioning_request_carries_normalized_proxy_settings() {
    for (http_port, socks_port, proxy_ports) in
        [(8081, 3128, vec![3128, 8081]), (8081, 8081, vec![8081])]
    {
        let request = framed_request(SandboxProvisioningRequest {
            codex_home: "D:\\Codex Homes\\Jos\u{00e9}\\.codex".to_string(),
            settings: WindowsSandboxProvisioningSettings {
                proxy_ports: vec![http_port, socks_port, http_port],
                allow_local_binding: true,
            },
            listeners: WindowsSandboxProxyListeners {
                http_ports: vec![http_port, http_port],
                socks_ports: vec![socks_port],
            },
        });
        assert_eq!(
            validate_request(&request).unwrap(),
            ProvisioningRequest {
                codex_home: PathBuf::from("D:\\Codex Homes\\Jos\u{00e9}\\.codex"),
                listeners: WindowsSandboxProxyListeners {
                    http_ports: vec![http_port],
                    socks_ports: vec![socks_port],
                },
                settings: WindowsSandboxProvisioningSettings {
                    proxy_ports,
                    allow_local_binding: true,
                },
            },
        );
    }
}

#[test]
fn structured_provisioning_request_accepts_independent_and_additional_proxy_ports() {
    for (http_ports, socks_ports, proxy_ports) in [
        (vec![3128], vec![], vec![3128]),
        (vec![], vec![1080], vec![1080]),
        (vec![3128, 8080], vec![1080], vec![1080, 1082, 3128, 8080]),
    ] {
        let settings = WindowsSandboxProvisioningSettings {
            proxy_ports,
            allow_local_binding: false,
        };
        let listeners = WindowsSandboxProxyListeners {
            http_ports,
            socks_ports,
        };
        let request = framed_request(SandboxProvisioningRequest {
            codex_home: r"C:\Users\alice\.codex".to_string(),
            settings: settings.clone(),
            listeners: listeners.clone(),
        });
        assert_eq!(
            validate_request(&request).unwrap(),
            ProvisioningRequest {
                codex_home: PathBuf::from(r"C:\Users\alice\.codex"),
                settings,
                listeners,
            }
        );
    }
}

#[test]
fn structured_provisioning_request_accepts_disabled_listeners() {
    let request = framed_request(SandboxProvisioningRequest {
        codex_home: r"C:\Users\alice\.codex".to_string(),
        settings: WindowsSandboxProvisioningSettings::default(),
        listeners: WindowsSandboxProxyListeners::default(),
    });
    assert_eq!(
        validate_request(&request).unwrap(),
        ProvisioningRequest {
            codex_home: PathBuf::from(r"C:\Users\alice\.codex"),
            listeners: WindowsSandboxProxyListeners::default(),
            settings: WindowsSandboxProvisioningSettings::default(),
        }
    );
}

#[test]
fn structured_provisioning_request_requires_exact_version_fields_and_framing() {
    let valid = SandboxProvisioningRequest {
        codex_home: r"C:\Users\alice\.codex".to_string(),
        settings: WindowsSandboxProvisioningSettings::default(),
        listeners: WindowsSandboxProxyListeners::default(),
    };
    let mut invalid_version = Vec::new();
    write_provisioning_frame(
        &mut invalid_version,
        &FramedProvisioningMessage {
            version: PROVISIONING_PROTOCOL_VERSION + 1,
            message: ProvisioningMessage::ProvisionSandboxRequest {
                payload: valid.clone(),
            },
        },
    )
    .unwrap();
    assert!(validate_request(&invalid_version).is_err());

    let mut unexpected_message = Vec::new();
    write_provisioning_frame(
        &mut unexpected_message,
        &FramedProvisioningMessage {
            version: PROVISIONING_PROTOCOL_VERSION,
            message: ProvisioningMessage::ProvisionSandboxResponse {
                payload: SandboxProvisioningResponse::Ok,
            },
        },
    )
    .unwrap();
    assert!(validate_request(&unexpected_message).is_err());

    let valid = framed_request(valid);
    assert!(validate_request(&valid[..valid.len() - 1]).is_err());
    let mut duplicate = valid.clone();
    duplicate.extend_from_slice(&valid);
    assert!(validate_request(&duplicate).is_err());
}

#[test]
fn structured_provisioning_request_rejects_invalid_or_inconsistent_ports() {
    for (proxy_ports, http_ports, socks_ports) in [
        (vec![0], vec![], vec![]),
        (vec![3128], vec![0], vec![]),
        (vec![3128], vec![3128], vec![0]),
        (vec![3128], vec![3128], vec![8081]),
    ] {
        let request = framed_request(SandboxProvisioningRequest {
            codex_home: r"C:\Users\alice\.codex".to_string(),
            settings: WindowsSandboxProvisioningSettings {
                proxy_ports,
                allow_local_binding: false,
            },
            listeners: WindowsSandboxProxyListeners {
                http_ports,
                socks_ports,
            },
        });
        assert!(validate_request(&request).is_err());
    }
}

#[test]
fn provisioning_request_rejects_empty_control_characters_and_invalid_utf8() {
    for home in ["", "C:\\safe\0evil", "C:\\safe\rmore", "C:\\safe\nmore"] {
        let request = framed_request(SandboxProvisioningRequest {
            codex_home: home.to_string(),
            settings: WindowsSandboxProvisioningSettings::default(),
            listeners: WindowsSandboxProxyListeners::default(),
        });
        assert!(validate_request(&request).is_err());
    }

    let mut invalid_utf8 = framed_request(SandboxProvisioningRequest {
        codex_home: r"C:\Users\alice\.codex".to_string(),
        settings: WindowsSandboxProvisioningSettings::default(),
        listeners: WindowsSandboxProxyListeners::default(),
    });
    invalid_utf8[std::mem::size_of::<u32>()] = 0xff;
    assert!(validate_request(&invalid_utf8).is_err());
}

#[test]
fn pin_existing_ancestors_accepts_drive_and_verbatim_drive_roots() {
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let verbatim_root = executable.ancestors().last().unwrap();
    let ordinary_root = PathBuf::from(
        verbatim_root
            .to_str()
            .unwrap()
            .strip_prefix(r"\\?\")
            .unwrap(),
    );

    for root in [ordinary_root.as_path(), verbatim_root] {
        let mut handles = Vec::new();
        pin_existing_ancestors(root, &mut handles).unwrap();
        assert_eq!(handles.len(), 1);
    }
}

#[test]
fn unpackaged_pipe_clients_are_rejected_before_sending_a_request() {
    let mut family_length = 0;
    let package_status =
        unsafe { Appx::GetCurrentPackageFamilyName(&mut family_length, ptr::null_mut()) };
    if package_status != foundation::APPMODEL_ERROR_NO_PACKAGE {
        assert_eq!(package_status, foundation::ERROR_INSUFFICIENT_BUFFER);
        return;
    }

    static NEXT_PIPE_INSTANCE: AtomicU64 = AtomicU64::new(0);
    let name = to_wide(format!(
        r"\\.\pipe\OpenAI.CodexSandbox.Tests.{}.{}",
        std::process::id(),
        NEXT_PIPE_INSTANCE.fetch_add(1, Ordering::Relaxed)
    ));
    let server = unsafe {
        pipes::CreateNamedPipeW(
            name.as_ptr(),
            filesystem::PIPE_ACCESS_DUPLEX | filesystem::FILE_FLAG_FIRST_PIPE_INSTANCE,
            pipes::PIPE_TYPE_BYTE
                | pipes::PIPE_READMODE_BYTE
                | pipes::PIPE_WAIT
                | pipes::PIPE_REJECT_REMOTE_CLIENTS,
            1,
            1024,
            1024,
            0,
            ptr::null(),
        )
    };
    assert_ne!(server, foundation::INVALID_HANDLE_VALUE);
    let server = OwnedHandle(server);

    let client = unsafe {
        filesystem::CreateFileW(
            name.as_ptr(),
            foundation::GENERIC_READ | foundation::GENERIC_WRITE,
            0,
            ptr::null(),
            filesystem::OPEN_EXISTING,
            0,
            0,
        )
    };
    assert_ne!(client, foundation::INVALID_HANDLE_VALUE);
    let _client = OwnedHandle(client);

    let connected = unsafe { pipes::ConnectNamedPipe(server.0, ptr::null_mut()) };
    assert!(
        connected != 0 || unsafe { foundation::GetLastError() } == foundation::ERROR_PIPE_CONNECTED
    );

    let error = match crate::package_identity::authorize_client_process(server.0) {
        Ok(_) => panic!("an unpackaged client was authorized without sending a request"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("installed Codex package identity"),
        "unexpected package authorization failure: {error:#}"
    );
}
