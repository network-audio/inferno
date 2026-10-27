//! The notification area (tray) icon: click for the settings page, right-click for Settings and Quit.

use std::sync::OnceLock;

use windows_sys::Win32::{
  Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
  System::LibraryLoader::GetModuleHandleW,
  UI::{
    Shell::{
      ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
      NOTIFYICONDATAW,
    },
    WindowsAndMessaging::{
      AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
      GetCursorPos, GetMessageW, LoadIconW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
      SetForegroundWindow, TrackPopupMenu, TranslateMessage, HWND_MESSAGE, IDI_APPLICATION, MF_SEPARATOR,
      MF_STRING, MSG, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CONTEXTMENU,
      WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW,
    },
  },
};

const WM_TRAY: u32 = WM_APP + 1;
const MENU_SETTINGS: usize = 1;
const MENU_QUIT: usize = 2;

static URL: OnceLock<Vec<u16>> = OnceLock::new();
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

fn wide(text: &str) -> Vec<u16> {
  text.encode_utf16().chain(Some(0)).collect()
}

fn open_settings() {
  if let Some(url) = URL.get() {
    let open = wide("open");
    unsafe {
      ShellExecuteW(
        std::ptr::null_mut(),
        open.as_ptr(),
        url.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        SW_SHOWNORMAL,
      )
    };
  }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
  let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
  data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
  data.hWnd = hwnd;
  data.uID = 1;
  data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
  data.uCallbackMessage = WM_TRAY;
  data.hIcon = unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) };
  for (dst, src) in data.szTip.iter_mut().zip(wide("Inferno Sound Cards")) {
    *dst = src;
  }
  data
}

fn add_icon(hwnd: HWND) {
  let data = icon_data(hwnd);
  unsafe { Shell_NotifyIconW(NIM_ADD, &data) };
}

fn show_menu(hwnd: HWND) {
  unsafe {
    let menu = CreatePopupMenu();
    let (settings, quit) = (wide("Settings…"), wide("Quit"));
    AppendMenuW(menu, MF_STRING, MENU_SETTINGS, settings.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    AppendMenuW(menu, MF_STRING, MENU_QUIT, quit.as_ptr());
    let mut point = POINT { x: 0, y: 0 };
    GetCursorPos(&mut point);
    // the menu closes when clicking elsewhere only if the window is in the foreground
    SetForegroundWindow(hwnd);
    let chosen =
      TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, point.x, point.y, 0, hwnd, std::ptr::null());
    DestroyMenu(menu);
    match chosen as usize {
      MENU_SETTINGS => open_settings(),
      MENU_QUIT => PostQuitMessage(0),
      _ => {}
    }
  }
}

unsafe extern "system" fn window_proc(
  hwnd: HWND,
  message: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  if message == WM_TRAY {
    match lparam as u32 {
      WM_LBUTTONUP => open_settings(),
      WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
      _ => {}
    }
    return 0;
  }
  if Some(&message) == TASKBAR_CREATED.get() {
    // Explorer restarted: the icon is gone
    add_icon(hwnd);
    return 0;
  }
  DefWindowProcW(hwnd, message, wparam, lparam)
}

/// Shows the icon until Quit is chosen.
pub fn run(settings_url: &str) {
  URL.get_or_init(|| wide(settings_url));
  unsafe {
    TASKBAR_CREATED.get_or_init(|| RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()));
    let instance = GetModuleHandleW(std::ptr::null());
    let class_name = wide("InfernoSoundCardTray");
    let mut class: WNDCLASSW = std::mem::zeroed();
    class.lpfnWndProc = Some(window_proc);
    class.hInstance = instance;
    class.lpszClassName = class_name.as_ptr();
    RegisterClassW(&class);
    let title = wide("Inferno Sound Cards");
    let hwnd = CreateWindowExW(
      0,
      class_name.as_ptr(),
      title.as_ptr(),
      0,
      0,
      0,
      0,
      0,
      HWND_MESSAGE,
      std::ptr::null_mut(),
      instance,
      std::ptr::null(),
    );
    add_icon(hwnd);
    let mut message: MSG = std::mem::zeroed();
    while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
      TranslateMessage(&message);
      DispatchMessageW(&message);
    }
    let data = icon_data(hwnd);
    Shell_NotifyIconW(NIM_DELETE, &data);
  }
}
