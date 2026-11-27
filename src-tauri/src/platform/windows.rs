pub fn check_webview_runtime() -> bool {
    use windows::{
        Win32::System::Registry::{
            HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6432KEY,
            RegGetValueW,
        },
        core::w,
    };
    let subkey =
        w!(r"SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}");
    [
        (HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ | RRF_SUBKEY_WOW6432KEY),
        (HKEY_CURRENT_USER, RRF_RT_REG_SZ),
    ]
    .into_iter()
    .any(|(hive, flags)| {
        let mut value = [0u16; 64];
        let mut size = std::mem::size_of_val(&value) as u32;
        let result = unsafe {
            RegGetValueW(
                hive,
                subkey,
                w!("pv"),
                flags,
                None,
                Some(value.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        result.is_ok()
            && String::from_utf16_lossy(&value)
                .trim_end_matches('\0')
                .split('.')
                .any(|part| part.parse::<u32>().is_ok_and(|number| number > 0))
    })
}

pub fn show_error(message: &str) {
    use windows::{
        Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
        core::{PCWSTR, w},
    };
    let message = message
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            w!("suonelo"),
            MB_OK | MB_ICONERROR,
        );
    }
}
