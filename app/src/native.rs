#[cfg(not(windows))]
use std::path::Path;

#[cfg(windows)]
mod windows {
    use std::cell::RefCell;
    use std::os::windows::ffi::OsStrExt;
    use std::{ffi::OsStr, path::Path, ptr};

    use gimage_core::{ExportFormat, RgbaImage};
    use windows_sys::Win32::{
        Foundation::{GlobalFree, HWND},
        Graphics::Gdi::*,
        Storage::{
            FileSystem::DELETE,
            Xps::{AbortDoc, DOCINFOW, EndDoc, EndPage, StartDocW, StartPage},
        },
        System::{
            Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
            Registry::{
                HKEY_CURRENT_USER, KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_NONE,
                REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW,
                RegDeleteKeyW, RegDeleteTreeW, RegOpenKeyExW, RegSetKeyValueW,
            },
            SystemServices::SFGAO_FILESYSTEM,
        },
        UI::{
            Controls::Dialogs::*,
            Shell::{
                OAIF_EXEC, OAIF_HIDE_REGISTRATION, OPENASINFO, SHCNE_ASSOCCHANGED, SHCNF_IDLIST,
                SHChangeNotify, SHOpenWithDialog, ShellExecuteW,
            },
            WindowsAndMessaging::{
                GetForegroundWindow, SPI_SETDESKWALLPAPER, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE,
                SystemParametersInfoW,
            },
        },
    };

    fn wide(text: &OsStr) -> Vec<u16> {
        text.encode_wide().chain(Some(0)).collect()
    }

    pub struct UiApartment(std::marker::PhantomData<std::rc::Rc<()>>);

    impl UiApartment {
        pub fn new() -> anyhow::Result<Self> {
            let result = unsafe { CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32) };
            anyhow::ensure!(result >= 0, "无法初始化 Windows 窗口线程（{result:#x}）");
            Ok(Self(std::marker::PhantomData))
        }
    }

    impl Drop for UiApartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    pub fn owner() -> usize {
        unsafe { GetForegroundWindow() as usize }
    }

    pub fn rename_without_replace(from: &Path, to: &Path) -> std::io::Result<()> {
        if to.canonicalize().ok().as_deref() == Some(from) {
            return std::fs::rename(from, to);
        }
        let source = wide(from.as_os_str());
        let destination = wide(to.as_os_str());
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileW(
                source.as_ptr(),
                destination.as_ptr(),
            )
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn file_associations() -> Result<(), String> {
        const CAPABILITIES: &str = r"Software\GImage\Capabilities";
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let exe = exe
            .to_str()
            .ok_or_else(|| "无法注册当前程序路径".to_string())?;
        let executable = Path::new(exe)
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| "无法取得程序文件名".to_string())?;
        let command = format!("\"{exe}\" \"%1\"");
        let icon = format!("\"{exe}\",0");
        let classes = format!(r"Software\Classes\Applications\{executable}");
        registry(
            &format!(r"{classes}\shell\open\command"),
            None,
            Some(&command),
        )?;
        registry(&classes, Some("FriendlyAppName"), Some("GImage"))?;
        registry(CAPABILITIES, Some("ApplicationName"), Some("GImage"))?;
        registry(
            CAPABILITIES,
            Some("ApplicationDescription"),
            Some("浏览和预览图片"),
        )?;
        registry(CAPABILITIES, Some("ApplicationIcon"), Some(&icon))?;
        for extension in gimage_core::SCANNABLE_EXTENSIONS {
            let extension = format!(".{extension}");
            let progid = format!("GImage{extension}");
            let key = format!(r"Software\Classes\{progid}");
            registry(&key, None, Some(&format!("GImage {extension}")))?;
            registry(&format!(r"{key}\DefaultIcon"), None, Some(&icon))?;
            registry(&format!(r"{key}\shell\open\command"), None, Some(&command))?;
            registry(
                &format!(r"{CAPABILITIES}\FileAssociations"),
                Some(&extension),
                Some(&progid),
            )?;
            registry(
                &format!(r"Software\Classes\{extension}\OpenWithProgids"),
                Some(&progid),
                None,
            )?;
            registry(
                &format!(r"{classes}\SupportedTypes"),
                Some(&extension),
                Some(""),
            )?;
        }
        registry(
            r"Software\RegisteredApplications",
            Some("GImage"),
            Some(CAPABILITIES),
        )?;
        let settings = wide(OsStr::new(
            "ms-settings:defaultapps?registeredAppUser=GImage",
        ));
        let result = unsafe {
            SHChangeNotify(
                SHCNE_ASSOCCHANGED as i32,
                SHCNF_IDLIST,
                ptr::null(),
                ptr::null(),
            );
            ShellExecuteW(
                owner() as HWND,
                ptr::null(),
                settings.as_ptr(),
                ptr::null(),
                ptr::null(),
                1,
            )
        } as isize;
        if result <= 32 {
            return Err(format!(
                "已注册 GImage，无法打开 Windows 默认应用设置（{result}）"
            ));
        }
        Ok(())
    }

    pub fn explorer_menu(enabled: bool, positions: [u8; 3]) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let exe = exe.to_str().ok_or("无法读取程序路径")?;
        let commands = [
            ("01Browse", "浏览图片", "--gimage-browse"),
            ("02Convert", "转换图片", "--gimage-convert"),
            ("03Rename", "批量重命名", "--gimage-rename"),
        ];
        const SUBCOMMANDS: &str = "GImage.ContextCommands";
        let icon = format!("\"{exe}\",0");
        let positions = if enabled { positions } else { [2; 3] };
        let has_subcommands = positions.contains(&1);
        let extensions = gimage_core::SCANNABLE_EXTENSIONS.iter()
            .map(|extension| format!(r"SystemFileAssociations\.{extension}"));

        for (index, (id, label, argument)) in commands.iter().enumerate() {
            let nested = format!(r"Software\Classes\{SUBCOMMANDS}\shell\{id}");
            if positions[index] == 1 {
                register_menu_command(&nested, label, exe, argument, None)?;
            } else {
                delete_registry_key(&nested)?;
            }
        }
        for class in extensions.chain(["Folder".to_owned()]) {
            let key = format!(r"Software\Classes\{class}\shell");
            let old_group = format!(r"{key}\GImage.Tools");
            if let Err(error) = delete_registry_key(&old_group) {
                tracing::warn!(%error, "could not remove legacy Explorer submenu");
                registry(&old_group, Some("LegacyDisable"), Some(""))?;
            }
            for (index, (id, label, argument)) in commands.iter().enumerate() {
                let primary = format!(r"{key}\GImage.{id}");
                if positions[index] == 0 {
                    register_menu_command(&primary, label, exe, argument, Some(&icon))?;
                    if class == "Folder" {
                        restrict_to_filesystem_folder(&primary)?;
                    }
                } else {
                    delete_registry_key(&primary)?;
                }
            }
            let group = format!(r"{key}\GImage.ToolsMenu");
            if has_subcommands {
                registry(&group, Some("MUIVerb"), Some("GImage 工具"))?;
                registry(&group, Some("Icon"), Some(&icon))?;
                registry(&group, Some("MultiSelectModel"), Some("Single"))?;
                registry(&group, Some("ExtendedSubCommandsKey"), Some(SUBCOMMANDS))?;
                if class == "Folder" {
                    restrict_to_filesystem_folder(&group)?;
                }
            } else {
                delete_registry_key(&group)?;
            }
        }
        // A machine policy may make the old Directory branch read-only.
        // Treat cleanup there as migration, not a prerequisite for registration.
        for id in ["01Browse", "02Convert", "03Rename", "Tools", "ToolsMenu"] {
            let key = format!(r"Software\Classes\Directory\shell\GImage.{id}");
            if let Err(error) = delete_registry_key(&key) {
                tracing::warn!(%error, "could not remove legacy Explorer folder entry");
            }
        }
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, ptr::null(), ptr::null()); }
        Ok(())
    }

    fn restrict_to_filesystem_folder(key: &str) -> Result<(), String> {
        registry_value(key, Some("AttributeMask"), REG_DWORD, &SFGAO_FILESYSTEM.to_le_bytes())?;
        registry_value(key, Some("AttributeValue"), REG_DWORD, &SFGAO_FILESYSTEM.to_le_bytes())?;
        registry_value(key, Some("ImpliedSelectionModel"), REG_DWORD, &0u32.to_le_bytes())
    }

    fn register_menu_command(
        key: &str, label: &str, exe: &str, argument: &str, icon: Option<&str>,
    ) -> Result<(), String> {
        registry(key, Some("MUIVerb"), Some(label))?;
        registry(key, Some("MultiSelectModel"), Some("Single"))?;
        if let Some(icon) = icon {
            registry(key, Some("Icon"), Some(icon))?;
        }
        registry(&format!(r"{key}\command"), None, Some(&format!("\"{exe}\" {argument} \"%1\"")))
    }

    fn delete_registry_key(key: &str) -> Result<(), String> {
        let path = wide(OsStr::new(key));
        let mut handle = ptr::null_mut();
        let code = unsafe {
            RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0,
                DELETE | KEY_ENUMERATE_SUB_KEYS | KEY_QUERY_VALUE | KEY_SET_VALUE, &mut handle)
        };
        match code {
            2 | 3 => return Ok(()),
            0 => {}
            _ => return Err(registry_error(key, code)),
        }
        let code = unsafe {
            let code = RegDeleteTreeW(handle, ptr::null());
            RegCloseKey(handle);
            if code == 0 { RegDeleteKeyW(HKEY_CURRENT_USER, path.as_ptr()) } else { code }
        };
        if matches!(code, 0 | 2 | 3) { Ok(()) } else { Err(registry_error(key, code)) }
    }

    fn registry(key: &str, name: Option<&str>, value: Option<&str>) -> Result<(), String> {
        if let Some(value) = value {
            let data: Vec<_> = value.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
            registry_value(key, name, REG_SZ, &data)
        } else {
            registry_value(key, name, REG_NONE, &[])
        }
    }

    fn registry_value(key: &str, name: Option<&str>, kind: u32, data: &[u8]) -> Result<(), String> {
        let path = wide(OsStr::new(key));
        let mut handle = ptr::null_mut();
        let code = unsafe {
            RegCreateKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, ptr::null(),
                REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, ptr::null(), &mut handle, ptr::null_mut())
        };
        if code != 0 {
            return Err(registry_error(key, code));
        }
        let name = name.map(|name| wide(OsStr::new(name)));
        let code = unsafe {
            let code = RegSetKeyValueW(
                handle,
                ptr::null(),
                name.as_ref().map_or(ptr::null(), |name| name.as_ptr()),
                kind,
                if data.is_empty() { ptr::null() } else { data.as_ptr().cast() },
                data.len() as u32,
            );
            RegCloseKey(handle);
            code
        };
        if code == 0 {
            Ok(())
        } else {
            Err(registry_error(key, code))
        }
    }

    fn registry_error(key: &str, code: u32) -> String {
        format!(r"HKCU\{key}：{}", std::io::Error::from_raw_os_error(code as i32))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::System::Registry::{RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW};

        const VERBS: [&str; 5] = ["01Browse", "02Convert", "03Rename", "Tools", "ToolsMenu"];
        const SUBCOMMANDS: &str = r"Software\Classes\GImage.ContextCommands\shell";

        fn key_exists(key: &str) -> bool {
            let path = wide(OsStr::new(key));
            let mut handle = ptr::null_mut();
            match unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_QUERY_VALUE, &mut handle) } {
                0 => {
                    unsafe { RegCloseKey(handle); }
                    true
                }
                2 | 3 => false,
                code => panic!("{}", registry_error(key, code)),
            }
        }

        fn read_value(key: &str, name: Option<&str>, flags: u32, out: &mut [u8]) -> Result<usize, u32> {
            let path = wide(OsStr::new(key));
            let value = name.map(|name| wide(OsStr::new(name)));
            let mut bytes = out.len() as u32;
            let code = unsafe {
                RegGetValueW(HKEY_CURRENT_USER, path.as_ptr(),
                    value.as_ref().map_or(ptr::null(), |name| name.as_ptr()), flags,
                    ptr::null_mut(), out.as_mut_ptr().cast(), &mut bytes)
            };
            if code == 0 { Ok(bytes as usize) } else { Err(code) }
        }

        fn read_string(key: &str, name: Option<&str>) -> String {
            let mut buffer = [0; 4096];
            let count = read_value(key, name, RRF_RT_REG_SZ, &mut buffer)
                .unwrap_or_else(|code| panic!("{}", registry_error(key, code)));
            let units: Vec<_> = buffer[..count].chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .take_while(|&unit| unit != 0).collect();
            String::from_utf16(&units).unwrap()
        }

        fn assert_unregistered() {
            let extensions = gimage_core::SCANNABLE_EXTENSIONS.iter()
                .map(|extension| format!(r"SystemFileAssociations\.{extension}"));
            for class in extensions.chain(["Folder".into(), "Directory".into()]) {
                for id in VERBS {
                    let key = format!(r"Software\Classes\{class}\shell\GImage.{id}");
                    assert!(!key_exists(&key), "existing menu at {key}");
                }
            }
            for id in &VERBS[..3] {
                let key = format!(r"{SUBCOMMANDS}\{id}");
                assert!(!key_exists(&key), "existing command at {key}");
            }
        }

        struct RegistryCleanup;

        impl Drop for RegistryCleanup {
            fn drop(&mut self) {
                if let Err(error) = explorer_menu(false, [2; 3]) {
                    eprintln!("Explorer menu cleanup failed: {error}");
                }
            }
        }

        #[test]
        #[ignore = "writes and removes GImage's current-user Explorer menu keys"]
        fn explorer_menu_registry_round_trip() {
            assert_unregistered();
            let _cleanup = RegistryCleanup;
            let folder = r"Software\Classes\Folder\shell";
            let image = r"Software\Classes\SystemFileAssociations\.jpg\shell";

            explorer_menu(true, [0, 0, 1]).unwrap();
            for parent in [folder, image] {
                let group = format!(r"{parent}\GImage.ToolsMenu");
                assert_eq!(read_string(&group, Some("MUIVerb")), "GImage 工具");
                assert_eq!(read_string(&group, Some("ExtendedSubCommandsKey")), "GImage.ContextCommands");
                assert!(read_string(&group, Some("Icon")).ends_with(",0"));
                let browse = format!(r"{parent}\GImage.01Browse\command");
                assert!(read_string(&browse, None).ends_with(" --gimage-browse \"%1\""));
                let convert = format!(r"{parent}\GImage.02Convert\command");
                assert!(read_string(&convert, None).ends_with(" --gimage-convert \"%1\""));
            }
            let group = format!(r"{folder}\GImage.ToolsMenu");
            let mut number = [0; 4];
            assert_eq!(read_value(&group, Some("AttributeMask"), RRF_RT_REG_DWORD, &mut number), Ok(4));
            assert_eq!(u32::from_le_bytes(number), SFGAO_FILESYSTEM);
            let rename = format!(r"{SUBCOMMANDS}\03Rename");
            assert_eq!(read_string(&rename, Some("MUIVerb")), "批量重命名");
            assert!(read_string(&format!(r"{rename}\command"), None).ends_with(" --gimage-rename \"%1\""));
            assert_eq!(read_value(&rename, Some("Icon"), RRF_RT_REG_SZ, &mut [0; 4096]), Err(2));
            println!("Default configuration: read back folder and JPEG menus, commands, and folder attributes");

            explorer_menu(true, [1, 2, 0]).unwrap();
            for parent in [folder, image] {
                assert!(!key_exists(&format!(r"{parent}\GImage.01Browse")));
                assert!(!key_exists(&format!(r"{parent}\GImage.02Convert")));
                assert!(key_exists(&format!(r"{parent}\GImage.03Rename\command")));
            }
            assert!(!key_exists(&rename));
            assert!(key_exists(&format!(r"{SUBCOMMANDS}\01Browse\command")));
            println!("Changed configuration: moved Browse into the submenu, hid Convert, and promoted Rename");

            explorer_menu(false, [2; 3]).unwrap();
            assert_unregistered();
            println!("Disabled: all registered GImage menu leaves removed");
        }
    }

    thread_local! {
        static SWATCHES: RefCell<[u32; 16]> = const { RefCell::new([0x00ff_ffff; 16]) };
    }

    pub fn choose_color(initial: slint::Color) -> Option<slint::Color> {
        SWATCHES.with(|swatches| {
            let mut swatches = swatches.borrow_mut();
            let mut dialog = CHOOSECOLORW {
                lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
                hwndOwner: owner() as HWND,
                rgbResult: u32::from_le_bytes([initial.red(), initial.green(), initial.blue(), 0]),
                lpCustColors: swatches.as_mut_ptr(),
                Flags: CC_RGBINIT | CC_FULLOPEN,
                ..Default::default()
            };
            (unsafe { ChooseColorW(&mut dialog) } != 0).then(|| {
                let [r, g, b, _] = dialog.rgbResult.to_le_bytes();
                slint::Color::from_rgb_u8(r, g, b)
            })
        })
    }

    pub fn open_with(path: &Path) -> Result<(), String> {
        let path = wide(OsStr::new(&crate::util::display_path(path)));
        let info = OPENASINFO {
            pcszFile: path.as_ptr(),
            pcszClass: ptr::null(),
            oaifInFlags: OAIF_EXEC | OAIF_HIDE_REGISTRATION,
        };
        let result = unsafe {
            let init = CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
            let result = SHOpenWithDialog(owner() as HWND, &info);
            if init >= 0 {
                CoUninitialize();
            }
            result
        };
        if result >= 0 || result as u32 == 0x8007_04c7 {
            Ok(())
        } else {
            Err(format!("无法打开应用选择器（{result:#x}）"))
        }
    }

    pub fn wallpaper(core: &crate::core::AppCore, image: &RgbaImage) -> Result<bool, String> {
        let path = core.data_dir.join("wallpaper.bmp");
        gimage_engine::write_atomic(core.engine.as_ref(), image, ExportFormat::Bmp, 100, &path)
            .map_err(|error| error.user_message())?;
        let path = wide(OsStr::new(&crate::util::display_path(&path)));
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_SETDESKWALLPAPER,
                0,
                path.as_ptr().cast_mut().cast(),
                SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
            )
        };
        if ok != 0 {
            Ok(true)
        } else {
            Err(format!(
                "无法设置桌面背景：{}",
                std::io::Error::last_os_error()
            ))
        }
    }

    pub fn print(image: &RgbaImage, title: &str, owner: usize) -> Result<bool, String> {
        let mut dialog = PRINTDLGW {
            lStructSize: std::mem::size_of::<PRINTDLGW>() as u32,
            hwndOwner: owner as HWND,
            Flags: PD_RETURNDC | PD_NOSELECTION | PD_NOPAGENUMS | PD_USEDEVMODECOPIESANDCOLLATE,
            nCopies: 1,
            ..Default::default()
        };
        let accepted = unsafe { PrintDlgW(&mut dialog) } != 0;
        let error = if accepted {
            0
        } else {
            unsafe { CommDlgExtendedError() }
        };
        for handle in [dialog.hDevMode, dialog.hDevNames] {
            if !handle.is_null() {
                unsafe { GlobalFree(handle) };
            }
        }
        let dc = dialog.hDC;
        if !accepted || dc.is_null() {
            if !dc.is_null() {
                unsafe { DeleteDC(dc) };
            }
            return if error == 0 {
                Ok(false)
            } else {
                Err(format!("无法打开打印对话框（{error:#x}）"))
            };
        }
        let result = print_page(dc, image, title);
        unsafe { DeleteDC(dc) };
        result.map(|()| true)
    }

    fn print_page(dc: HDC, image: &RgbaImage, title: &str) -> Result<(), String> {
        let title = wide(OsStr::new(title));
        let doc = DOCINFOW {
            cbSize: std::mem::size_of::<DOCINFOW>() as i32,
            lpszDocName: title.as_ptr(),
            ..Default::default()
        };
        let (pw, ph) = unsafe {
            (
                GetDeviceCaps(dc, HORZRES as i32),
                GetDeviceCaps(dc, VERTRES as i32),
            )
        };
        if pw <= 0 || ph <= 0 || image.is_empty() {
            return Err("无法取得打印页面尺寸".into());
        }
        let factor = (pw as f64 / image.width as f64).min(ph as f64 / image.height as f64);
        let (w, h) = (
            (image.width as f64 * factor) as i32,
            (image.height as f64 * factor) as i32,
        );
        let mut pixels = image.pixels.clone();
        for p in pixels.chunks_exact_mut(4) {
            let (r, g, b, a) = (p[0], p[1], p[2], u16::from(p[3]));
            let over_white = |c| ((u16::from(c) * a + 255 * (255 - a)) / 255) as u8;
            p.copy_from_slice(&[over_white(b), over_white(g), over_white(r), 0]);
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: image.width as i32,
                biHeight: -(image.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            if StartDocW(dc, &doc) <= 0 {
                return Err("无法启动打印任务".into());
            }
            if StartPage(dc) > 0 {
                SetStretchBltMode(dc, HALFTONE);
                if StretchDIBits(
                    dc,
                    (pw - w) / 2,
                    (ph - h) / 2,
                    w.max(1),
                    h.max(1),
                    0,
                    0,
                    image.width as i32,
                    image.height as i32,
                    pixels.as_ptr().cast(),
                    &info,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                ) > 0
                    && EndPage(dc) > 0
                    && EndDoc(dc) > 0
                {
                    return Ok(());
                }
            }
            AbortDoc(dc);
        }
        Err("打印图片失败".into())
    }
}

#[cfg(windows)]
pub use windows::*;

#[cfg(not(windows))]
pub fn owner() -> usize {
    0
}

#[cfg(not(windows))]
pub fn rename_without_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.try_exists()? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "目标文件已存在",
        ));
    }
    std::fs::rename(from, to)
}

#[cfg(not(windows))]
pub fn choose_color(_: slint::Color) -> Option<slint::Color> {
    None
}

#[cfg(not(windows))]
pub fn open_with(path: &Path) -> Result<(), String> {
    crate::util::open_folder_in_shell(path);
    Ok(())
}

#[cfg(not(windows))]
pub fn file_associations() -> Result<(), String> {
    Err("文件关联设置仅支持 Windows".into())
}

#[cfg(not(windows))]
pub fn explorer_menu(_: bool, _: [u8; 3]) -> Result<(), String> {
    Err("资源管理器右键菜单仅支持 Windows".into())
}

#[cfg(not(windows))]
pub fn wallpaper(_: &crate::core::AppCore, _: &gimage_core::RgbaImage) -> Result<bool, String> {
    Err("当前系统不支持设置桌面背景".into())
}

#[cfg(not(windows))]
pub fn print(_: &gimage_core::RgbaImage, _: &str, _: usize) -> Result<bool, String> {
    Err("当前系统不支持打印".into())
}
